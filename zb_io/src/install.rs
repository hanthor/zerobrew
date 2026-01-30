use std::collections::BTreeMap;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;

use crate::api::ApiClient;
use crate::blob::BlobCache;
use crate::db::Database;
use crate::download::{
    DownloadProgressCallback, DownloadRequest, DownloadResult, ParallelDownloader,
};
use crate::link::{LinkedFile, Linker};
use crate::materialize::Cellar;
use crate::progress::{InstallProgress, ProgressCallback};
use crate::store::Store;
use crate::tap::TapManager;

use zb_core::{Cask, Error, Formula, SelectedBottle, resolve_closure, select_bottle};

/// Maximum number of retries for corrupted downloads
const MAX_CORRUPTION_RETRIES: usize = 3;

pub struct Installer {
    api_client: ApiClient,
    downloader: ParallelDownloader,
    pub store: Store,
    pub cellar: Cellar,
    pub linker: Linker,
    pub db: Database,
    pub tap_manager: TapManager,
}

pub struct InstallPlan {
    pub formulas: Vec<Formula>,
    pub bottles: Vec<SelectedBottle>,
}

pub struct ExecuteResult {
    pub installed: usize,
}

/// Internal struct for tracking processed packages during streaming install
#[derive(Clone)]
struct ProcessedPackage {
    name: String,
    version: String,
    store_key: String,
    linked_files: Vec<LinkedFile>,
}

