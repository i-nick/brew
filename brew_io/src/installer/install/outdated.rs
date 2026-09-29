use std::collections::HashMap;

use brew_core::{Error, select_bottle};

use crate::installer::cask::resolve_cask;
use crate::storage::db::InstalledKeg;

use super::{Installer, OutdatedPackage};

impl Installer {
    pub async fn is_outdated(&self, name: &str) -> Result<Option<OutdatedPackage>, Error> {
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;

        self.outdated_for_installed(&installed).await
    }

    async fn outdated_for_installed(
        &self,
        installed: &InstalledKeg,
    ) -> Result<Option<OutdatedPackage>, Error> {
        if installed.name.starts_with("cask:") {
            return self.outdated_cask(installed).await;
        }

        self.outdated_formula(installed).await
    }

    async fn outdated_formula(
        &self,
        installed: &InstalledKeg,
    ) -> Result<Option<OutdatedPackage>, Error> {
        let name = &installed.name;
        let formula = self.api_client.get_formula(name).await?;
        let is_source = installed.store_key.starts_with("source:");

        if is_source {
            let current_version = formula.effective_version();
            if installed.version == current_version {
                Ok(None)
            } else {
                Ok(Some(OutdatedPackage {
                    name: name.to_string(),
                    installed_version: installed.version.clone(),
                    installed_sha256: installed.store_key.clone(),
                    current_version,
                    current_sha256: String::new(),
                    is_source_build: true,
                }))
            }
        } else {
            let bottle = select_bottle(&formula)?;
            if installed.store_key == bottle.sha256 {
                Ok(None)
            } else {
                Ok(Some(OutdatedPackage {
                    name: name.to_string(),
                    installed_version: installed.version.clone(),
                    installed_sha256: installed.store_key.clone(),
                    current_version: formula.effective_version(),
                    current_sha256: bottle.sha256,
                    is_source_build: false,
                }))
            }
        }
    }

    async fn outdated_cask(
        &self,
        installed: &InstalledKeg,
    ) -> Result<Option<OutdatedPackage>, Error> {
        let token = installed
            .name
            .strip_prefix("cask:")
            .ok_or_else(|| Error::InvalidArgument {
                message: format!("invalid installed cask name '{}'", installed.name),
            })?;
        let cask_json = self.api_client.get_cask(token).await?;
        let cask = resolve_cask(token, &cask_json)?;

        if installed.store_key == cask.sha256 {
            return Ok(None);
        }

        Ok(Some(OutdatedPackage {
            name: installed.name.clone(),
            installed_version: installed.version.clone(),
            installed_sha256: installed.store_key.clone(),
            current_version: cask.version,
            current_sha256: cask.sha256,
            is_source_build: false,
        }))
    }

    pub async fn check_outdated(&self) -> Result<(Vec<OutdatedPackage>, Vec<String>), Error> {
        let installed = self.db.list_installed()?;
        if installed.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let (installed_casks, installed_formulas): (Vec<_>, Vec<_>) = installed
            .into_iter()
            .partition(|keg| keg.name.starts_with("cask:"));

        let installed_names: std::collections::HashSet<&str> =
            installed_formulas.iter().map(|k| k.name.as_str()).collect();

        let mut bulk_map: HashMap<String, brew_core::Formula> = HashMap::new();
        if !installed_formulas.is_empty() {
            let bulk_raw = self.api_client.get_all_formulas_raw().await?;
            let bulk_values: Vec<serde_json::Value> = serde_json::from_str(&bulk_raw)
                .map_err(Error::network("failed to parse bulk formula JSON"))?;

            for val in bulk_values {
                let name = match val.get("name").and_then(|n| n.as_str()) {
                    Some(n) if installed_names.contains(n) => n.to_string(),
                    _ => continue,
                };
                if let Ok(f) = serde_json::from_value(val) {
                    bulk_map.insert(name, f);
                }
            }
        }

        let mut outdated = Vec::new();
        let mut warnings = Vec::new();

        for keg in &installed_formulas {
            let is_tap = keg.name.contains('/');

            let formula = if is_tap || !bulk_map.contains_key(&keg.name) {
                match self.api_client.get_formula(&keg.name).await {
                    Ok(f) => f,
                    Err(e) => {
                        warnings.push(format!("{}: {}", keg.name, e));
                        continue;
                    }
                }
            } else {
                bulk_map.remove(&keg.name).unwrap()
            };

            let is_source = keg.store_key.starts_with("source:");

            if is_source {
                let current_version = formula.effective_version();
                if keg.version != current_version {
                    outdated.push(OutdatedPackage {
                        name: keg.name.clone(),
                        installed_version: keg.version.clone(),
                        installed_sha256: keg.store_key.clone(),
                        current_version,
                        current_sha256: String::new(),
                        is_source_build: true,
                    });
                }
            } else {
                match select_bottle(&formula) {
                    Ok(bottle) => {
                        if keg.store_key != bottle.sha256 {
                            outdated.push(OutdatedPackage {
                                name: keg.name.clone(),
                                installed_version: keg.version.clone(),
                                installed_sha256: keg.store_key.clone(),
                                current_version: formula.effective_version(),
                                current_sha256: bottle.sha256,
                                is_source_build: false,
                            });
                        }
                    }
                    Err(e) => warnings.push(format!("{}: {}", keg.name, e)),
                }
            }
        }

        for keg in &installed_casks {
            match self.outdated_cask(keg).await {
                Ok(Some(pkg)) => outdated.push(pkg),
                Ok(None) => {}
                Err(e) => warnings.push(format!("{}: {}", keg.name, e)),
            }
        }

        outdated.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((outdated, warnings))
    }

