//! Tap registry: the third-party taps a user has added with `b tap` and the
//! formulae/casks each one provides.
//!
//! Unlike Homebrew, taps are not cloned. Tapping records an index of the
//! repository's formula and cask names (read from the GitHub tree API) so
//! bare names like `terraform` can be resolved to `hashicorp/tap/terraform`.
//! Formula sources are still fetched on demand from raw.githubusercontent.com.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use brew_core::Error;
use serde::{Deserialize, Serialize};

use crate::network::ApiClient;

const REGISTRY_FILE: &str = "taps.json";

/// A normalized `owner/repo` tap name (lowercase, without the `homebrew-`
/// repository prefix).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TapName {
    pub owner: String,
    pub repo: String,
}

impl TapName {
    pub fn parse(input: &str) -> Result<Self, Error> {
        let trimmed = input.trim().trim_end_matches('/');
        let invalid = || Error::InvalidArgument {
            message: format!("invalid tap name '{input}': expected 'user/repo'"),
        };

        let (owner, repo) = trimmed.split_once('/').ok_or_else(invalid)?;
        let owner = owner.to_ascii_lowercase();
        let repo = repo.to_ascii_lowercase();
        let repo = repo.strip_prefix("homebrew-").unwrap_or(&repo).to_string();

        let valid_part = |part: &str| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !valid_part(&owner) || !valid_part(&repo) {
            return Err(invalid());
        }

        Ok(Self { owner, repo })
    }

    /// `homebrew/core` and `homebrew/cask` are served by the JSON API and are
    /// always available.
    pub fn is_builtin(&self) -> bool {
        self.owner == "homebrew" && matches!(self.repo.as_str(), "core" | "cask")
    }

    /// Repository names to try on GitHub, in Homebrew's preference order.
    pub fn candidate_repos(&self) -> [String; 2] {
        [format!("homebrew-{}", self.repo), self.repo.clone()]
    }
}

impl std::fmt::Display for TapName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// What a tapped repository provides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapIndex {
    /// Normalized `owner/repo` name.
    pub name: String,
    /// GitHub `owner/repository` the index was read from.
    pub remote: String,
    #[serde(default)]
    pub formulae: Vec<String>,
    #[serde(default)]
    pub casks: Vec<String>,
    /// Unix timestamp of the last index refresh.
    pub updated_at: i64,
}

impl TapIndex {
    /// Builds an index from the file paths of a tap repository, following
    /// the layouts the raw formula fetcher understands.
    pub fn from_repo_paths<'a>(
        name: &TapName,
        remote: String,
        paths: impl IntoIterator<Item = &'a str>,
        updated_at: i64,
    ) -> Self {
        let mut formulae = Vec::new();
        let mut root_rb = Vec::new();
        let mut casks = Vec::new();
        let mut has_formula_dir = false;

        for path in paths {
            let segments: Vec<&str> = path.split('/').collect();
            let (dir, rest) = match segments.as_slice() {
                [file] => {
                    if let Some(token) = rb_token(file) {
                        root_rb.push(token);
                    }
                    continue;
                }
                [dir, rest @ ..] => (*dir, rest),
                [] => continue,
            };

            let file = match rest {
                [file] => *file,
                [shard, file] if shard.chars().count() == 1 => *file,
                _ => continue,
            };
            let Some(token) = rb_token(file) else {
                continue;
            };

            match dir {
                "Formula" | "HomebrewFormula" => {
                    has_formula_dir = true;
                    formulae.push(token);
                }
                "Casks" => casks.push(token),
                _ => {}
            }
        }

        // Homebrew only treats root-level Ruby files as formulae when the tap
        // has no dedicated formula directory.
        if !has_formula_dir {
            formulae.extend(root_rb);
        }

        formulae.sort();
        formulae.dedup();
        casks.sort();
        casks.dedup();

        Self {
            name: name.to_string(),
            remote,
            formulae,
            casks,
            updated_at,
        }
    }

    pub fn provides_formula(&self, token: &str) -> bool {
        self.formulae
            .binary_search_by(|f| f.as_str().cmp(token))
            .is_ok()
    }

    pub fn provides_cask(&self, token: &str) -> bool {
        self.casks
            .binary_search_by(|c| c.as_str().cmp(token))
            .is_ok()
    }
}

fn rb_token(file: &str) -> Option<String> {
    let token = file.strip_suffix(".rb")?;
    (!token.is_empty() && !token.starts_with('.')).then(|| token.to_string())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    taps: Vec<TapIndex>,
}

/// The set of taps recorded under `<root>/taps.json`.
#[derive(Debug)]
pub struct TapRegistry {
    path: PathBuf,
    taps: BTreeMap<String, TapIndex>,
}