impl Installer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_client: ApiClient,
        blob_cache: BlobCache,
        store: Store,
        cellar: Cellar,
        linker: Linker,
        db: Database,
        tap_manager: TapManager,
        download_concurrency: usize,
    ) -> Self {
        Self {
            api_client,
            downloader: ParallelDownloader::new(blob_cache, download_concurrency),
            store,
            cellar,
            linker,
            db,
            tap_manager,
        }
    }

    /// Resolve dependencies and plan the install
    pub async fn plan(&self, name: &str) -> Result<InstallPlan, Error> {
        // Recursively fetch all formulas we need
        let formulas = self.fetch_all_formulas(name).await?;

        // Resolve in topological order
        let ordered = resolve_closure(name, &formulas)?;

        // Build list of formulas in order
        let all_formulas: Vec<Formula> = ordered
            .iter()
            .map(|n| formulas.get(n).cloned().unwrap())
            .collect();

        // Select bottles for each formula
        let mut bottles = Vec::new();
        for formula in &all_formulas {
            let bottle = select_bottle(formula)?;
            bottles.push(bottle);
        }

        Ok(InstallPlan {
            formulas: all_formulas,
            bottles,
        })
    }

    /// Try to extract a download, with automatic retry on corruption
    async fn extract_with_retry(
        &self,
        download: &DownloadResult,
        formula: &Formula,
        bottle: &SelectedBottle,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<std::path::PathBuf, Error> {
        let mut blob_path = download.blob_path.clone();
        let mut last_error = None;

        for attempt in 0..MAX_CORRUPTION_RETRIES {
            match self.store.ensure_entry(&bottle.sha256, &blob_path) {
                Ok(entry) => return Ok(entry),
                Err(Error::StoreCorruption { message }) => {
                    // Remove the corrupted blob
                    self.downloader.remove_blob(&bottle.sha256);

                    if attempt + 1 < MAX_CORRUPTION_RETRIES {
                        // Log retry attempt
                        eprintln!(
                            "    Corrupted download detected for {}, retrying ({}/{})...",
                            formula.name,
                            attempt + 2,
                            MAX_CORRUPTION_RETRIES
                        );

                        // Re-download
                        let request = DownloadRequest {
                            url: bottle.url.clone(),
                            sha256: bottle.sha256.clone(),
                            name: formula.name.clone(),
                        };

                        match self
                            .downloader
                            .download_single(request, progress.clone())
                            .await
                        {
                            Ok(new_path) => {
                                blob_path = new_path;
                                // Continue to next iteration to retry extraction
                            }
                            Err(e) => {
                                last_error = Some(e);
                                break;
                            }
                        }
                    } else {
                        last_error = Some(Error::StoreCorruption {
                            message: format!(
                                "{message}\n\nFailed after {MAX_CORRUPTION_RETRIES} attempts. The download may be corrupted at the source."
                            ),
                        });
                    }
                }
                Err(e) => {
                    last_error = Some(e);
                    break;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::StoreCorruption {
            message: "extraction failed with unknown error".to_string(),
        }))
    }

    /// Recursively fetch a formula and all its dependencies in parallel batches
    async fn fetch_all_formulas(&self, name: &str) -> Result<BTreeMap<String, Formula>, Error> {
        use std::collections::HashSet;

        let mut formulas = BTreeMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        let mut to_fetch: Vec<String> = vec![name.to_string()];

        while !to_fetch.is_empty() {
            // Fetch current batch in parallel
            let batch: Vec<String> = to_fetch
                .drain(..)
                .filter(|n| !fetched.contains(n))
                .collect();

            if batch.is_empty() {
                break;
            }

            // Mark as fetched before starting (to avoid re-queueing)
            for n in &batch {
                fetched.insert(n.clone());
            }

            // Fetch all in parallel
            let futures: Vec<_> = batch
                .iter()
                .map(|n| self.api_client.get_formula(n))
                .collect();

            let results = futures::future::join_all(futures).await;

            // Process results and queue new dependencies
            for (i, result) in results.into_iter().enumerate() {
                let formula = result?;

                // Queue dependencies for next batch
                for dep in &formula.dependencies {
                    if !fetched.contains(dep) && !to_fetch.contains(dep) {
                        to_fetch.push(dep.clone());
                    }
                }

                formulas.insert(batch[i].clone(), formula);
            }
        }

        Ok(formulas)
    }

    /// Execute the install plan
    pub async fn execute(&mut self, plan: InstallPlan, link: bool) -> Result<ExecuteResult, Error> {
        self.execute_with_progress(plan, link, None).await
    }

    /// Execute the install plan with progress callback
    /// Uses streaming extraction - starts extracting each package as soon as its download completes
    pub async fn execute_with_progress(
        &mut self,
        plan: InstallPlan,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<ExecuteResult, Error> {
        let report = |event: InstallProgress| {
            if let Some(ref cb) = progress {
                cb(event);
            }
        };

        // Pair formulas with bottles
        let to_install: Vec<(Formula, SelectedBottle)> = plan
            .formulas
            .into_iter()
            .zip(plan.bottles.into_iter())
            .collect();

        if to_install.is_empty() {
            return Ok(ExecuteResult { installed: 0 });
        }

        // Download all bottles
        let requests: Vec<DownloadRequest> = to_install
            .iter()
            .map(|(f, b)| DownloadRequest {
                url: b.url.clone(),
                sha256: b.sha256.clone(),
                name: f.name.clone(),
            })
            .collect();

        // Convert progress callback for download
        let download_progress: Option<DownloadProgressCallback> = progress.clone().map(|cb| {
            Arc::new(move |event: InstallProgress| {
                cb(event);
            }) as DownloadProgressCallback
        });

        // Use streaming downloads - process each as it completes
        let mut rx = self
            .downloader
            .download_streaming(requests, download_progress.clone());

        // Track results by index to maintain install order for database records
        let total = to_install.len();
        let mut completed: Vec<Option<ProcessedPackage>> = vec![None; total];
        let mut error: Option<Error> = None;

        // Process downloads as they complete
        while let Some(result) = rx.recv().await {
            match result {
                Ok(download) => {
                    let idx = download.index;
                    let (formula, bottle) = &to_install[idx];

                    report(InstallProgress::UnpackStarted {
                        name: formula.name.clone(),
                    });

                    // Try extraction with retry logic for corrupted downloads
                    let store_entry = match self
                        .extract_with_retry(&download, formula, bottle, download_progress.clone())
                        .await
                    {
                        Ok(entry) => entry,
                        Err(e) => {
                            error = Some(e);
                            continue;
                        }
                    };

                    // Materialize to cellar
                    // Use effective_version() which includes rebuild suffix if applicable
                    let keg_path = match self.cellar.materialize(
                        &formula.name,
                        &formula.effective_version(),
                        &store_entry,
                    ) {
                        Ok(path) => path,
                        Err(e) => {
                            error = Some(e);
                            continue;
                        }
                    };

                    report(InstallProgress::UnpackCompleted {
                        name: formula.name.clone(),
                    });

                    // Link executables if requested
                    let linked_files = if link {
                        report(InstallProgress::LinkStarted {
                            name: formula.name.clone(),
                        });
                        match self.linker.link_keg(&keg_path) {
                            Ok(files) => {
                                report(InstallProgress::LinkCompleted {
                                    name: formula.name.clone(),
                                });
                                files
                            }
                            Err(e) => {
                                error = Some(e);
                                continue;
                            }
                        }
                    } else {
                        Vec::new()
                    };

                    // Report installation completed for this package
                    report(InstallProgress::InstallCompleted {
                        name: formula.name.clone(),
                    });

                    completed[idx] = Some(ProcessedPackage {
                        name: formula.name.clone(),
                        version: formula.effective_version(),
                        store_key: bottle.sha256.clone(),
                        linked_files,
                    });
                }
                Err(e) => {
                    error = Some(e);
                }
            }
        }

        // Return error if any download failed
        if let Some(e) = error {
            return Err(e);
        }

        // Record all successful installs in database (in order)
        for processed in completed.into_iter().flatten() {
            let tx = self.db.transaction()?;
            tx.record_install(&processed.name, &processed.version, &processed.store_key)?;

            for linked in &processed.linked_files {
                tx.record_linked_file(
                    &processed.name,
                    &processed.version,
                    &linked.link_path.to_string_lossy(),
                    &linked.target_path.to_string_lossy(),
                )?;
            }

            tx.commit()?;
        }

        Ok(ExecuteResult {
            installed: to_install.len(),
        })
    }

    /// Convenience method to plan and execute in one call
    pub async fn install(&mut self, name: &str, link: bool) -> Result<ExecuteResult, Error> {
        let plan = self.plan(name).await?;
        self.execute(plan, link).await
    }

    /// Uninstall a formula
    pub fn uninstall(&mut self, name: &str) -> Result<(), Error> {
        // Check if installed
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;

        // Unlink executables
        let keg_path = self.cellar.keg_path(name, &installed.version);
        self.linker.unlink_keg(&keg_path)?;

        // Remove from database (decrements store ref)
        {
            let tx = self.db.transaction()?;
            tx.record_uninstall(name)?;
            tx.commit()?;
        }

        // Remove cellar entry
        self.cellar.remove_keg(name, &installed.version)?;

        Ok(())
    }

    /// Garbage collect unreferenced store entries
    pub fn gc(&mut self) -> Result<Vec<String>, Error> {
        let unreferenced = self.db.get_unreferenced_store_keys()?;
        let mut removed = Vec::new();

        for store_key in unreferenced {
            self.store.remove_entry(&store_key)?;
            removed.push(store_key);
        }

        Ok(removed)
    }


    /// Check if a formula is installed
    pub fn is_installed(&self, name: &str) -> bool {
        self.db.get_installed(name).is_some()
    }

    /// Get info about an installed formula
    pub fn get_installed(&self, name: &str) -> Option<crate::db::InstalledKeg> {
        self.db.get_installed(name)
    }

    /// List all installed formulas
    pub fn list_installed(&self) -> Result<Vec<crate::db::InstalledKeg>, Error> {
        self.db.list_installed()
    }

    /// Tap a repository
    pub fn tap(&self, user: &str, repo: &str) -> Result<(), Error> {
        self.tap_manager.ensure_tap(user, repo).map(|_| ())
    }

    /// Install a cask (Linux only support for now)
    pub async fn install_cask(
        &self, 
        name: &str, 
        progress: Option<Arc<dyn Fn(InstallProgress) + Send + Sync>>,
    ) -> Result<(), Error> {
        let (path, cask_name) = self.tap_manager.resolve_cask(name)?;
        let content = std::fs::read_to_string(&path).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to read cask file {}: {}", path.display(), e),
        })?;

        // Determine arch for logic
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64_linux"
        } else {
            "x86_64_linux"
        };

        let cask = Cask::parse(&content, arch).map_err(|e| Error::FormulaParseError {
            message: e.to_string(),
        })?;

        // Download artifact
        let url = cask.url;
        let sha256 = cask.sha256;
        
        // We can reuse downloader but we need a DownloadRequest
        let request = DownloadRequest {
            url: url.clone(),
            sha256: sha256.clone(),
            name: cask_name.clone(),
        };

        // Download (using single download for simplicity, or we could stream)
        let blob_path = self.downloader.download_single(request, progress.clone()).await?;

        // Ensure entry in store (verify checksum)
        let store_entry = self.store.ensure_entry(&sha256, &blob_path)?;
        
        if let Some(cb) = &progress {
            cb(InstallProgress::UnpackStarted { name: cask.name.clone() });
        }

        // Prepare Caskroom directory
        // root/Caskroom/name/version
        let caskroom = self.store.root().join("Caskroom").join(&cask_name).join(&cask.version);
        if caskroom.exists() {
             std::fs::remove_dir_all(&caskroom).map_err(|e| Error::StoreCorruption {
                 message: format!("Failed to remove existing caskroom dir: {}", e),
             })?;
        }
        let caskroom_parent = caskroom.parent().unwrap();
        std::fs::create_dir_all(caskroom_parent).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to create caskroom parent: {}", e),
        })?;

        // Link store entry to caskroom
        // We do not extract again, as ensure_entry (via downloader) provides the content
        symlink(&store_entry, &caskroom).map_err(|e| Error::StoreCorruption {
             message: format!("Failed to symlink store entry to caskroom: {}", e),
        })?;

        if let Some(cb) = &progress {
            cb(InstallProgress::UnpackCompleted { name: cask.name.clone() });
            cb(InstallProgress::LinkStarted { name: cask.name.clone() });
        }

        // Link binaries
        // Scan artifacts
        for artifact in cask.artifacts {
            match artifact {
                zb_core::cask::Artifact::Binary(bin_path) => {
                     // bin_path is relative to staged path (caskroom)
                     let source = caskroom.join(&bin_path);
                     if !source.exists() {
                         eprintln!("Warning: Binary {} not found", source.display());
                         continue;
                     }
                     
                     let name = source.file_name().unwrap();
                     let target = self.linker.prefix().join("bin").join(name);
                     
                     // Helper: create symlink
                     if target.exists() {
                         std::fs::remove_file(&target).ok();
                     }
                     symlink(&source, &target).map_err(|e| Error::StoreCorruption {
                        message: format!("Failed to symlink binary: {}", e),
                     })?;
                }
                _ => {
                    // Ignore other artifacts for now
                }
            }
        }
        
        // Single binary field support (legacy)
        if let Some(bin_path) = cask.binary {
             let source = caskroom.join(&bin_path);
             if source.exists() {
                 let name = source.file_name().unwrap();
                 let target = self.linker.prefix().join("bin").join(name);
                 if target.exists() {
                     std::fs::remove_file(&target).ok();
                 }
                 symlink(&source, &target).map_err(|e| Error::StoreCorruption {
                    message: format!("Failed to symlink binary: {}", e),
                 })?;
             }
        }

        if let Some(cb) = &progress {
            cb(InstallProgress::LinkCompleted { name: cask.name.clone() });
            cb(InstallProgress::InstallCompleted { name: cask.name.clone() });
        }

        Ok(())
    }
}

