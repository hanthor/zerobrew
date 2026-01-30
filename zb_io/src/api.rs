use crate::cache::{ApiCache, CacheEntry};
use zb_core::{Error, Formula};

pub struct ApiClient {
    base_url: String,
    client: reqwest::Client,
    cache: Option<ApiCache>,
}

enum EntryType {
    Formula,
    Cask,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FormulaSummary {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub is_cask: bool,
}

impl ApiClient {
    pub fn new() -> Self {
        Self::with_base_url("https://formulae.brew.sh/api/formula".to_string())
    }

    pub fn with_base_url(base_url: String) -> Self {
        // Use HTTP/2 with connection pooling for better multiplexing of parallel requests
        let client = reqwest::Client::builder()
            .user_agent("zerobrew/0.1")
            .pool_max_idle_per_host(20)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base_url,
            client,
            cache: None,
        }
    }

    pub fn with_cache(mut self, cache: ApiCache) -> Self {
        self.cache = Some(cache);
        self
    }

    pub async fn get_formula(&self, name: &str) -> Result<Formula, Error> {
        let url = format!("{}/{}.json", self.base_url, name);

        let cached_entry = self.cache.as_ref().and_then(|c| c.get(&url));

        let mut request = self.client.get(&url);

        if let Some(ref entry) = cached_entry {
            if let Some(ref etag) = entry.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(ref last_modified) = entry.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }

        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(entry) = cached_entry
        {
            let formula: Formula =
                serde_json::from_str(&entry.body).map_err(|e| Error::NetworkFailure {
                    message: format!("failed to parse cached formula JSON: {e}"),
                })?;
            return Ok(formula);
        }

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = response.text().await.map_err(|e| Error::NetworkFailure {
            message: format!("failed to read response body: {e}"),
        })?;

        if let Some(ref cache) = self.cache {
            let entry = CacheEntry {
                etag,
                last_modified,
                body: body.clone(),
            };
            let _ = cache.put(&url, &entry);
        }

        let formula: Formula = serde_json::from_str(&body).map_err(|e| Error::NetworkFailure {
            message: format!("failed to parse formula JSON: {e}"),
        })?;

        Ok(formula)
    }

