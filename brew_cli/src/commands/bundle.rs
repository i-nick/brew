use console::style;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::install;
use crate::cli::BundleCommands;
use crate::ui::StdUi;

pub async fn execute(
    installer: &mut brew_io::Installer,
    command: Option<BundleCommands>,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    match command.unwrap_or(BundleCommands::Install {
        file: PathBuf::from("Brewfile"),
        no_link: false,
    }) {
        BundleCommands::Install { file, no_link } => {
            install_from_file(installer, &file, no_link, ui).await
        }
        BundleCommands::Dump { file, force } => dump_to_file(installer, &file, force),
    }
}

async fn install_from_file(
    installer: &mut brew_io::Installer,
    manifest_path: &Path,
    no_link: bool,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let formulas = load_manifest(manifest_path)?;
    println!(
        "{} Installing {} formulas from {}...",
        style("==>").cyan().bold(),
        style(formulas.len()).green().bold(),
        manifest_path.display()
    );

    let start = Instant::now();
    for formula in formulas {
        install::execute(installer, vec![formula], no_link, false, ui).await?;
    }

    println!(
        "{} Finished installing manifest in {:.2}s",
        style("==>").cyan().bold(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn dump_to_file(
    installer: &mut brew_io::Installer,
    file_path: &Path,
    force: bool,
) -> Result<(), brew_core::Error> {
    if file_path.exists() && !force {
        return Err(brew_core::Error::FileError {
            message: format!(
                "file {} already exists (use --force to overwrite)",
                file_path.display()
            ),
        });
    }

    let installed = installer.list_installed()?;
    let mut content = String::new();
    for keg in &installed {
        if let Some(token) = keg.name.strip_prefix("cask:") {
            content.push_str(&format!("cask \"{}\"\n", token));
        } else {
            content.push_str(&format!("brew \"{}\"\n", keg.name));
        }
    }

    std::fs::write(file_path, content).map_err(|e| brew_core::Error::FileError {
        message: format!("failed to write {}: {}", file_path.display(), e),
    })?;

    println!(
        "{} Dumped {} packages to {}",
        style("==>").cyan().bold(),
        style(installed.len()).green().bold(),
        file_path.display()
    );

    Ok(())
}

fn load_manifest(path: &Path) -> Result<Vec<String>, brew_core::Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| brew_core::Error::FileError {
        message: format!("failed to read manifest {}: {}", path.display(), e),
    })?;

    let mut formulas = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for line in contents.lines() {
        // Handle inline comments by splitting on '#' and taking the first part
        let entry = line.split('#').next().unwrap_or("").trim();
        if entry.is_empty() {
            continue;
        }

        if let Some(tap) = parse_quoted_directive(entry, "tap") {
            return Err(unsupported_tap_directive_error(path, tap));
        }

        if let Some(parsed) = parse_brewfile_entry(entry)
            && seen.insert(parsed.clone())
        {
            formulas.push(parsed);
        }
    }

    if formulas.is_empty() {
        return Err(brew_core::Error::FileError {
            message: format!("manifest {} did not contain any formulas", path.display()),
        });
    }

    Ok(formulas)
}

fn unsupported_tap_directive_error(path: &Path, tap: &str) -> brew_core::Error {
    brew_core::Error::FileError {
        message: format!(
            "manifest {} uses unsupported tap directive '{}'. brew does not support Brewfile tap directives; replace tapped formulas with explicit refs like brew \"owner/repo/foo\"",
            path.display(),
            tap
        ),
    }
}

fn parse_brewfile_entry(line: &str) -> Option<String> {
    if let Some(token) = parse_quoted_directive(line, "cask") {
        return Some(format!("cask:{token}"));
    }

    if let Some(formula) = parse_quoted_directive(line, "brew") {
        return Some(formula.to_string());
    }

    Some(line.to_string())
}

fn parse_quoted_directive<'a>(line: &'a str, directive: &str) -> Option<&'a str> {
    if !line.starts_with(directive) {
        return None;
    }

    let rest = line[directive.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let tail = &rest[1..];
    let end = tail.find(quote)?;
    Some(&tail[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use brew_io::network::api::ApiClient;
    use brew_io::{BlobCache, Cellar, Database, Installer, Linker, Store};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs;
    use std::io::Write;
    use tar::Builder;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn create_bottle_tarball(formula_name: &str) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{formula_name}/1.0.0/bin/{formula_name}"))
            .unwrap();
        header.set_size(20);
        header.set_mode(0o755);
        header.set_cksum();

        let content = format!("#!/bin/sh\necho {formula_name}");
        builder.append(&header, content.as_bytes()).unwrap();

        let tar_data = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    fn get_test_bottle_tag() -> &'static str {
        "arm64_sonoma"
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

    #[test]
    fn load_manifest_parses_entries_ignoring_whitespace_and_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "# comment\n\njq\nwget\njq\n   git  \n# another comment"
        )
        .unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(entries, vec!["jq", "wget", "git"]);
    }

    #[test]
    fn load_manifest_handles_inline_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "jq # inline comment\nwget# no space\n  git  # with spaces  "
        )
        .unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(entries, vec!["jq", "wget", "git"]);
    }

    #[test]
    fn load_manifest_errors_when_only_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "# nothing here\n   # still nothing").unwrap();

        let err = load_manifest(file.path()).unwrap_err();
        match err {
            brew_core::Error::FileError { message } => {
                assert!(message.contains("did not contain any formulas"))
            }
            other => panic!("expected file error, got {other:?}"),
        }
    }

    #[test]
    fn load_manifest_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        let err = load_manifest(&missing).unwrap_err();
        match err {
            brew_core::Error::FileError { message } => {
                assert!(message.contains("failed to read manifest"))
            }
            other => panic!("expected file error, got {other:?}"),
        }
    }

    #[test]
    fn load_manifest_parses_brewfile_cask_and_brew_entries() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "brew \"wget\"\ncask \"docker-desktop\"\ncask \"example/tap/zed\"\n"
        )
        .unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(
            entries,
            vec!["wget", "cask:docker-desktop", "cask:example/tap/zed"]
        );
    }

    #[test]
    fn load_manifest_accepts_explicit_tap_formula_references() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "brew \"hashicorp/tap/terraform\"\n").unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(entries, vec!["hashicorp/tap/terraform"]);
    }

    #[test]
    fn load_manifest_errors_on_tap_directive() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "tap \"hashicorp/tap\"\nbrew \"terraform\"\n").unwrap();

        let err = load_manifest(file.path()).unwrap_err();
        match err {
            brew_core::Error::FileError { message } => {
                assert!(message.contains("unsupported tap directive"));
                assert!(message.contains("hashicorp/tap"));
                assert!(message.contains("brew \"owner/repo/foo\""));
            }
            other => panic!("expected file error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dump_to_file_preserves_full_tap_formula_names() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();

        let bottle = create_bottle_tarball("terraform");
        let sha = sha256_hex(&bottle);

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb(
                &mock_server.uri(),
                "1.10.0",
                &sha,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/hashicorp/tap/terraform/blobs/sha256:{sha}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
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

        installer
            .install(&["hashicorp/tap/terraform".to_string()], true)
            .await
            .unwrap();

        let dump_path = tmp.path().join("Brewfile");
        dump_to_file(&mut installer, &dump_path, true).unwrap();

        let content = fs::read_to_string(dump_path).unwrap();
        assert!(content.contains("brew \"hashicorp/tap/terraform\""));
    }

    #[test]
    fn parse_brewfile_entry_handles_brew_directive() {
        assert_eq!(parse_brewfile_entry("brew \"jq\""), Some("jq".to_string()));
        assert_eq!(
            parse_brewfile_entry("brew 'wget'"),
            Some("wget".to_string())
        );
    }

    #[test]
    fn parse_brewfile_entry_handles_cask_directive() {
        assert_eq!(
            parse_brewfile_entry("cask \"docker\""),
            Some("cask:docker".to_string())
        );
        assert_eq!(
            parse_brewfile_entry("cask \"example/tap/zed\""),
            Some("cask:example/tap/zed".to_string())
        );
    }
}
