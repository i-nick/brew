use crate::{Error, Formula};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedBottle {
    pub tag: String,
    pub url: String,
    pub sha256: String,
}

/// macOS codenames that can run on Apple Silicon, newest first.
/// Big Sur (11) was the first Apple Silicon release, so this list covers
/// every arm64 bottle tag Homebrew has ever published.
const MACOS_CODENAMES_NEWEST_FIRST: &[&str] = &[
    "golden_gate",
    "tahoe",
    "sequoia",
    "sonoma",
    "ventura",
    "monterey",
    "big_sur",
];

pub fn macos_major_version() -> Option<u32> {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    version.trim().split('.').next()?.parse().ok()
}

fn codename_for_major(major: u32) -> Option<&'static str> {
    match major {
        // Future macOS releases run bottles for every older release; treat
        // them as the newest codename we know about.
        27.. => Some("golden_gate"),
        26 => Some("tahoe"),
        15 => Some("sequoia"),
        14 => Some("sonoma"),
        13 => Some("ventura"),
        12 => Some("monterey"),
        11 => Some("big_sur"),
        _ => None,
    }
}

/// Codenames whose bottles run on the given macOS major version (a bottle
/// built for an older macOS runs on newer releases), newest first.
pub fn compatible_codenames(major_version: Option<u32>) -> Vec<&'static str> {
    let Some(pos) = major_version
        .and_then(codename_for_major)
        .and_then(|c| MACOS_CODENAMES_NEWEST_FIRST.iter().position(|&t| t == c))
    else {
        // Unknown macOS version: accept any bottle rather than failing.
        return MACOS_CODENAMES_NEWEST_FIRST.to_vec();
    };

    MACOS_CODENAMES_NEWEST_FIRST[pos..].to_vec()
}

/// Select the best Apple Silicon bottle for the running macOS version.
///
/// Preference order: the newest compatible `arm64_*` tag, then the
/// architecture-independent `all` tag. Anything else (Intel macOS or Linux
/// tags) cannot run on Apple Silicon and is reported via
/// [`Error::UnsupportedBottle`] together with the tags that were available.
pub fn select_bottle(formula: &Formula) -> Result<SelectedBottle, Error> {
    select_bottle_with_version(formula, macos_major_version())
}

