use std::sync::LazyLock;

use brew_core::Error;
use regex::Regex;
use serde_json::{Value, json};

use crate::network::tap_formula::{TapFormulaRef, preprocess_tap_source};

static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*version\s+["']([^"']+)["']"#).expect("VERSION_RE must compile")
});
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*url\s+["']([^"']+)["']"#).expect("URL_RE must compile"));
static SHA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*sha256\s+(?:"([0-9a-f]{64})"|:no_check)"#).expect("SHA_RE must compile")
});
static NO_CHECK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*sha256\s+:no_check\b"#).expect("NO_CHECK_RE must compile")
});
static APP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*app\s+["']([^"']+)["'](?:\s*,\s*target:\s*["']([^"']+)["'])?"#)
        .expect("APP_RE must compile")
});
static BINARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*binary\s+["']([^"']+)["'](?:\s*,\s*target:\s*["']([^"']+)["'])?"#)
        .expect("BINARY_RE must compile")
});
static PKG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*pkg\s+["']"#).expect("PKG_RE must compile"));
static PREFLIGHT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*preflight\s+do\b"#).expect("PREFLIGHT_RE must compile"));
static ZAP_ARRAY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)(trash|rmdir):\s*\[(.*?)\]"#).expect("ZAP_ARRAY_RE must compile")
});
static ZAP_STRING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(trash|rmdir):\s*["']([^"']+)["']"#).expect("ZAP_STRING_RE must compile")
});
static QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"["']([^"']+)["']"#).expect("QUOTED_RE must compile"));

pub fn parse_tap_cask_ruby(spec: &TapFormulaRef, source: &str) -> Result<Value, Error> {
    let source = preprocess_tap_source(source);
    let version = capture_required(&VERSION_RE, &source, "version", &spec.formula)?;
    let url = capture_required(&URL_RE, &source, "url", &spec.formula)?;

    if NO_CHECK_RE.is_match(&source) {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' uses an unsupported checksum mode: no_check",
                spec.formula
            ),
        });
    }

    let sha256 = capture_required(&SHA_RE, &source, "sha256", &spec.formula)?;
    let artifacts = parse_artifacts(&source);
    if artifacts.is_empty() {
        return Err(Error::InvalidArgument {
            message: format!("tap cask '{}' has no supported artifacts", spec.formula),
        });
    }

    Ok(json!({
        "token": spec.formula,
        "version": version,
        "url": url,
        "sha256": sha256,
        "artifacts": artifacts,
    }))
}

fn capture_required(
    regex: &Regex,
    source: &str,
    field: &str,
    token: &str,
) -> Result<String, Error> {
    regex
        .captures(source)
        .and_then(|capture| capture.get(1))
        .map(|matched| matched.as_str().to_string())
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("failed to parse tap cask '{token}': missing {field}"),
        })
}

fn parse_artifacts(source: &str) -> Vec<Value> {
    let mut artifacts = Vec::new();

    for capture in APP_RE.captures_iter(source) {
        let Some(path) = capture.get(1).map(|matched| matched.as_str()) else {
            continue;
        };
        if let Some(target) = capture.get(2).map(|matched| matched.as_str()) {
            artifacts.push(json!({ "app": [path, { "target": target }] }));
        } else {
            artifacts.push(json!({ "app": [path] }));
        }
    }

    for capture in BINARY_RE.captures_iter(source) {
        let Some(path) = capture.get(1).map(|matched| matched.as_str()) else {
            continue;
        };
        if let Some(target) = capture.get(2).map(|matched| matched.as_str()) {
            artifacts.push(json!({ "binary": [path, { "target": target }] }));
        } else {
            artifacts.push(json!({ "binary": [path] }));
        }
    }

    if PKG_RE.is_match(source) {
        artifacts.push(json!({ "pkg": ["__unsupported__.pkg"] }));
    }

    if PREFLIGHT_RE.is_match(source) {
        artifacts.push(json!({ "preflight": [] }));
    }

    let zap_paths = parse_zap_paths(source);
    if !zap_paths.is_empty() {
        artifacts.push(json!({ "zap": [{ "trash": zap_paths }] }));
    }

    artifacts
}

fn parse_zap_paths(source: &str) -> Vec<String> {
    let mut paths = Vec::new();

    for capture in ZAP_ARRAY_RE.captures_iter(source) {
        let Some(body) = capture.get(2).map(|matched| matched.as_str()) else {
            continue;
        };
        for quoted in QUOTED_RE.captures_iter(body) {
            if let Some(path) = quoted.get(1).map(|matched| matched.as_str()) {
                paths.push(path.to_string());
            }
        }
    }

    for capture in ZAP_STRING_RE.captures_iter(source) {
        if let Some(path) = capture.get(2).map(|matched| matched.as_str())
            && !paths.iter().any(|existing| existing == path)
        {
            paths.push(path.to_string());
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TapFormulaRef {
        TapFormulaRef {
            owner: "example".to_string(),
            repo: "tap".to_string(),
            formula: "zed".to_string(),
        }
    }

    #[test]
    fn parses_simple_tap_cask() {
        let cask = parse_tap_cask_ruby(
            &spec(),
            r#"
cask "zed" do
  version "1.0.0"
  url "https://example.com/Zed.zip"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  app "Zed.app"
end
"#,
        )
        .unwrap();

        assert_eq!(cask["token"], "zed");
        assert_eq!(cask["version"], "1.0.0");
        assert_eq!(cask["artifacts"][0]["app"][0], "Zed.app");
    }

    #[test]
    fn parses_binary_target_and_zap_paths() {
        let cask = parse_tap_cask_ruby(
            &spec(),
            r#"
cask "zed" do
  version "1.0.0"
  url "https://example.com/Zed.zip"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  app "Zed.app", target: "Zed Editor.app"
  binary "$APPDIR/Zed Editor.app/Contents/MacOS/cli", target: "zed"
  zap trash: [
    "~/Library/Application Support/Zed",
  ],
  rmdir: "~/Library/Caches/Zed"
end
"#,
        )
        .unwrap();

        assert_eq!(cask["artifacts"][0]["app"][1]["target"], "Zed Editor.app");
        assert_eq!(cask["artifacts"][1]["binary"][1]["target"], "zed");
        assert_eq!(
            cask["artifacts"][2]["zap"][0]["trash"][0],
            "~/Library/Application Support/Zed"
        );
        assert_eq!(
            cask["artifacts"][2]["zap"][0]["trash"][1],
            "~/Library/Caches/Zed"
        );
    }

    #[test]
    fn resolves_version_interpolation_and_arch_blocks() {
        let cask = parse_tap_cask_ruby(
            &spec(),
            r#"
cask "zed" do
  version "1.2.3,456"
  on_arm do
    url "https://example.com/#{version.csv.first}/#{version.csv.second}/arm.zip"
  end
  on_intel do
    url "https://example.com/#{version}/intel.zip"
  end
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  app "Zed.app"
end
"#,
        )
        .unwrap();

        assert_eq!(cask["url"], "https://example.com/1.2.3/456/arm.zip");
    }

    #[test]
    fn rejects_no_check() {
        let err = parse_tap_cask_ruby(
            &spec(),
            r#"
cask "zed" do
  version "1.0.0"
  url "https://example.com/Zed.zip"
  sha256 :no_check
  app "Zed.app"
end
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("no_check"));
    }
}