    pub async fn suggest_formulas(&self, query: &str, limit: usize) -> Result<Vec<String>, Error> {
        self.api_client.suggest_formulas(query, limit).await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    use super::super::test_support::get_test_bottle_tag;

    fn formula_json(name: &str, version: &str, sha256: &str) -> String {
        let tag = get_test_bottle_tag();
        format!(
            r#"{{
                "name": "{}",
                "versions": {{ "stable": "{}" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "https://example.com/{}-{}.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            name, version, tag, name, version, tag, sha256
        )
    }

    fn cask_json(token: &str, version: &str, sha256: &str) -> String {
        format!(
            r#"{{
                "token": "{token}",
                "version": "{version}",
                "url": "https://example.com/{token}-{version}.zip",
                "sha256": "{sha256}",
                "artifacts": [
                    {{ "app": ["{token}.app"] }},
                    {{ "binary": ["$APPDIR/{token}.app/Contents/MacOS/{token}", {{ "target": "{token}" }}] }}
                ]
            }}"#
        )
    }

    fn tap_formula_rb(mock_server_uri: &str, version: &str, sha256: &str) -> String {
        let tag = get_test_bottle_tag();
        format!(
            r#"
class Terraform < Formula
  version "{version}"
  bottle do
    root_url "{mock_server_uri}/v2/hashicorp/tap"
    sha256 {tag}: "{sha256}"
  end
end
"#
        )
    }

    async fn test_installer() -> (Installer, MockServer, TempDir) {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_cask_base_url(format!("{}/cask", mock_server.uri()))
            .with_tap_raw_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix,
            root.join("locks"),
        );
        (installer, mock_server, tmp)
    }