    pub async fn get_formula_json(&self, name: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/{}.json", self.base_url, name);
        let cached_entry = self.cache.as_ref().and_then(|c| c.get(&url));
        let mut request = self.client.get(&url);

        if let Some(ref entry) = cached_entry {
            if let Some(ref etag) = entry.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(ref last_modified) = entry.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }

        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(entry) = cached_entry
        {
            let val: serde_json::Value =
                serde_json::from_str(&entry.body).map_err(|e| Error::NetworkFailure {
                    message: format!("failed to parse cached formula JSON: {e}"),
                })?;
            return Ok(val);
        }

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = response.text().await.map_err(|e| Error::NetworkFailure {
            message: format!("failed to read response body: {e}"),
        })?;

        if let Some(ref cache) = self.cache {
            let entry = CacheEntry {
                etag,
                last_modified,
                body: body.clone(),
            };
            let _ = cache.put(&url, &entry);
        }

        let val: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| Error::NetworkFailure {
                message: format!("failed to parse formula JSON: {e}"),
            })?;
        Ok(val)
    }

    /// Fetch a cask from the Homebrew API
    /// Returns (url, sha256, version, name, artifacts) for the current platform
    pub async fn get_cask(
        &self,
        name: &str,
    ) -> Result<(String, String, String, String, Vec<String>), Error> {
        let url = format!("https://formulae.brew.sh/api/cask/{}.json", name);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::NetworkFailure {
                message: e.to_string(),
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        let body = response.text().await.map_err(|e| Error::NetworkFailure {
            message: format!("failed to read response body: {e}"),
        })?;

        // Parse cask JSON
        #[derive(serde::Deserialize)]
        struct Variation {
            url: String,
            sha256: String,
        }

        #[derive(serde::Deserialize)]
        struct CaskJson {
            token: String,
            version: String,
            url: Option<String>,
            sha256: Option<String>,
            artifacts: Vec<serde_json::Value>,
            variations: Option<std::collections::HashMap<String, Variation>>,
        }

        let cask: CaskJson = serde_json::from_str(&body).map_err(|e| Error::NetworkFailure {
            message: format!("failed to parse cask JSON: {e}"),
        })?;

        // Determine platform key
        let platform_key = if cfg!(target_arch = "aarch64") {
            "arm64_linux"
        } else {
            "x86_64_linux"
        };

        // Try to get platform-specific variation first
        let (download_url, sha256) = if let Some(variations) = &cask.variations {
            if let Some(variation) = variations.get(platform_key) {
                (variation.url.clone(), variation.sha256.clone())
            } else {
                // Fall back to default
                (
                    cask.url.ok_or_else(|| Error::MissingFormula {
                        name: format!("{} (no Linux URL)", name),
                    })?,
                    cask.sha256.ok_or_else(|| Error::MissingFormula {
                        name: format!("{} (no sha256)", name),
                    })?,
                )
            }
        } else {
            (
                cask.url.ok_or_else(|| Error::MissingFormula {
                    name: format!("{} (no URL)", name),
                })?,
                cask.sha256.ok_or_else(|| Error::MissingFormula {
                    name: format!("{} (no sha256)", name),
                })?,
            )
        };

        // Extract binary artifacts
        let mut binaries = Vec::new();
        for artifact in &cask.artifacts {
            if let Some(obj) = artifact.as_object()
                && let Some(binary) = obj.get("binary")
                && let Some(arr) = binary.as_array()
            {
                for b in arr {
                    if let Some(s) = b.as_str() {
                        binaries.push(s.to_string());
                    }
                }
            }
        }

        Ok((download_url, sha256, cask.version, cask.token, binaries))
    }

    pub async fn get_cask_json(&self, name: &str) -> Result<serde_json::Value, Error> {
        let url = format!("https://formulae.brew.sh/api/cask/{}.json", name);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::NetworkFailure {
                message: e.to_string(),
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::MissingFormula {
                name: name.to_string(),
            });
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        let body = response.text().await.map_err(|e| Error::NetworkFailure {
            message: format!("failed to read response body: {e}"),
        })?;

        let val: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| Error::NetworkFailure {
                message: format!("failed to parse cask JSON: {e}"),
            })?;
        Ok(val)
    }
    pub async fn get_all_formula_names(&self) -> Result<Vec<String>, Error> {
        let summaries = self.get_all_formula_summaries().await?;
        Ok(summaries.into_iter().map(|s| s.name).collect())
    }

    pub async fn get_all_cask_names(&self) -> Result<Vec<String>, Error> {
        let summaries = self.get_all_cask_summaries().await?;
        Ok(summaries.into_iter().map(|s| s.name).collect())
    }

    pub async fn get_all_formula_summaries(&self) -> Result<Vec<FormulaSummary>, Error> {
        self.fetch_summaries_from_list(
            "https://formulae.brew.sh/api/formula.json",
            EntryType::Formula,
        )
        .await
    }

    pub async fn get_all_cask_summaries(&self) -> Result<Vec<FormulaSummary>, Error> {
        self.fetch_summaries_from_list("https://formulae.brew.sh/api/cask.json", EntryType::Cask)
            .await
    }

    async fn fetch_summaries_from_list(
        &self,
        url: &str,
        entry_type: EntryType,
    ) -> Result<Vec<FormulaSummary>, Error> {
        let cached_entry = self.cache.as_ref().and_then(|c| c.get(url));
        let mut request = self.client.get(url);

        if let Some(ref entry) = cached_entry {
            if let Some(ref etag) = entry.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(ref last_modified) = entry.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }

        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(entry) = cached_entry
        {
            let summaries: Vec<FormulaSummary> =
                serde_json::from_str(&entry.body).map_err(|e| Error::NetworkFailure {
                    message: format!("failed to parse cached summaries JSON: {e}"),
                })?;
            return Ok(summaries);
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = response.text().await.map_err(|e| Error::NetworkFailure {
            message: format!("failed to read response body: {e}"),
        })?;

        // Extract metadata
        #[derive(serde::Deserialize)]
        struct BottleStable {
            files: Option<std::collections::HashMap<String, serde_json::Value>>,
        }
        #[derive(serde::Deserialize)]
        struct Bottle {
            stable: Option<BottleStable>,
        }
        #[derive(serde::Deserialize)]
        struct Versions {
            stable: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Item {
            name: Option<serde_json::Value>, // Casks use token, Formula use name
            token: Option<String>,           // Casks use token
            desc: Option<String>,

            // Formula fields
            versions: Option<Versions>,
            #[serde(default)]
            bottle: Option<Bottle>,

            // Cask fields
            version: Option<String>,
        }

        let items: Vec<Item> = serde_json::from_str(&body).map_err(|e| Error::NetworkFailure {
            message: format!("failed to parse full list JSON: {e}"),
        })?;

        let summaries: Vec<FormulaSummary> = items
            .into_iter()
            .filter_map(|i| {
                // OS-based filtering
                if cfg!(target_os = "linux") {
                    if matches!(entry_type, EntryType::Cask) {
                        return None;
                    }
                    // Filter formulas: if bottle info exists, ensure it has linux support
                    if matches!(entry_type, EntryType::Formula)
                        && let Some(files) = i
                            .bottle
                            .as_ref()
                            .and_then(|b| b.stable.as_ref())
                            .and_then(|s| s.files.as_ref())
                    {
                        let has_linux = files
                            .keys()
                            .any(|k| k.contains("linux") || k == "all" || k == "x86_64_linux");
                        if !has_linux {
                            return None;
                        }
                    }
                } else if cfg!(target_os = "macos") {
                    // On macOS, we generally support everything
                }

                let name = if let Some(token) = i.token {
                    token
                } else if let Some(name_val) = i.name {
                    if let Some(name_str) = name_val.as_str() {
                        name_str.to_string()
                    } else {
                        return None;
                    }
                } else {
                    return None;
                };

                let version = match entry_type {
                    EntryType::Formula => i.versions.and_then(|v| v.stable).unwrap_or_default(),
                    EntryType::Cask => i.version.unwrap_or_default(),
                };

                let is_cask = matches!(entry_type, EntryType::Cask);

                Some(FormulaSummary {
                    name,
                    version,
                    description: i.desc,
                    is_cask,
                })
            })
            .collect();

        if let Some(ref cache) = self.cache {
            let summaries_json = serde_json::to_string(&summaries).unwrap();
            let entry = CacheEntry {
                etag,
                last_modified,
                body: summaries_json,
            };
            let _ = cache.put(url, &entry);
        }

        Ok(summaries)
    }

    pub fn get_cached_formula_names(&self) -> Option<Vec<String>> {
        let url = "https://formulae.brew.sh/api/formula.json";
        self.cache
            .as_ref()
            .and_then(|c| c.get(url))
            .and_then(|entry| serde_json::from_str::<Vec<FormulaSummary>>(&entry.body).ok())
            .map(|summaries| summaries.into_iter().map(|s| s.name).collect())
    }

    pub fn get_cached_cask_names(&self) -> Option<Vec<String>> {
        let url = "https://formulae.brew.sh/api/cask.json";
        self.cache
            .as_ref()
            .and_then(|c| c.get(url))
            .and_then(|entry| serde_json::from_str::<Vec<FormulaSummary>>(&entry.body).ok())
            .map(|summaries| summaries.into_iter().map(|s| s.name).collect())
    }
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetches_formula_from_mock_server() {
        let mock_server = MockServer::start().await;

        let fixture = include_str!("../../zb_core/fixtures/formula_foo.json");

        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri());
        let formula = client.get_formula("foo").await.unwrap();

        assert_eq!(formula.name, "foo");
        assert_eq!(formula.versions.stable, "1.2.3");
    }

    #[tokio::test]
    async fn returns_missing_formula_on_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/nonexistent.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri());
        let err = client.get_formula("nonexistent").await.unwrap_err();

        assert!(matches!(
            err,
            Error::MissingFormula { name } if name == "nonexistent"
        ));
    }

    #[tokio::test]
    async fn first_request_stores_etag() {
        let mock_server = MockServer::start().await;
        let fixture = include_str!("../../zb_core/fixtures/formula_foo.json");

        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture)
                    .insert_header("etag", "\"abc123\""),
            )
            .mount(&mock_server)
            .await;

        let cache = ApiCache::in_memory().unwrap();
        let client = ApiClient::with_base_url(mock_server.uri()).with_cache(cache);

        let _ = client.get_formula("foo").await.unwrap();

        let cached = client
            .cache
            .as_ref()
            .unwrap()
            .get(&format!("{}/foo.json", mock_server.uri()))
            .unwrap();
        assert_eq!(cached.etag, Some("\"abc123\"".to_string()));
    }

    #[tokio::test]
    async fn second_request_sends_if_none_match() {
        let mock_server = MockServer::start().await;
        let fixture = include_str!("../../zb_core/fixtures/formula_foo.json");

        // First request returns 200 with ETag
        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture)
                    .insert_header("etag", "\"abc123\""),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let cache = ApiCache::in_memory().unwrap();
        let client = ApiClient::with_base_url(mock_server.uri()).with_cache(cache);

        // First request
        let _ = client.get_formula("foo").await.unwrap();

        // Reset mocks for second request
        mock_server.reset().await;

        // Second request should send If-None-Match and receive 304
        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .and(header("If-None-Match", "\"abc123\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&mock_server)
            .await;

        let formula = client.get_formula("foo").await.unwrap();
        assert_eq!(formula.name, "foo");
    }

    #[tokio::test]
    async fn uses_cached_body_on_304() {
        let mock_server = MockServer::start().await;
        let fixture = include_str!("../../zb_core/fixtures/formula_foo.json");

        // First request returns 200 with ETag
        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(fixture)
                    .insert_header("etag", "\"abc123\""),
            )
            .mount(&mock_server)
            .await;

        let cache = ApiCache::in_memory().unwrap();
        let client = ApiClient::with_base_url(mock_server.uri()).with_cache(cache);

        // First request populates cache
        let _ = client.get_formula("foo").await.unwrap();

        mock_server.reset().await;

        // Second request returns 304 (no body)
        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .and(header("If-None-Match", "\"abc123\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&mock_server)
            .await;

        // Should return cached formula
        let formula = client.get_formula("foo").await.unwrap();
        assert_eq!(formula.name, "foo");
        assert_eq!(formula.versions.stable, "1.2.3");
    }
}
