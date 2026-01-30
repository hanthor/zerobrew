use std::path::{Path, PathBuf};
use std::process::Command;
use zb_core::Error;

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
        if let Some((user, rest)) = name.split_once('/') {
            if let Some((repo, cask_name)) = rest.split_once('/') {
                // Specific tap: user/repo/cask
                let tap_dir = self.ensure_tap(user, repo)?;
                let cask_path = tap_dir.join("Casks").join(format!("{}.rb", cask_name));
                if cask_path.exists() {
                    return Ok((cask_path, cask_name.to_string()));
                }
                return Err(Error::MissingFormula { name: name.to_string() });
            }
        }
        
        // Search installed taps? Or just error for now unless it's a known default?
        // Zerobrew doesn't have a default Cask tap yet.
        Err(Error::MissingFormula { name: name.to_string() })
    }
}