    #[tokio::test]
    async fn suggest_formulas_returns_matches_from_api_client() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bulk = r#"[
            {"name":"python"},
            {"name":"pytest"},
            {"name":"pypy"}
        ]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix,
            root.join("locks"),
        );

        let suggestions = installer.suggest_formulas("pythn", 3).await.unwrap();
        assert_eq!(suggestions.first().map(String::as_str), Some("python"));
    }

    #[tokio::test]
    async fn is_outdated_returns_none_when_sha256_matches() {
        let (mut installer, mock_server, _tmp) = test_installer().await;
        let sha = "abc123def456";

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.1", sha).unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json("jq", "1.7.1", sha)),
            )
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn is_outdated_returns_some_when_sha256_differs() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.0", "old_sha256").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                "jq",
                "1.7.1",
                "new_sha256",
            )))
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap().unwrap();
        assert_eq!(result.name, "jq");
        assert_eq!(result.installed_version, "1.7.0");
        assert_eq!(result.current_version, "1.7.1");
        assert!(!result.is_source_build);
    }

    #[tokio::test]
    async fn is_outdated_supports_explicit_tap_formula_references() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri())
            .with_cask_base_url(format!("{}/cask", mock_server.uri()));
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix,
            root.join("locks"),
        );

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("hashicorp/tap/terraform", "1.10.0", "old_sha256")
                .unwrap();
            tx.commit().unwrap();
        }

        let new_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb(
                &mock_server.uri(),
                "1.11.0",
                new_sha,
            )))
            .mount(&mock_server)
            .await;

        let result = installer
            .is_outdated("hashicorp/tap/terraform")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.name, "hashicorp/tap/terraform");
        assert_eq!(result.installed_version, "1.10.0");
        assert_eq!(result.current_version, "1.11.0");
    }

    #[tokio::test]
    async fn is_outdated_supports_explicit_tap_cask_references() {
        let (mut installer, mock_server, _tmp) = test_installer().await;
        let old_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let new_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("cask:example/tap/zed", "1.0.0", old_sha)
                .unwrap();
            tx.commit().unwrap();
        }

        let rb = format!(
            r#"
cask "zed" do
  version "1.1.0"
  url "https://example.com/Zed.zip"
  sha256 "{new_sha}"
  app "Zed.app"
end
"#
        );
        Mock::given(method("GET"))
            .and(path("/example/homebrew-tap/main/Casks/zed.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let result = installer
            .is_outdated("cask:example/tap/zed")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.name, "cask:example/tap/zed");
        assert_eq!(result.installed_version, "1.0.0");
        assert_eq!(result.current_version, "1.1.0");
        assert_eq!(result.current_sha256, new_sha);
    }

    #[tokio::test]
    async fn is_outdated_errors_for_not_installed() {
        let (installer, _mock_server, _tmp) = test_installer().await;

        let err = installer.is_outdated("jq").await.unwrap_err();
        assert!(matches!(err, brew_core::Error::NotInstalled { .. }));
    }

    #[tokio::test]
    async fn is_outdated_source_build_compares_version_only() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.1", "source:jq:1.7.1").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                "jq",
                "1.7.1",
                "irrelevant",
            )))
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn is_outdated_source_build_detects_new_version() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.6", "source:jq:1.6").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                "jq",
                "1.7.1",
                "irrelevant",
            )))
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("jq").await.unwrap().unwrap();
        assert_eq!(result.installed_version, "1.6");
        assert_eq!(result.current_version, "1.7.1");
        assert!(result.is_source_build);
    }

    #[tokio::test]
    async fn is_outdated_returns_none_when_cask_sha_matches() {
        let (mut installer, mock_server, _tmp) = test_installer().await;
        let sha = "abc123def456";

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("cask:zed", "1.0.0", sha).unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/cask/zed.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(cask_json("zed", "1.0.0", sha)),
            )
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("cask:zed").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn is_outdated_detects_cask_rebuild_with_same_version() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("cask:zed", "1.0.0", "old_sha").unwrap();
            tx.commit().unwrap();
        }

        Mock::given(method("GET"))
            .and(path("/cask/zed.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(cask_json("zed", "1.0.0", "new_sha")),
            )
            .mount(&mock_server)
            .await;

        let result = installer.is_outdated("cask:zed").await.unwrap().unwrap();
        assert_eq!(result.name, "cask:zed");
        assert_eq!(result.installed_version, "1.0.0");
        assert_eq!(result.current_version, "1.0.0");
        assert_eq!(result.current_sha256, "new_sha");
    }

    #[tokio::test]
    async fn check_outdated_empty_when_nothing_installed() {
        let (installer, _mock_server, _tmp) = test_installer().await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert!(outdated.is_empty());
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn check_outdated_continues_on_network_failure() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("good", "1.0.0", "old_sha").unwrap();
            tx.record_install("bad", "1.0.0", "old_sha").unwrap();
            tx.commit().unwrap();
        }

        let bulk = format!("[{}]", formula_json("good", "2.0.0", "new_sha"));
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/formula/bad.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].name, "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad"));
    }

    #[tokio::test]
    async fn check_outdated_warns_on_missing_bottle() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("nobottle", "1.0.0", "old_sha").unwrap();
            tx.commit().unwrap();
        }

        let bulk = r#"[{
            "name": "nobottle",
            "versions": { "stable": "2.0.0" },
            "dependencies": [],
            "bottle": { "stable": { "files": {} } }
        }]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert!(outdated.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nobottle"));
    }

    #[tokio::test]
    async fn check_outdated_includes_outdated_casks() {
        let (mut installer, mock_server, _tmp) = test_installer().await;

        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("jq", "1.7.0", "old_formula_sha").unwrap();
            tx.record_install("cask:zed", "1.0.0", "old_cask_sha")
                .unwrap();
            tx.commit().unwrap();
        }

        let bulk = format!("[{}]", formula_json("jq", "1.7.1", "new_formula_sha"));
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/cask/zed.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(cask_json(
                "zed",
                "1.0.1",
                "new_cask_sha",
            )))
            .mount(&mock_server)
            .await;

        let (outdated, warnings) = installer.check_outdated().await.unwrap();
        assert!(warnings.is_empty());
        assert_eq!(outdated.len(), 2);
        assert!(outdated.iter().any(|pkg| pkg.name == "jq"));
        assert!(outdated.iter().any(|pkg| pkg.name == "cask:zed"));
    }
}
