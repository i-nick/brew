use std::process::Command;

use brew_core::Error;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskBinary {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskApp {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskZap {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCask {
    pub install_name: String,
    pub token: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub binaries: Vec<CaskBinary>,
    pub apps: Vec<CaskApp>,
    pub zap: CaskZap,
    pub has_pkg: bool,
    pub has_preflight: bool,
}

pub fn resolve_cask(token: &str, cask: &Value) -> Result<ResolvedCask, Error> {
    let mut url = required_string(cask, "url")?;
    let mut sha256 = required_string(cask, "sha256")?;
    let mut version = required_string(cask, "version")?;

    if let Some(variation) = select_platform_variation(cask) {
        if let Some(variation_version) = variation.get("version").and_then(Value::as_str) {
            version = variation_version.to_string();
        }
        if let Some(variation_url) = variation.get("url").and_then(Value::as_str) {
            url = variation_url.to_string();
        }
        if let Some(variation_sha) = variation.get("sha256").and_then(Value::as_str) {
            sha256 = variation_sha.to_string();
        }
    }

    if sha256 == "no_check" {
        return Err(Error::InvalidArgument {
            message: format!("cask '{token}' uses an unsupported checksum mode: no_check"),
        });
    }

    let binaries = parse_binary_artifacts(cask)?;
    let apps = parse_app_artifacts(cask)?;
    let zap = parse_zap(cask)?;
    let has_pkg = has_artifact_type(cask, "pkg");
    let has_preflight = has_artifact_type(cask, "preflight");

    if binaries.is_empty() && apps.is_empty() {
        let found = artifact_types(cask);
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{token}' has no supported installable artifacts (found: {found}); \
                 only casks with 'app' or 'binary' artifacts are currently supported"
            ),
        });
    }

    if binaries
        .iter()
        .any(|binary| binary.source.starts_with("$APPDIR"))
        && apps.is_empty()
    {
        let detail = if has_pkg {
            "uses pkg-installed APPDIR binaries, which are not supported yet"
        } else {
            "uses APPDIR binary artifacts but has no installable 'app' artifacts"
        };
        return Err(Error::InvalidArgument {
            message: format!("cask '{token}' {detail}"),
        });
    }

    Ok(ResolvedCask {
        install_name: format!("cask:{token}"),
        token: token.to_string(),
        version,
        url,
        sha256,
        binaries,
        apps,
        zap,
        has_pkg,
        has_preflight,
    })
}

fn required_string(value: &Value, field: &str) -> Result<String, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("failed to parse cask JSON: missing string field '{field}'"),
        })
}

fn select_platform_variation(cask: &Value) -> Option<&Value> {
    let key = current_platform_variation_key()?;
    select_platform_variation_for_key(cask, key)
}

fn select_platform_variation_for_key<'a>(cask: &'a Value, key: &str) -> Option<&'a Value> {
    cask.get("variations")?
        .get(key)
        .filter(|value| !value.is_null())
}

fn current_platform_variation_key() -> Option<&'static str> {
    current_macos_major_version().and_then(arm64_macos_variation_key_for_major)
}

fn current_macos_major_version() -> Option<u32> {
    let output = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn arm64_macos_variation_key_for_major(major: u32) -> Option<&'static str> {
    match major {
        27.. => Some("arm64_golden_gate"),
        26 => Some("arm64_tahoe"),
        15 => Some("arm64_sequoia"),
        14 => Some("arm64_sonoma"),
        13 => Some("arm64_ventura"),
        12 => Some("arm64_monterey"),
        11 => Some("arm64_big_sur"),
        _ => None,
    }
}

fn artifact_types(cask: &Value) -> String {
    let types: Vec<&str> = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_object())
        .flat_map(|obj| obj.keys())
        .map(String::as_str)
        .collect();

    if types.is_empty() {
        "none".to_string()
    } else {
        types.join(", ")
    }
}

fn has_artifact_type(cask: &Value, artifact_type: &str) -> bool {
    cask.get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|artifact| artifact.contains_key(artifact_type))
}

fn parse_binary_artifacts(cask: &Value) -> Result<Vec<CaskBinary>, Error> {
    parse_artifact_entries(cask, "binary", |entry| {
        let (source, target) = parse_binary_entry(entry)?;
        Ok(CaskBinary { source, target })
    })
}

fn parse_app_artifacts(cask: &Value) -> Result<Vec<CaskApp>, Error> {
    parse_artifact_entries(cask, "app", |entry| {
        let (source, target) = parse_app_entry(entry)?;
        Ok(CaskApp { source, target })
    })
}

