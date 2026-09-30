use brew_io::Installer;
use console::style;
use std::path::PathBuf;

/// Force a name to be interpreted as a cask (used by `install --cask`).
pub fn as_cask_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.starts_with("cask:") {
        return trimmed.to_string();
    }
    let token = trimmed.strip_prefix("homebrew/cask/").unwrap_or(trimmed);
    format!("cask:{token}")
}

pub fn normalize_formula_name(name: &str) -> Result<String, brew_core::Error> {
    let trimmed = name.trim();
    if let Some(token) = trimmed.strip_prefix("cask:") {
        if token.is_empty() {
            return Err(brew_core::Error::InvalidArgument {
                message: "cask token cannot be empty".to_string(),
            });
        }
        return Ok(trimmed.to_string());
    }

    if let Some((tap, formula)) = trimmed.rsplit_once('/') {
        if formula.is_empty() {
            return Err(brew_core::Error::MissingFormula {
                name: trimmed.to_string(),
            });
        }

        if tap == "homebrew/core" {
            return Ok(formula.to_string());
        }

        if tap == "homebrew/cask" {
            return Ok(format!("cask:{formula}"));
        }

        return Ok(trimmed.to_string());
    }

    Ok(trimmed.to_string())
}

pub fn format_formula_suggestions(requested: &str, suggestions: &[String]) -> Option<String> {
    if suggestions.is_empty() {
        return None;
    }

    let mut rendered = format!(
        "{} Formula '{}' was not found. Did you mean:\n",
        style("Hint:").cyan().bold(),
        style(requested).bold()
    );

    for suggestion in suggestions {
        rendered.push_str(&format!("      {}\n", style(suggestion).green()));
    }

    if let Some(top_suggestion) = suggestions.first() {
        rendered.push_str("\n      Try installing the closest match with brew:\n");
        rendered.push_str(&format!(
            "      {}\n",
            style(format!("b install {top_suggestion}")).cyan()
        ));
    }

    Some(rendered)
}

pub fn suggest_formula_matches(requested: &str, suggestions: &[String]) {
    if let Some(message) = format_formula_suggestions(requested, suggestions) {
        eprintln!();
        eprint!("{message}");
        eprintln!();
    }
}

pub async fn suggest_missing_formula_matches(
    installer: &Installer,
    error: &brew_core::Error,
) -> bool {
    if let brew_core::Error::MissingFormula { name } = error {
        if let Ok(suggestions) = installer.suggest_formulas(name, 3).await {
            suggest_formula_matches(name, &suggestions);
        }
        return true;
    }

    false
}

pub fn get_root_path(cli_root: Option<PathBuf>) -> PathBuf {
    if let Some(root) = cli_root {
        return root;
    }

    if let Ok(env_root) = std::env::var("BREW_ROOT") {
        return PathBuf::from(env_root);
    }

    PathBuf::from("/opt/brew")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use brew_io::cellar::Cellar;
    use brew_io::network::ApiClient;
    use brew_io::storage::{BlobCache, Database, Store};
    use brew_io::{Installer, Linker};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        as_cask_name, format_formula_suggestions, normalize_formula_name,
        suggest_missing_formula_matches,
    };

    #[test]
    fn as_cask_name_prefixes_bare_and_tapped_names() {
        assert_eq!(as_cask_name("zed"), "cask:zed");
        assert_eq!(as_cask_name("cask:zed"), "cask:zed");
        assert_eq!(as_cask_name("homebrew/cask/zed"), "cask:zed");
    }

    #[test]
    fn normalize_core_tap_formula() {
        assert_eq!(
            normalize_formula_name("homebrew/core/wget").unwrap(),
            "wget".to_string()
        );
    }

    #[test]
    fn normalize_external_tap_formula_keeps_full_name() {
        assert_eq!(
            normalize_formula_name("hashicorp/tap/terraform").unwrap(),
            "hashicorp/tap/terraform".to_string()
        );
    }

    #[test]
    fn normalize_homebrew_cask_prefixes_token() {
        assert_eq!(
            normalize_formula_name("homebrew/cask/docker-desktop").unwrap(),
            "cask:docker-desktop".to_string()
        );
    }

    #[test]
    fn format_formula_suggestions_renders_list() {
        let rendered =
            format_formula_suggestions("pythn", &["python".to_string(), "pytest".to_string()])
                .unwrap();

        assert!(rendered.contains("Did you mean"));
        assert!(rendered.contains("python"));
        assert!(rendered.contains("pytest"));
        assert!(rendered.contains("b install python"));
    }

    #[test]
    fn format_formula_suggestions_returns_none_for_empty_input() {
        assert!(format_formula_suggestions("pythn", &[]).is_none());
    }

    #[tokio::test]
    async fn suggest_missing_formula_matches_fetches_related_suggestions() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name":"python"},
                    {"name":"pytest"}
                ]"#,
            ))
            .expect(1)
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

        let error = brew_core::Error::MissingFormula {
            name: "pythn".to_string(),
        };

        assert!(suggest_missing_formula_matches(&installer, &error).await);
    }

    #[tokio::test]
    async fn suggest_missing_formula_matches_returns_false_for_non_missing_errors() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::new();
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

        let error = brew_core::Error::InvalidArgument {
            message: "bad formula".to_string(),
        };

        assert!(!suggest_missing_formula_matches(&installer, &error).await);
    }
}
