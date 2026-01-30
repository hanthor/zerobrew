use std::path::PathBuf;
use std::process::Command;
use zb_core::Error;

#[derive(Clone)]
pub struct TapManager {
    root: PathBuf,
}

impl TapManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn taps_dir(&self) -> PathBuf {
        self.root.join("Library/Taps")
    }

    /// Ensure a tap is installed (user/repo)
    /// repo can be "homebrew-foo" or just "foo" (which expands to homebrew-foo)
    pub fn ensure_tap(&self, user: &str, repo: &str) -> Result<PathBuf, Error> {
        let repo_name = if repo.starts_with("homebrew-") {
            repo.to_string()
        } else {
            format!("homebrew-{}", repo)
        };

        let tap_dir = self.taps_dir().join(user).join(&repo_name);

        if tap_dir.exists() {
            // Already installed, maybe update?
            // For now, assume if it exists it's fine.
            return Ok(tap_dir);
        }

        // Clone
        let url = format!("https://github.com/{}/{}.git", user, repo_name);
        println!("==> Tapping {}/{}...", user, repo);

        // Create user dir
        std::fs::create_dir_all(tap_dir.parent().unwrap()).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to create tap dir: {}", e),
        })?;

        let status = Command::new("git")
            .args(["clone", &url, &tap_dir.to_string_lossy()])
            .status()
            .map_err(|e| Error::NetworkFailure {
                message: format!("Failed to run git: {}", e),
            })?;

        if !status.success() {
            return Err(Error::NetworkFailure {
                message: format!("Failed to clone tap {}", url),
            });
        }

        Ok(tap_dir)
    }

    /// Resolve a cask by name, checking standard taps and specific tap if provided
    /// name can be "cask-name" or "user/repo/cask-name"
    pub fn resolve_cask(&self, name: &str) -> Result<(PathBuf, String), Error> {
        if let Some((user, rest)) = name.split_once('/')
            && let Some((repo, cask_name)) = rest.split_once('/')
        {
            // Specific tap: user/repo/cask
            let tap_dir = self.ensure_tap(user, repo)?;
            let cask_path = tap_dir.join("Casks").join(format!("{}.rb", cask_name));
            if cask_path.exists() {
                return Ok((cask_path, cask_name.to_string()));
            }
            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        }

        // Search installed taps? Or just error for now unless it's a known default?
        // Zerobrew doesn't have a default Cask tap yet.
        Err(Error::MissingFormula {
            name: name.to_string(),
        })
    }

    /// List all available items (formulas and casks) from installed taps
    pub fn list_available_items(&self) -> Vec<String> {
        let mut items = Vec::new();

        if let Ok(users) = std::fs::read_dir(self.taps_dir()) {
            for user_entry in users.flatten() {
                if !user_entry.path().is_dir() {
                    continue;
                }
                let user = user_entry.file_name();
                let user_str = user.to_string_lossy();

                if let Ok(repos) = std::fs::read_dir(user_entry.path()) {
                    for repo_entry in repos.flatten() {
                        if !repo_entry.path().is_dir() {
                            continue;
                        }
                        let repo = repo_entry.file_name();
                        let repo_str = repo.to_string_lossy();

                        // Clean repo name: "homebrew-foo" -> "foo"
                        let short_repo = if repo_str.starts_with("homebrew-") {
                            repo_str.trim_start_matches("homebrew-")
                        } else {
                            &repo_str
                        };

                        // Scan Formula directory
                        let formula_dir = repo_entry.path().join("Formula");
                        if formula_dir.exists()
                            && let Ok(files) = std::fs::read_dir(formula_dir)
                        {
                            for file in files.flatten() {
                                let path = file.path();
                                if path.extension().is_some_and(|e| e == "rb")
                                    && let Some(stem) = path.file_stem()
                                {
                                    // Short name
                                    items.push(stem.to_string_lossy().to_string());
                                    // Fully qualified name: user/repo/name
                                    items.push(format!(
                                        "{}/{}/{}",
                                        user_str,
                                        short_repo,
                                        stem.to_string_lossy()
                                    ));
                                }
                            }
                        }

                        // Scan Casks directory
                        let cask_dir = repo_entry.path().join("Casks");
                        if cask_dir.exists()
                            && let Ok(files) = std::fs::read_dir(cask_dir)
                        {
                            for file in files.flatten() {
                                let path = file.path();
                                if path.extension().is_some_and(|e| e == "rb")
                                    && let Some(stem) = path.file_stem()
                                {
                                    // Short name
                                    items.push(stem.to_string_lossy().to_string());
                                    // Fully qualified name: user/repo/name
                                    items.push(format!(
                                        "{}/{}/{}",
                                        user_str,
                                        short_repo,
                                        stem.to_string_lossy()
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        items.sort();
        items.dedup();
        items
    }

    pub fn tap_path(&self, user: &str, repo: &str) -> PathBuf {
        let repo_name = if repo.starts_with("homebrew-") {
            repo.to_string()
        } else {
            format!("homebrew-{}", repo)
        };
        self.taps_dir().join(user).join(repo_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_list_items() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let manager = TapManager::new(root.clone());

        // Create fake structure
        let core = manager.tap_path("homebrew", "core");
        fs::create_dir_all(core.join("Formula")).unwrap();
        fs::write(core.join("Formula/foo.rb"), "").unwrap();

        let custom = manager.tap_path("user", "repo");
        fs::create_dir_all(custom.join("Casks")).unwrap();
        fs::write(custom.join("Casks/bar.rb"), "").unwrap();

        let items = manager.list_available_items();

        assert!(items.contains(&"foo".to_string()));
        assert!(items.contains(&"homebrew/core/foo".to_string()));
        assert!(items.contains(&"bar".to_string()));
        assert!(items.contains(&"user/repo/bar".to_string()));
    }

    #[test]
    fn test_resolve_cask() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        let manager = TapManager::new(root);

        // Pre-create the directory so ensure_tap doesn't try to git clone
        let tap_dir = manager.tap_path("user", "repo");
        fs::create_dir_all(tap_dir.join("Casks")).unwrap();
        fs::write(tap_dir.join("Casks/mycask.rb"), "").unwrap();

        let (path, name) = manager.resolve_cask("user/repo/mycask").unwrap();
        assert_eq!(name, "mycask");
        assert!(path.ends_with("user/homebrew-repo/Casks/mycask.rb"));
    }
}