fn parse_zap(cask: &Value) -> Result<CaskZap, Error> {
    let mut paths = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(raw) = artifact.get("zap") else {
            continue;
        };

        let entries = raw.as_array().ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask zap artifact shape".to_string(),
        })?;

        for entry in entries {
            let obj = entry.as_object().ok_or_else(|| Error::InvalidArgument {
                message: "unsupported cask zap entry shape".to_string(),
            })?;

            for key in ["trash", "rmdir"] {
                let Some(values) = obj.get(key) else {
                    continue;
                };

                match values {
                    Value::String(value) => paths.push(value.to_string()),
                    Value::Array(values) => {
                        for value in values {
                            let value = value.as_str().ok_or_else(|| Error::InvalidArgument {
                                message: format!("unsupported cask zap {key} entry shape"),
                            })?;
                            paths.push(value.to_string());
                        }
                    }
                    _ => {
                        return Err(Error::InvalidArgument {
                            message: format!("unsupported cask zap {key} entry shape"),
                        });
                    }
                }
            }
        }
    }

    Ok(CaskZap { paths })
}

fn parse_artifact_entries<T>(
    cask: &Value,
    artifact_type: &str,
    mut parse_entry: impl FnMut(&Value) -> Result<T, Error>,
) -> Result<Vec<T>, Error> {
    let mut parsed = Vec::new();
    let artifacts = cask
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidArgument {
            message: "failed to parse cask JSON: missing artifacts array".to_string(),
        })?;

    for artifact in artifacts {
        let Some(raw) = artifact.get(artifact_type) else {
            continue;
        };

        match raw {
            Value::String(_) => parsed.push(parse_entry(raw)?),
            Value::Array(entries) if is_direct_artifact_pair(entries) => {
                parsed.push(parse_entry(raw)?);
            }
            Value::Array(entries) => {
                for entry in entries {
                    parsed.push(parse_entry(entry)?);
                }
            }
            _ => {
                return Err(Error::InvalidArgument {
                    message: format!("unsupported cask {artifact_type} artifact shape"),
                });
            }
        }
    }

    Ok(parsed)
}

fn is_direct_artifact_pair(entries: &[Value]) -> bool {
    entries.len() == 2 && entries[0].is_string() && entries[1].is_object()
}

fn parse_binary_entry(entry: &Value) -> Result<(String, String), Error> {
    if let Some(path) = entry.as_str() {
        let target = basename(path)?;
        validate_simple_target(&target, "binary")?;
        return Ok((path.to_string(), target));
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask binary artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask binary source".to_string(),
        })?;

    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| basename(source).unwrap_or_else(|_| source.to_string()));

    validate_simple_target(&target, "binary")?;

    Ok((source.to_string(), target))
}

fn parse_app_entry(entry: &Value) -> Result<(String, String), Error> {
    if let Some(path) = entry.as_str() {
        let target = basename(path)?;
        validate_simple_target(&target, "app")?;
        validate_app_target(&target)?;
        return Ok((path.to_string(), target));
    }

    let array = entry.as_array().ok_or_else(|| Error::InvalidArgument {
        message: "unsupported cask app artifact shape".to_string(),
    })?;
    let source = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidArgument {
            message: "unsupported cask app source".to_string(),
        })?;

    let target = array
        .get(1)
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("target"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| basename(source).unwrap_or_else(|_| source.to_string()));

    validate_simple_target(&target, "app")?;
    validate_app_target(&target)?;

    Ok((source.to_string(), target))
}

fn validate_simple_target(target: &str, artifact_type: &str) -> Result<(), Error> {
    let invalid = target.is_empty()
        || target == "."
        || target == ".."
        || target.contains('/')
        || target.contains('\\')
        || target.contains('$')
        || target.contains('~')
        || std::path::Path::new(target).components().count() != 1;
    if invalid {
        return Err(Error::InvalidArgument {
            message: format!("unsupported cask {artifact_type} target path '{target}'"),
        });
    }
    Ok(())
}

fn validate_app_target(target: &str) -> Result<(), Error> {
    if !target.ends_with(".app") {
        return Err(Error::InvalidArgument {
            message: format!("unsupported cask app target path '{target}'"),
        });
    }
    Ok(())
}

