use std::path::PathBuf;
use std::process::Command;
use zb_core::Error;

/// Result of a tap operation
#[derive(Debug, Clone)]
pub enum TapResult {
    /// Already existed
    Existed,
    /// Freshly cloned
    Cloned,
}

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
    /// Returns Ok((path, TapResult)) where TapResult indicates if it was cloned or already existed
    pub fn ensure_tap(&self, user: &str, repo: &str) -> Result<(PathBuf, TapResult), Error> {
        let repo_name = if repo.starts_with("homebrew-") {
            repo.to_string()
        } else {
            format!("homebrew-{}", repo)
        };

        let tap_dir = self.taps_dir().join(user).join(&repo_name);

        if tap_dir.exists() {
            // Already installed
            return Ok((tap_dir, TapResult::Existed));
        }

        // Clone
        let url = format!("https://github.com/{}/{}.git", user, repo_name);

        // Create user dir
        std::fs::create_dir_all(tap_dir.parent().unwrap()).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to create tap dir: {}", e),
        })?;

        let output = Command::new("git")
            .args(["clone", "--depth", "1", &url, &tap_dir.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| Error::NetworkFailure {
                message: format!("Failed to run git: {}", e),
            })?;

        if !output.success() {
            return Err(Error::NetworkFailure {
                message: format!("Failed to clone tap {}", url),
            });
        }

        Ok((tap_dir, TapResult::Cloned))
    }

    /// Remove a tap
    pub fn untap(&self, name: &str) -> Result<(), Error> {
        let (user, repo) = if let Some((u, r)) = name.split_once('/') {
            (u, r)
        } else {
            // If no slash, maybe it's a full repo name "homebrew-foo"?
            // But usually untap expects user/repo
            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        };

        let repo_name = if repo.starts_with("homebrew-") {
            repo.to_string()
        } else {
            format!("homebrew-{}", repo)
        };

        let tap_dir = self.taps_dir().join(user).join(&repo_name);

        if !tap_dir.exists() {
            return Err(Error::MissingFormula {
                name: name.to_string(),
            }); // Tap not found
        }

        std::fs::remove_dir_all(tap_dir).map_err(|e| Error::StoreCorruption {
            message: format!("Failed to remove tap dir: {}", e),
        })?;

        Ok(())
    }

    /// List installed taps
    pub fn list_taps(&self) -> Vec<String> {
        let mut taps = Vec::new();
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

                        // Convert homebrew-core -> core
                        let short_repo = if repo_str.starts_with("homebrew-") {
                            repo_str.trim_start_matches("homebrew-")
                        } else {
                            &repo_str
                        };

                        taps.push(format!("{}/{}", user_str, short_repo));
                    }
                }
            }
        }
        taps.sort();
        taps
    }

    /// Resolve a cask by name, checking standard taps and specific tap if provided
    /// name can be "cask-name" or "user/repo/cask-name"
    pub fn resolve_cask(&self, name: &str) -> Result<(PathBuf, String), Error> {
        if let Some((user, rest)) = name.split_once('/')
            && let Some((repo, cask_name)) = rest.split_once('/')
        {
            // Specific tap: user/repo/cask
            let (tap_dir, _) = self.ensure_tap(user, repo)?;
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

    /// Resolve a formula by name from a tap
    /// name must be "user/repo/formula"
    pub fn resolve_formula(&self, name: &str) -> Result<zb_core::Formula, Error> {
        if let Some((user, rest)) = name.split_once('/')
            && let Some((repo, formula_name)) = rest.split_once('/')
        {
            // Specific tap: user/repo/formula
            let (tap_dir, _) = self.ensure_tap(user, repo)?;

            // Try root directory first (common for GoReleaser formulas)
            let formula_path = tap_dir.join(format!("{}.rb", formula_name));
            if formula_path.exists() {
                return crate::formula_parser::FormulaParser::parse_file(
                    &formula_path,
                    formula_name,
                );
            }

            // Try Formula subdirectory
            let formula_path = tap_dir.join("Formula").join(format!("{}.rb", formula_name));
            if formula_path.exists() {
                return crate::formula_parser::FormulaParser::parse_file(
                    &formula_path,
                    formula_name,
                );
            }

            // Try HomebrewFormula subdirectory
            let formula_path = tap_dir
                .join("HomebrewFormula")
                .join(format!("{}.rb", formula_name));
            if formula_path.exists() {
                return crate::formula_parser::FormulaParser::parse_file(
                    &formula_path,
                    formula_name,
                );
            }

            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        }

        // Not a tap formula
        Err(Error::MissingFormula {
            name: name.to_string(),
        })
    }

    /// Find a formula by its short name in any installed tap
    pub fn find_formula(&self, short_name: &str) -> Option<zb_core::Formula> {
        let taps = self.list_taps();
        for tap in taps {
            if let Some((user, repo)) = tap.split_once('/') {
                // Try resolving this formula in this tap
                let full_name = format!("{}/{}/{}", user, repo, short_name);
                if let Ok(formula) = self.resolve_formula(&full_name) {
                    return Some(formula);
                }
            }
        }
        None
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

                        // Scan HomebrewFormula directory
                        let formula_dir = repo_entry.path().join("HomebrewFormula");
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
}