impl TapRegistry {
    pub fn load(root: &Path) -> Result<Self, Error> {
        let path = root.join(REGISTRY_FILE);
        let taps = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let file: RegistryFile =
                    serde_json::from_str(&contents).map_err(|e| Error::FileError {
                        message: format!("failed to parse {}: {e}", path.display()),
                    })?;
                file.taps
                    .into_iter()
                    .map(|tap| (tap.name.clone(), tap))
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(Error::FileError {
                    message: format!("failed to read {}: {e}", path.display()),
                });
            }
        };
        Ok(Self { path, taps })
    }

    pub fn save(&self) -> Result<(), Error> {
        let file = RegistryFile {
            taps: self.taps.values().cloned().collect(),
        };
        let contents = serde_json::to_string_pretty(&file).map_err(|e| Error::FileError {
            message: format!("failed to serialize tap registry: {e}"),
        })?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, contents)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| Error::FileError {
                message: format!("failed to write {}: {e}", self.path.display()),
            })
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    pub fn get(&self, name: &TapName) -> Option<&TapIndex> {
        self.taps.get(&name.to_string())
    }

    pub fn iter(&self) -> impl Iterator<Item = &TapIndex> {
        self.taps.values()
    }

    pub fn insert(&mut self, index: TapIndex) {
        self.taps.insert(index.name.clone(), index);
    }

    pub fn remove(&mut self, name: &TapName) -> Option<TapIndex> {
        self.taps.remove(&name.to_string())
    }

    /// Resolves a bare formula name (or `cask:` token) to a fully qualified
    /// tap reference when a tapped repository provides it and the core API
    /// does not. Names that are already qualified, or that no tap provides,
    /// are returned unchanged without touching the network.
    pub async fn resolve(&self, api: &ApiClient, name: &str) -> Result<String, Error> {
        let (is_cask, token) = match name.strip_prefix("cask:") {
            Some(token) => (true, token),
            None => (false, name),
        };
        if token.contains('/') {
            return Ok(name.to_string());
        }

        let providers: Vec<&str> = self
            .taps
            .values()
            .filter(|tap| {
                if is_cask {
                    tap.provides_cask(token)
                } else {
                    tap.provides_formula(token)
                }
            })
            .map(|tap| tap.name.as_str())
            .collect();
        if providers.is_empty() {
            return Ok(name.to_string());
        }

        // Core takes precedence over taps, as in Homebrew.
        let core = if is_cask {
            api.get_cask(token).await.map(|_| ())
        } else {
            api.get_formula(token).await.map(|_| ())
        };
        match core {
            Ok(()) => return Ok(name.to_string()),
            Err(Error::MissingFormula { .. }) => {}
            Err(e) => return Err(e),
        }

        let prefix = if is_cask { "cask:" } else { "" };
        match providers.as_slice() {
            [tap] => Ok(format!("{prefix}{tap}/{token}")),
            _ => Err(Error::InvalidArgument {
                message: format!(
                    "'{token}' is provided by multiple taps; use one of: {}",
                    providers
                        .iter()
                        .map(|tap| format!("{prefix}{tap}/{token}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }
    }
}

/// Returns the tap (`owner/repo`) an installed package name belongs to, if
/// it was installed from a third-party tap.
pub fn installed_tap_of(installed_name: &str) -> Option<String> {
    let name = installed_name
        .strip_prefix("cask:")
        .unwrap_or(installed_name);
    let mut parts = name.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    parts.next()?;
    TapName::parse(&format!("{owner}/{repo}"))
        .ok()
        .map(|tap| tap.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tap(name: &str) -> TapName {
        TapName::parse(name).unwrap()
    }

    fn index(name: &str, formulae: &[&str], casks: &[&str]) -> TapIndex {
        TapIndex {
            name: name.to_string(),
            remote: String::new(),
            formulae: formulae.iter().map(|s| s.to_string()).collect(),
            casks: casks.iter().map(|s| s.to_string()).collect(),
            updated_at: 0,
        }
    }

    #[test]
    fn parse_normalizes_case_and_homebrew_prefix() {
        let name = tap("HashiCorp/homebrew-Tap");
        assert_eq!(name.to_string(), "hashicorp/tap");
        assert_eq!(name.candidate_repos(), ["homebrew-tap", "tap"]);
    }

    #[test]
    fn parse_rejects_malformed_names() {
        for bad in [
            "hashicorp",
            "hashicorp/",
            "/tap",
            "a/b/c",
            "a/../b",
            "a/b c",
        ] {
            assert!(TapName::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn builtin_taps_are_detected() {
        assert!(tap("homebrew/core").is_builtin());
        assert!(tap("Homebrew/homebrew-cask").is_builtin());
        assert!(!tap("homebrew/services").is_builtin());
    }

    #[test]
    fn index_reads_supported_layouts() {
        let paths = [
            "Formula/terraform.rb",
            "Formula/p/packer.rb",
            "HomebrewFormula/vault.rb",
            "Casks/vagrant.rb",
            "Casks/b/boundary-desktop.rb",
            "README.md",
            "setup.rb",
            "Formula/nested/deep/x.rb",
            "cmd/brew-foo.rb",
        ];
        let idx = TapIndex::from_repo_paths(&tap("hashicorp/tap"), "r".into(), paths, 1);
        assert_eq!(idx.formulae, vec!["packer", "terraform", "vault"]);
        assert_eq!(idx.casks, vec!["boundary-desktop", "vagrant"]);
    }

    #[test]
    fn index_uses_root_ruby_files_without_formula_dir() {
        let paths = ["foo.rb", "bar.rb", "Casks/baz.rb"];
        let idx = TapIndex::from_repo_paths(&tap("a/b"), "r".into(), paths, 1);
        assert_eq!(idx.formulae, vec!["bar", "foo"]);
        assert_eq!(idx.casks, vec!["baz"]);
    }

    #[test]
    fn registry_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let mut registry = TapRegistry::load(tmp.path()).unwrap();
        assert!(registry.is_empty());

        registry.insert(index("hashicorp/tap", &["terraform"], &[]));
        registry.save().unwrap();

        let mut reloaded = TapRegistry::load(tmp.path()).unwrap();
        assert!(reloaded.get(&tap("hashicorp/tap")).is_some());
        assert!(reloaded.remove(&tap("hashicorp/tap")).is_some());
        reloaded.save().unwrap();
        assert!(TapRegistry::load(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn installed_tap_of_extracts_tap_names() {
        assert_eq!(
            installed_tap_of("hashicorp/tap/terraform").as_deref(),
            Some("hashicorp/tap")
        );
        assert_eq!(
            installed_tap_of("cask:example/tap/zed").as_deref(),
            Some("example/tap")
        );
        assert_eq!(installed_tap_of("wget"), None);
        assert_eq!(installed_tap_of("cask:zed"), None);
    }

    #[tokio::test]
    async fn fetch_tap_index_falls_back_to_unprefixed_repo() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/homebrew-tools/git/trees/HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/tools/git/trees/HEAD"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sha": "abc",
                "truncated": false,
                "tree": [
                    {"path": "Formula", "type": "tree"},
                    {"path": "Formula/widget.rb", "type": "blob"},
                    {"path": "Casks/gadget.rb", "type": "blob"}
                ]
            })))
            .mount(&server)
            .await;

        let api = ApiClient::new().with_github_api_base_url(server.uri());
        let index = api.fetch_tap_index(&tap("acme/tools")).await.unwrap();
        assert_eq!(index.name, "acme/tools");
        assert_eq!(index.remote, "acme/tools");
        assert_eq!(index.formulae, vec!["widget"]);
        assert_eq!(index.casks, vec!["gadget"]);
    }

    #[tokio::test]
    async fn fetch_tap_index_reports_missing_tap() {
        let server = MockServer::start().await;
        let api = ApiClient::new().with_github_api_base_url(server.uri());
        let err = api.fetch_tap_index(&tap("acme/nope")).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn resolve_leaves_unknown_and_qualified_names_untouched() {
        let tmp = TempDir::new().unwrap();
        let mut registry = TapRegistry::load(tmp.path()).unwrap();
        registry.insert(index("hashicorp/tap", &["terraform"], &[]));
        // No mocks mounted: any network call would fail the resolution.
        let api = ApiClient::with_base_url("http://127.0.0.1:9".into()).unwrap();

        assert_eq!(registry.resolve(&api, "wget").await.unwrap(), "wget");
        assert_eq!(
            registry.resolve(&api, "other/tap/terraform").await.unwrap(),
            "other/tap/terraform"
        );
    }

    #[tokio::test]
    async fn resolve_prefers_core_then_tap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula/terraform.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cask/vagrant.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/packer.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    std::fs::read_to_string(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../brew_core/fixtures/formula_foo.json"
                    ))
                    .unwrap(),
                ),
            )
            .mount(&server)
            .await;

        let api = ApiClient::with_base_url(format!("{}/formula", server.uri()))
            .unwrap()
            .with_cask_base_url(format!("{}/cask", server.uri()));
        let tmp = TempDir::new().unwrap();
        let mut registry = TapRegistry::load(tmp.path()).unwrap();
        registry.insert(index(
            "hashicorp/tap",
            &["packer", "terraform"],
            &["vagrant"],
        ));

        assert_eq!(
            registry.resolve(&api, "terraform").await.unwrap(),
            "hashicorp/tap/terraform"
        );
        assert_eq!(
            registry.resolve(&api, "cask:vagrant").await.unwrap(),
            "cask:hashicorp/tap/vagrant"
        );
        assert_eq!(registry.resolve(&api, "packer").await.unwrap(), "packer");
    }

    #[tokio::test]
    async fn resolve_errors_when_multiple_taps_provide_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula/terraform.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let api = ApiClient::with_base_url(format!("{}/formula", server.uri())).unwrap();
        let tmp = TempDir::new().unwrap();
        let mut registry = TapRegistry::load(tmp.path()).unwrap();
        registry.insert(index("hashicorp/tap", &["terraform"], &[]));
        registry.insert(index("other/tools", &["terraform"], &[]));

        let err = registry.resolve(&api, "terraform").await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("hashicorp/tap/terraform"));
        assert!(message.contains("other/tools/terraform"));
    }
}