fn select_bottle_with_version(
    formula: &Formula,
    macos_version: Option<u32>,
) -> Result<SelectedBottle, Error> {
    let files = &formula.bottle.stable.files;

    for codename in compatible_codenames(macos_version) {
        let tag = format!("arm64_{codename}");
        if let Some(file) = files.get(tag.as_str()) {
            return Ok(SelectedBottle {
                tag,
                url: file.url.clone(),
                sha256: file.sha256.clone(),
            });
        }
    }

    if let Some(file) = files.get("all") {
        return Ok(SelectedBottle {
            tag: "all".to_string(),
            url: file.url.clone(),
            sha256: file.sha256.clone(),
        });
    }

    Err(Error::UnsupportedBottle {
        name: formula.name.clone(),
        available: files.keys().cloned().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::types::{Bottle, BottleFile, BottleStable, KegOnly, Versions};
    use std::collections::BTreeMap;

    fn formula_with_files(name: &str, files: BTreeMap<String, BottleFile>) -> Formula {
        Formula {
            name: name.to_string(),
            versions: Versions {
                stable: "1.0.0".to_string(),
            },
            dependencies: Vec::new(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
        }
    }

    fn bottle_file(url: &str, sha256: &str) -> BottleFile {
        BottleFile {
            url: url.to_string(),
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn selects_arm64_bottle() {
        let fixture = include_str!("../../fixtures/formula_foo.json");
        let formula: Formula = serde_json::from_str(fixture).unwrap();

        let selected = select_bottle_with_version(&formula, Some(14)).unwrap();

        assert_eq!(selected.tag, "arm64_sonoma");
        assert_eq!(
            selected.url,
            "https://example.com/foo-1.2.3.arm64_sonoma.bottle.tar.gz"
        );
        assert_eq!(
            selected.sha256,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        let selected = select_bottle_with_version(&formula, Some(27)).unwrap();

        assert_eq!(selected.tag, "arm64_golden_gate");
        assert_eq!(
            selected.url,
            "https://example.com/foo-1.2.3.arm64_golden_gate.bottle.tar.gz"
        );
    }

    #[test]
    fn selects_all_bottle_for_universal_packages() {
        let mut files = BTreeMap::new();
        files.insert(
            "all".to_string(),
            bottle_file(
                "https://ghcr.io/v2/homebrew/core/ca-certificates/blobs/sha256:abc123",
                "abc123",
            ),
        );

        let formula = formula_with_files("ca-certificates", files);

        let selected = select_bottle_with_version(&formula, Some(15)).unwrap();
        assert_eq!(selected.tag, "all");
        assert!(selected.url.contains("ca-certificates"));
    }

    #[test]
    fn errors_when_only_intel_bottle_exists() {
        let mut files = BTreeMap::new();
        files.insert(
            "sonoma".to_string(),
            bottle_file("https://example.com/legacy.tar.gz", &"c".repeat(64)),
        );

        let formula = formula_with_files("legacy", files);

        let err = select_bottle_with_version(&formula, Some(14)).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedBottle { ref name, ref available }
                if name == "legacy" && available == &vec!["sonoma".to_string()]
        ));
        let message = err.to_string();
        assert!(message.contains("legacy"), "got: {message}");
        assert!(message.contains("Apple Silicon"), "got: {message}");
        assert!(message.contains("sonoma"), "got: {message}");
    }

    #[test]
    fn errors_when_only_linux_bottle_exists() {
        let mut files = BTreeMap::new();
        files.insert(
            "x86_64_linux".to_string(),
            bottle_file("https://example.com/linux.tar.gz", &"b".repeat(64)),
        );

        let formula = formula_with_files("linuxonly", files);

        let err = select_bottle_with_version(&formula, Some(15)).unwrap_err();
        assert!(matches!(err, Error::UnsupportedBottle { .. }));
        assert!(err.to_string().contains("x86_64_linux"));
    }

    #[test]
    fn errors_when_no_bottles_at_all() {
        let formula = formula_with_files("nobottle", BTreeMap::new());

        let err = select_bottle_with_version(&formula, Some(15)).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedBottle { ref available, .. } if available.is_empty()
        ));
        assert!(err.to_string().contains("no pre-built bottles"));
    }

    #[test]
    fn errors_when_arm64_bottle_requires_newer_macos() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            bottle_file("https://example.com/tahoe.tar.gz", &"a".repeat(64)),
        );

        let formula = formula_with_files("toonew", files);

        let err = select_bottle_with_version(&formula, Some(13)).unwrap_err();
        assert!(matches!(err, Error::UnsupportedBottle { .. }));
        let message = err.to_string();
        assert!(message.contains("newer macOS"), "got: {message}");
    }

    #[test]
    fn selects_older_arm64_bottle_on_newer_macos() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_big_sur".to_string(),
            bottle_file("https://example.com/big_sur.tar.gz", &"a".repeat(64)),
        );

        let formula = formula_with_files("oldie", files);

        let selected = select_bottle_with_version(&formula, Some(26)).unwrap();
        assert_eq!(selected.tag, "arm64_big_sur");
    }

    #[test]
    fn compatible_codenames_on_sequoia_excludes_tahoe() {
        let codenames = compatible_codenames(Some(15));
        assert_eq!(
            codenames,
            vec!["sequoia", "sonoma", "ventura", "monterey", "big_sur"]
        );
    }

    #[test]
    fn compatible_codenames_on_tahoe_excludes_golden_gate() {
        let codenames = compatible_codenames(Some(26));
        assert_eq!(
            codenames,
            vec![
                "tahoe", "sequoia", "sonoma", "ventura", "monterey", "big_sur"
            ]
        );
    }

    #[test]
    fn compatible_codenames_on_golden_gate_includes_all() {
        let codenames = compatible_codenames(Some(27));
        assert_eq!(codenames, MACOS_CODENAMES_NEWEST_FIRST);
    }

    #[test]
    fn compatible_codenames_on_ventura_excludes_newer() {
        let codenames = compatible_codenames(Some(13));
        assert_eq!(codenames, vec!["ventura", "monterey", "big_sur"]);
    }

    #[test]
    fn compatible_codenames_future_version_returns_all() {
        let codenames = compatible_codenames(Some(99));
        assert_eq!(codenames, MACOS_CODENAMES_NEWEST_FIRST);
    }

    #[test]
    fn compatible_codenames_none_returns_all() {
        let codenames = compatible_codenames(None);
        assert_eq!(codenames, MACOS_CODENAMES_NEWEST_FIRST);
    }

    #[test]
    fn sequoia_user_skips_tahoe_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            bottle_file("https://example.com/tahoe.tar.gz", &"aaaa".repeat(16)),
        );
        files.insert(
            "arm64_sequoia".to_string(),
            bottle_file("https://example.com/sequoia.tar.gz", &"bbbb".repeat(16)),
        );

        let formula = formula_with_files("libpq", files);

        let selected = select_bottle_with_version(&formula, Some(15)).unwrap();
        assert_eq!(selected.tag, "arm64_sequoia");
    }

    #[test]
    fn tahoe_user_picks_tahoe_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            bottle_file("https://example.com/tahoe.tar.gz", &"aaaa".repeat(16)),
        );
        files.insert(
            "arm64_sequoia".to_string(),
            bottle_file("https://example.com/sequoia.tar.gz", &"bbbb".repeat(16)),
        );

        let formula = formula_with_files("libpq", files);

        let selected = select_bottle_with_version(&formula, Some(26)).unwrap();
        assert_eq!(selected.tag, "arm64_tahoe");
    }

    #[test]
    fn tahoe_user_skips_golden_gate_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_golden_gate".to_string(),
            bottle_file("https://example.com/golden_gate.tar.gz", &"aaaa".repeat(16)),
        );
        files.insert(
            "arm64_tahoe".to_string(),
            bottle_file("https://example.com/tahoe.tar.gz", &"bbbb".repeat(16)),
        );

        let formula = formula_with_files("libpq", files);

        let selected = select_bottle_with_version(&formula, Some(26)).unwrap();
        assert_eq!(selected.tag, "arm64_tahoe");
    }

    #[test]
    fn golden_gate_user_picks_golden_gate_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_golden_gate".to_string(),
            bottle_file("https://example.com/golden_gate.tar.gz", &"aaaa".repeat(16)),
        );
        files.insert(
            "arm64_tahoe".to_string(),
            bottle_file("https://example.com/tahoe.tar.gz", &"bbbb".repeat(16)),
        );

        let formula = formula_with_files("libpq", files);

        let selected = select_bottle_with_version(&formula, Some(27)).unwrap();
        assert_eq!(selected.tag, "arm64_golden_gate");
    }
}