fn basename(path: &str) -> Result<String, Error> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::InvalidArgument {
            message: format!("invalid cask binary path '{path}'"),
        })?;
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cask_ignores_non_apple_silicon_variations() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/darwin.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }],
            "variations": {
                "x86_64_linux": {
                    "version": "0.9.0",
                    "url": "https://example.com/linux.zip",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            }
        });

        let resolved = resolve_cask("test", &cask).unwrap();
        assert_eq!(resolved.version, "1.0.0");
        assert_eq!(resolved.url, "https://example.com/darwin.zip");
        assert_eq!(
            resolved.sha256,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn select_platform_variation_for_key_uses_exact_match_only() {
        let cask = serde_json::json!({
            "variations": {
                "arm64_big_sur": {
                    "version": "1.106.3",
                    "url": "https://example.com/big-sur.zip",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            }
        });

        assert!(select_platform_variation_for_key(&cask, "arm64_golden_gate").is_none());
        assert!(select_platform_variation_for_key(&cask, "arm64_tahoe").is_none());
        let selected = select_platform_variation_for_key(&cask, "arm64_big_sur").unwrap();
        assert_eq!(
            selected.get("version").and_then(Value::as_str),
            Some("1.106.3")
        );
    }

    #[test]
    fn resolve_cask_keeps_top_level_artifact_when_only_older_macos_variation_exists() {
        let cask = serde_json::json!({
            "token": "visual-studio-code",
            "version": "1.113.0",
            "url": "https://update.code.visualstudio.com/1.113.0/darwin-arm64/stable",
            "sha256": "9bdc19fbb7b2b936c692bf938c252b48649caf52f479ab4a0bb3748c969c8f36",
            "artifacts": [
                { "app": ["Visual Studio Code.app"] },
                { "binary": ["$APPDIR/Visual Studio Code.app/Contents/Resources/app/bin/code", {"target": "code"}] }
            ],
            "variations": {
                "arm64_big_sur": {
                    "version": "1.106.3",
                    "url": "https://update.code.visualstudio.com/1.106.3/darwin-arm64/stable",
                    "sha256": "35dd438808dde1dd1f65490ffe7713ed64102324c0809efbec0b4eb2809b218b"
                }
            }
        });

        let resolved = resolve_cask("visual-studio-code", &cask).unwrap();
        assert_eq!(resolved.version, "1.113.0");
        assert_eq!(
            resolved.url,
            "https://update.code.visualstudio.com/1.113.0/darwin-arm64/stable"
        );
        assert_eq!(
            resolved.sha256,
            "9bdc19fbb7b2b936c692bf938c252b48649caf52f479ab4a0bb3748c969c8f36"
        );
    }

    #[test]
    fn arm64_variation_key_uses_golden_gate_for_newer_macos_versions() {
        assert_eq!(arm64_macos_variation_key_for_major(26), Some("arm64_tahoe"));
        assert_eq!(
            arm64_macos_variation_key_for_major(27),
            Some("arm64_golden_gate")
        );
        assert_eq!(
            arm64_macos_variation_key_for_major(30),
            Some("arm64_golden_gate")
        );
    }

    #[test]
    fn resolve_cask_parses_binary_targets() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{
                "binary": [
                    ["bin/tool"],
                    ["bin/tool2", {"target": "tool-two"}]
                ]
            }]
        });

        let resolved = resolve_cask("test", &cask).unwrap();
        assert_eq!(resolved.binaries.len(), 2);
        assert_eq!(resolved.binaries[0].target, "tool");
        assert_eq!(resolved.binaries[1].target, "tool-two");
    }

    #[test]
    fn resolve_cask_parses_direct_binary_pair_shape() {
        let cask = serde_json::json!({
            "token": "zed",
            "version": "1.0.0",
            "url": "https://example.com/Zed.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Zed.app"] },
                { "binary": ["$APPDIR/Zed.app/Contents/MacOS/cli", {"target": "zed"}] }
            ]
        });

        let resolved = resolve_cask("zed", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 1);
        assert_eq!(resolved.apps[0].target, "Zed.app");
        assert_eq!(resolved.binaries.len(), 1);
        assert_eq!(
            resolved.binaries[0].source,
            "$APPDIR/Zed.app/Contents/MacOS/cli"
        );
        assert_eq!(resolved.binaries[0].target, "zed");
    }

    #[test]
    fn resolve_cask_parses_app_targets() {
        let cask = serde_json::json!({
            "token": "thorium",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Thorium.app", {"target": "Thorium Browser.app"}] },
                { "binary": ["thorium"] }
            ]
        });

        let resolved = resolve_cask("thorium", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 1);
        assert_eq!(resolved.apps[0].source, "Thorium.app");
        assert_eq!(resolved.apps[0].target, "Thorium Browser.app");
    }

    #[test]
    fn resolve_cask_supports_app_only_casks() {
        let cask = serde_json::json!({
            "token": "brave-browser",
            "version": "1.0.0",
            "url": "https://example.com/Brave-Browser.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Brave Browser.app"] }
            ]
        });

        let resolved = resolve_cask("brave-browser", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 1);
        assert!(resolved.binaries.is_empty());
    }

    #[test]
    fn resolve_cask_missing_required_field_is_invalid_argument() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [{ "binary": [["op"]] }]
        });

        let err = resolve_cask("test", &cask).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn resolve_cask_missing_artifacts_array_is_invalid_argument() {
        let cask = serde_json::json!({
            "token": "test",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });

        let err = resolve_cask("test", &cask).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn resolve_cask_supports_app_casks_with_ignored_extras_and_zap() {
        let cask = serde_json::json!({
            "token": "ghostty",
            "version": "1.0.0",
            "url": "https://example.com/Ghostty.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Ghostty.app"] },
                { "manpage": ["$APPDIR/Ghostty.app/Contents/Resources/man/man1/ghostty.1"] },
                { "bash_completion": ["$APPDIR/Ghostty.app/Contents/Resources/bash-completion/completions/ghostty.bash"] },
                { "zap": [{ "trash": ["~/.config/ghostty/"] }] }
            ]
        });

        let resolved = resolve_cask("ghostty", &cask).unwrap();
        assert_eq!(resolved.apps.len(), 1);
        assert!(resolved.binaries.is_empty());
        assert_eq!(resolved.zap.paths, vec!["~/.config/ghostty/".to_string()]);
    }

    #[test]
    fn resolve_cask_rejects_casks_without_supported_installable_artifacts() {
        let cask = serde_json::json!({
            "token": "ghostty",
            "version": "1.0.0",
            "url": "https://example.com/Ghostty.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "zap": [{ "trash": ["~/.config/ghostty/"] }] }
            ]
        });

        let err = resolve_cask("ghostty", &cask).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no supported installable artifacts"),
            "got: {msg}"
        );
        assert!(msg.contains("zap"), "got: {msg}");
    }

    #[test]
    fn resolve_cask_rejects_pathlike_binary_targets() {
        let cask = serde_json::json!({
            "token": "bad",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "binary": [["bin/tool", {"target": "$APPDIR/tool"}]] }
            ]
        });

        let err = resolve_cask("bad", &cask).unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported cask binary target path")
        );
    }

    #[test]
    fn resolve_cask_rejects_dot_binary_targets() {
        for target in ["", ".", ".."] {
            let cask = serde_json::json!({
                "token": "bad",
                "version": "1.0.0",
                "url": "https://example.com/test.zip",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "artifacts": [
                    { "binary": [["bin/tool", {"target": target}]] }
                ]
            });

            let err = resolve_cask("bad", &cask).unwrap_err();
            assert!(
                err.to_string()
                    .contains("unsupported cask binary target path"),
                "target {target:?} got: {err}"
            );
        }
    }

    #[test]
    fn resolve_cask_rejects_pathlike_app_targets() {
        let cask = serde_json::json!({
            "token": "bad-app",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Foo.app", {"target": "~/Applications/Foo.app"}] },
                { "binary": ["foo"] }
            ]
        });

        let err = resolve_cask("bad-app", &cask).unwrap_err();
        assert!(err.to_string().contains("unsupported cask app target path"));
    }

    #[test]
    fn resolve_cask_rejects_app_target_without_app_suffix() {
        let cask = serde_json::json!({
            "token": "bad-app",
            "version": "1.0.0",
            "url": "https://example.com/test.zip",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Foo.app", {"target": "Foo"}] }
            ]
        });

        let err = resolve_cask("bad-app", &cask).unwrap_err();
        assert!(err.to_string().contains("unsupported cask app target path"));
    }

    #[test]
    fn resolve_cask_reports_pkg_backed_appdir_casks_as_unsupported() {
        let cask = serde_json::json!({
            "token": "pkg-app",
            "version": "1.0.0",
            "url": "https://example.com/test.pkg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "pkg": ["test.pkg"] },
                { "binary": ["$APPDIR/Test.app/Contents/MacOS/test"] }
            ]
        });

        let err = resolve_cask("pkg-app", &cask).unwrap_err();
        assert!(err.to_string().contains("pkg-installed APPDIR binaries"));
    }

    #[test]
    fn resolve_cask_parses_zap_trash_and_rmdir_entries() {
        let cask = serde_json::json!({
            "token": "brave-browser",
            "version": "1.0.0",
            "url": "https://example.com/Brave-Browser.dmg",
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "artifacts": [
                { "app": ["Brave Browser.app"] },
                { "zap": [{ "trash": ["~/Library/Caches/com.brave.Browser"], "rmdir": ["~/Library/Caches/BraveSoftware"] }] }
            ]
        });

        let resolved = resolve_cask("brave-browser", &cask).unwrap();
        assert_eq!(
            resolved.zap.paths,
            vec![
                "~/Library/Caches/com.brave.Browser".to_string(),
                "~/Library/Caches/BraveSoftware".to_string()
            ]
        );
    }
}