/// Create an Installer with standard paths
pub fn create_installer(
    root: &Path,
    prefix: &Path,
    download_concurrency: usize,
) -> Result<Installer, Error> {
    use std::fs;

    // First ensure the root directory exists
    if !root.exists() {
        fs::create_dir_all(root).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::StoreCorruption {
                    message: format!(
                        r#"cannot create root directory '{}': permission denied.

Create it with:
  sudo mkdir -p {} && sudo chown $USER {}"#,
                        root.display(),
                        root.display(),
                        root.display()
                    ),
                }
            } else {
                Error::StoreCorruption {
                    message: format!("failed to create root directory '{}': {e}", root.display()),
                }
            }
        })?;
    }

    // Ensure all subdirectories exist
    fs::create_dir_all(root.join("db")).map_err(|e| Error::StoreCorruption {
        message: format!("failed to create db directory: {e}"),
    })?;

    let api_client = ApiClient::new();
    let blob_cache = BlobCache::new(&root.join("cache")).map_err(|e| Error::StoreCorruption {
        message: format!("failed to create blob cache: {e}"),
    })?;
    let store = Store::new(root).map_err(|e| Error::StoreCorruption {
        message: format!("failed to create store: {e}"),
    })?;
    // Use prefix/Cellar so bottles' hardcoded rpaths work
    let cellar = Cellar::new_at(prefix.join("Cellar")).map_err(|e| Error::StoreCorruption {
        message: format!("failed to create cellar: {e}"),
    })?;
    let linker = Linker::new(prefix).map_err(|e| Error::StoreCorruption {
        message: format!("failed to create linker: {e}"),
    })?;
    let db = Database::open(&root.join("db/zb.sqlite3"))?;
    let tap_manager = TapManager::new(root.to_path_buf());

    Ok(Installer::new(
        api_client,
        blob_cache,
        store,
        cellar,
        linker,
        db,
        tap_manager,
        download_concurrency,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn create_bottle_tarball(formula_name: &str) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        use tar::Builder;

        let mut builder = Builder::new(Vec::new());

        // Create bin directory with executable
        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{}/1.0.0/bin/{}", formula_name, formula_name))
            .unwrap();
        header.set_size(20);
        header.set_mode(0o755);
        header.set_cksum();

        let content = format!("#!/bin/sh\necho {}", formula_name);
        builder.append(&header, content.as_bytes()).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }

    fn get_test_bottle_tag() -> &'static str {
        if cfg!(target_os = "linux") {
            "x86_64_linux"
        } else {
            "arm64_sonoma"
        }
    }

    #[tokio::test]
    async fn install_completes_successfully() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        // Create bottle
        let bottle = create_bottle_tarball("testpkg");
        let bottle_sha = sha256_hex(&bottle);

        // Create formula JSON
        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "testpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        // Mount formula API mock
        Mock::given(method("GET"))
            .and(path("/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        // Mount bottle download mock
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        // Create installer with mocked API
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install
        installer.install("testpkg", true).await.unwrap();

        // Verify keg exists
        assert!(root.join("cellar/testpkg/1.0.0").exists());

        // Verify link exists
        assert!(prefix.join("bin/testpkg").exists());

        // Verify database records
        let installed = installer.db.get_installed("testpkg");
        assert!(installed.is_some());
        assert_eq!(installed.unwrap().version, "1.0.0");
    }

    #[tokio::test]
    async fn uninstall_cleans_everything() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        // Create bottle
        let bottle = create_bottle_tarball("uninstallme");
        let bottle_sha = sha256_hex(&bottle);

        // Create formula JSON
        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "uninstallme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/uninstallme-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        // Mount mocks
        Mock::given(method("GET"))
            .and(path("/uninstallme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/uninstallme-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        // Create installer
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install
        installer.install("uninstallme", true).await.unwrap();

        // Verify installed
        assert!(installer.is_installed("uninstallme"));
        assert!(root.join("cellar/uninstallme/1.0.0").exists());
        assert!(prefix.join("bin/uninstallme").exists());

        // Uninstall
        installer.uninstall("uninstallme").unwrap();

        // Verify everything cleaned up
        assert!(!installer.is_installed("uninstallme"));
        assert!(!root.join("cellar/uninstallme/1.0.0").exists());
        assert!(!prefix.join("bin/uninstallme").exists());
    }

    #[tokio::test]
    async fn gc_removes_unreferenced_store_entries() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        // Create bottle
        let bottle = create_bottle_tarball("gctest");
        let bottle_sha = sha256_hex(&bottle);

        // Create formula JSON
        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "gctest",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/gctest-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        // Mount mocks
        Mock::given(method("GET"))
            .and(path("/gctest.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/bottles/gctest-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        // Create installer
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install and uninstall
        installer.install("gctest", true).await.unwrap();
        installer.uninstall("gctest").unwrap();

        // Run GC
        let removed = installer.gc().unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], bottle_sha);

        // Verify removed from store
        assert!(!installer.store.has_entry(&bottle_sha));
    }

    #[tokio::test]
    async fn gc_does_not_remove_referenced_store_entries() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        // Create bottle
        let bottle = create_bottle_tarball("keepme");
        let bottle_sha = sha256_hex(&bottle);

        // Create formula JSON
        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "keepme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/keepme-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        // Mount mocks
        Mock::given(method("GET"))
            .and(path("/keepme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/bottles/keepme-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        // Create installer
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install but don't uninstall
        installer.install("keepme", true).await.unwrap();

        // Run GC
        let removed = installer.gc().unwrap();
        assert_eq!(removed.len(), 0);

        // Verify still in store
        assert!(installer.store.has_entry(&bottle_sha));
    }

    #[tokio::test]
    async fn install_with_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        // Create bottles
        let dep_bottle = create_bottle_tarball("deppkg");
        let dep_sha = sha256_hex(&dep_bottle);
        let main_bottle = create_bottle_tarball("mainpkg");
        let main_sha = sha256_hex(&main_bottle);

        let tag = get_test_bottle_tag();

        // Create formula JSONs
        let dep_json = format!(
            r#"{{
                "name": "deppkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/deppkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            dep_sha
        );

        let main_json = format!(
            r#"{{
                "name": "mainpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": ["deppkg"],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/mainpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            main_sha
        );

        // Mount mocks
        Mock::given(method("GET"))
            .and(path("/deppkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&dep_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/mainpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&main_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/bottles/deppkg-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(dep_bottle))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/bottles/mainpkg-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(main_bottle))
            .mount(&mock_server)
            .await;

        // Create installer
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install main package (should also install dependency)
        installer.install("mainpkg", true).await.unwrap();

        // Verify both are installed
        assert!(installer.is_installed("mainpkg"));
        assert!(installer.is_installed("deppkg"));
    }

    #[tokio::test]
    async fn parallel_api_fetching_with_deep_deps() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();

        // Create 5 packages in a chain: pkg1 -> pkg2 -> pkg3 -> pkg4 -> pkg5
        for i in 1..=5 {
            let deps = if i < 5 {
                format!("[\"pkg{}\"]", i + 1)
            } else {
                "[]".to_string()
            };

            let json = format!(
                r#"{{
                    "name": "pkg{}",
                    "versions": {{ "stable": "1.0.0" }},
                    "dependencies": {},
                    "bottle": {{
                        "stable": {{
                            "files": {{
                                "{}": {{
                                    "url": "{}/bottles/pkg{}-1.0.0.{}.bottle.tar.gz",
                                    "sha256": "fake_sha{}"
                                }}
                            }}
                        }}
                    }}
                }}"#,
                i,
                deps,
                tag,
                mock_server.uri(),
                i,
                tag,
                i
            );

            Mock::given(method("GET"))
                .and(path(format!("/pkg{}.json", i)))
                .respond_with(ResponseTemplate::new(200).set_body_string(&json))
                .mount(&mock_server)
                .await;

            // Mock simple bottle download (will fail checksum but plan() doesn't check)
            Mock::given(method("GET"))
                .and(path(format!("/bottles/pkg{}-1.0.0.{}.bottle.tar.gz", i, tag)))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 10]))
                .mount(&mock_server)
                .await;
        }

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Plan "root" (pkg1)
        let plan = installer.plan("pkg1").await.unwrap();

        // Should have 5 formulas in correct topological order
        assert_eq!(plan.formulas.len(), 5);
        assert_eq!(plan.formulas[0].name, "pkg5");
        assert_eq!(plan.formulas[4].name, "pkg1");
    }

    #[tokio::test]
    async fn streaming_extraction_processes_as_downloads_complete() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();

        // Create bottles
        let fast_bottle = create_bottle_tarball("fastpkg");
        let fast_sha = sha256_hex(&fast_bottle);
        let slow_bottle = create_bottle_tarball("slowpkg");
        let slow_sha = sha256_hex(&slow_bottle);

        // Fast package JSON
        let fast_json = format!(
            r#"{{
                "name": "fastpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/fast-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            fast_sha
        );

        // Slow package JSON (depends on fast)
        let slow_json = format!(
            r#"{{
                "name": "slowpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": ["fastpkg"],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/slow-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            slow_sha
        );

        // Mount API mocks
        Mock::given(method("GET"))
            .and(path("/fastpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&fast_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/slowpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&slow_json))
            .mount(&mock_server)
            .await;

        // Fast download mock
        Mock::given(method("GET"))
            .and(path(format!("/bottles/fast-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fast_bottle))
            .mount(&mock_server)
            .await;

        // Slow download mock (delayed by 200ms)
        Mock::given(method("GET"))
            .and(path(format!("/bottles/slow-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(slow_bottle)
                    .set_delay(std::time::Duration::from_millis(200)),
            )
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install slow package (which depends on fast)
        // With streaming, fast should be extracted while slow is still downloading
        installer.install("slowpkg", true).await.unwrap();

        assert!(installer.is_installed("slowpkg"));
        assert!(installer.is_installed("fastpkg"));
    }

    #[tokio::test]
    async fn retries_on_corrupted_download() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        // Prepare valid bottle
        let valid_bottle = create_bottle_tarball("retrypkg");
        let valid_sha = sha256_hex(&valid_bottle);

        // Formula JSON
        let formula_json = format!(
            r#"{{
                "name": "retrypkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/retry.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            valid_sha
        );

        Mock::given(method("GET"))
            .and(path("/retrypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        // Custom responder that fails first time, succeeds second
        let call_count = Arc::new(AtomicUsize::new(0));
        let valid_bottle_clone = valid_bottle.clone();
        let call_count_clone = call_count.clone();

        Mock::given(method("GET"))
            .and(path("/bottles/retry.tar.gz"))
            .respond_with(move |_: &wiremock::Request| {
                if call_count_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Return corrupted data first time
                    ResponseTemplate::new(200).set_body_bytes(vec![0xAA; 50])
                } else {
                    // Return valid data
                    ResponseTemplate::new(200).set_body_bytes(valid_bottle_clone.clone())
                }
            })
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tap_manager = TapManager::new(root.clone());

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            tap_manager,
            4,
        );

        // Install - should succeed (first download is valid in this test)
        installer.install("retrypkg", true).await.unwrap();

        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        assert!(installer.is_installed("retrypkg"));
    }
}
