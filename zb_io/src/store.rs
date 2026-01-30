use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::extract::extract_tarball;
use zb_core::Error;

pub struct Store {
    store_dir: PathBuf,
    locks_dir: PathBuf,
}

impl Store {
    pub fn root(&self) -> &Path {
        self.store_dir.parent().unwrap()
    }

    pub fn new(root: &Path) -> io::Result<Self> {
        let store_dir = root.join("store");
        let locks_dir = root.join("locks");

        fs::create_dir_all(&store_dir)?;
        fs::create_dir_all(&locks_dir)?;

        Ok(Self {
            store_dir,
            locks_dir,
        })
    }

    pub fn entry_path(&self, store_key: &str) -> PathBuf {
        self.store_dir.join(store_key)
    }

    pub fn has_entry(&self, store_key: &str) -> bool {
        self.entry_path(store_key).exists()
    }

    pub fn ensure_entry(&self, store_key: &str, blob_path: &Path) -> Result<PathBuf, Error> {
        let entry_path = self.entry_path(store_key);

        // Fast path: already exists
        if entry_path.exists() {
            return Ok(entry_path);
        }

        // Acquire exclusive lock for this store_key
        let lock_path = self.locks_dir.join(format!("{store_key}.lock"));
        let lock_file = File::create(&lock_path).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create lock file: {e}"),
        })?;

        lock_file
            .lock_exclusive()
            .map_err(|e| Error::StoreCorruption {
                message: format!("failed to acquire lock: {e}"),
            })?;

        // Double-check after acquiring lock (another process may have created it)
        if entry_path.exists() {
            // Lock will be released when lock_file is dropped
            return Ok(entry_path);
        }

        // Unpack to a temp directory first
        let tmp_dir = self
            .store_dir
            .join(format!(".{store_key}.tmp.{}", std::process::id()));

        // Clean up any leftover temp directory from a previous interrupted extraction
        // (can happen if the process crashed or was killed during extraction)
        if tmp_dir.exists() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }

        fs::create_dir_all(&tmp_dir).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create temp directory: {e}"),
        })?;

        // Extract the tarball
        if let Err(e) = extract_tarball(blob_path, &tmp_dir) {
            // Clean up temp directory on failure
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }

        // Atomically rename temp dir to final path
        if let Err(e) = fs::rename(&tmp_dir, &entry_path) {
            // Clean up temp directory on failure
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(Error::StoreCorruption {
                message: format!("failed to rename store entry: {e}"),
            });
        }

        // Lock will be released when lock_file is dropped
        Ok(entry_path)
    }

    /// Like ensure_entry but for raw binary files (not archives).
    /// Copies the binary file directly to the store as-is.
    pub fn ensure_entry_binary(
        &self,
        store_key: &str,
        blob_path: &Path,
        binary_name: &str,
    ) -> Result<PathBuf, Error> {
        let entry_path = self.entry_path(store_key);

        // Fast path: already exists
        if entry_path.exists() {
            return Ok(entry_path);
        }

        // Acquire exclusive lock for this store_key
        let lock_path = self.locks_dir.join(format!("{store_key}.lock"));
        let lock_file = File::create(&lock_path).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create lock file: {e}"),
        })?;

        lock_file
            .lock_exclusive()
            .map_err(|e| Error::StoreCorruption {
                message: format!("failed to acquire lock: {e}"),
            })?;

        // Double-check after acquiring lock
        if entry_path.exists() {
            return Ok(entry_path);
        }

        // Create the entry directory with the binary inside
        fs::create_dir_all(&entry_path).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create entry directory: {e}"),
        })?;

        // Copy the binary file into the entry
        let dest_path = entry_path.join(binary_name);
        fs::copy(blob_path, &dest_path).map_err(|e| Error::StoreCorruption {
            message: format!("failed to copy binary file: {e}"),
        })?;

        // Make it executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&dest_path)
                .map_err(|e| Error::StoreCorruption {
                    message: format!("failed to get metadata: {e}"),
                })?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&dest_path, perms).map_err(|e| Error::StoreCorruption {
                message: format!("failed to set permissions: {e}"),
            })?;
        }

        Ok(entry_path)
    }

    /// Check if a file looks like an archive (gzip/tar/zip/zstd/rpm/appimage) based on magic bytes
    pub fn is_likely_archive(path: &Path) -> bool {
        if let Ok(mut file) = File::open(path) {
            let mut magic = [0u8; 12]; // Need 12 bytes for AppImage check
            if io::Read::read(&mut file, &mut magic).is_ok() {
                // gzip magic
                if magic[0] == 0x1f && magic[1] == 0x8b {
                    return true;
                }
                // zip magic
                if magic[0] == 0x50 && magic[1] == 0x4b {
                    return true;
                }
                // xz magic
                if magic[0] == 0xfd && magic[1] == 0x37 && magic[2] == 0x7a {
                    return true;
                }
                // zstd magic (28 b5 2f fd)
                if magic[0] == 0x28 && magic[1] == 0xb5 && magic[2] == 0x2f && magic[3] == 0xfd {
                    return true;
                }
                // rpm magic (ed ab ee db)
                if magic[0] == 0xed && magic[1] == 0xab && magic[2] == 0xee && magic[3] == 0xdb {
                    return true;
                }
                // AppImage magic (ELF + AI\x02 at offset 8)
                // ELF magic: 7f 45 4c 46
                // AppImage magic: 41 49 02 at offset 8
                if magic[0] == 0x7f
                    && magic[1] == 0x45
                    && magic[2] == 0x4c
                    && magic[3] == 0x46
                    && magic[8] == 0x41
                    && magic[9] == 0x49
                    && magic[10] == 0x02
                {
                    return true;
                }
            }
        }
        false
    }

    /// Remove a store entry. This should only be called when the refcount is 0.
    pub fn remove_entry(&self, store_key: &str) -> Result<(), Error> {
        let entry_path = self.entry_path(store_key);

        if !entry_path.exists() {
            return Ok(());
        }

        // Acquire exclusive lock for this store_key
        let lock_path = self.locks_dir.join(format!("{store_key}.lock"));
        let lock_file = File::create(&lock_path).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create lock file: {e}"),
        })?;

        lock_file
            .lock_exclusive()
            .map_err(|e| Error::StoreCorruption {
                message: format!("failed to acquire lock: {e}"),
            })?;

        // Remove the directory
        if entry_path.exists() {
            fs::remove_dir_all(&entry_path).map_err(|e| Error::StoreCorruption {
                message: format!("failed to remove store entry: {e}"),
            })?;
        }

        // Clean up the lock file
        let _ = fs::remove_file(&lock_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use tar::Builder;
    use tempfile::TempDir;

    fn create_test_tarball(content: &[u8]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header.set_path("test.txt").unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, content).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn second_call_is_noop() {
        let tmp = TempDir::new().unwrap();
        let store = Store::new(tmp.path()).unwrap();

        let tarball = create_test_tarball(b"hello world");
        let blob_path = tmp.path().join("test.tar.gz");
        fs::write(&blob_path, &tarball).unwrap();

        let store_key = "abc123";

        // First call extracts
        let path1 = store.ensure_entry(store_key, &blob_path).unwrap();
        assert!(path1.exists());
        assert!(path1.join("test.txt").exists());

        // Modify the file to detect if it gets overwritten
        fs::write(path1.join("marker.txt"), "original").unwrap();

        // Second call should be a no-op
        let path2 = store.ensure_entry(store_key, &blob_path).unwrap();
        assert_eq!(path1, path2);

        // Marker file should still exist (wasn't re-extracted)
        assert!(path2.join("marker.txt").exists());
    }

    #[test]
    fn concurrent_calls_unpack_once() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(Store::new(tmp.path()).unwrap());

        let tarball = create_test_tarball(b"concurrent test");
        let blob_path = tmp.path().join("test.tar.gz");
        fs::write(&blob_path, &tarball).unwrap();

        let store_key = "concurrent123";
        let unpack_count = Arc::new(AtomicUsize::new(0));

        // Spawn multiple threads that all try to ensure the same entry
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let store = store.clone();
                let blob = blob_path.clone();
                let count = unpack_count.clone();
                let key = store_key.to_string();

                thread::spawn(move || {
                    let entry_path = store.entry_path(&key);
                    let existed_before = entry_path.exists();

                    let result = store.ensure_entry(&key, &blob);

                    if !existed_before && result.is_ok() && entry_path.exists() {
                        // This thread might have been the one to create it
                        count.fetch_add(1, Ordering::SeqCst);
                    }

                    result
                })
            })
            .collect();

        // All threads should succeed
        for handle in handles {
            let result = handle.join().unwrap();
            assert!(result.is_ok());
        }

        // Entry should exist
        assert!(store.has_entry(store_key));

        // Content should be correct
        let entry_path = store.entry_path(store_key);
        let content = fs::read_to_string(entry_path.join("test.txt")).unwrap();
        assert_eq!(content, "concurrent test");
    }

    #[test]
    fn has_entry_returns_correct_state() {
        let tmp = TempDir::new().unwrap();
        let store = Store::new(tmp.path()).unwrap();

        let store_key = "checkme";

        assert!(!store.has_entry(store_key));

        let tarball = create_test_tarball(b"exists");
        let blob_path = tmp.path().join("test.tar.gz");
        fs::write(&blob_path, &tarball).unwrap();

        store.ensure_entry(store_key, &blob_path).unwrap();

        assert!(store.has_entry(store_key));
    }
}
