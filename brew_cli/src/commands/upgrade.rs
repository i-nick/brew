use console::style;

use crate::ui::StdUi;
use crate::utils::normalize_formula_name;

pub async fn execute(
    installer: &mut brew_io::Installer,
    formulas: Vec<String>,
    casks_only: bool,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    installer.clear_api_cache()?;

    let (mut outdated, warnings, skipped) = if formulas.is_empty() {
        let (outdated, warnings) = installer.check_outdated().await?;
        let filtered = if casks_only {
            outdated
                .into_iter()
                .filter(|pkg| pkg.name.starts_with("cask:"))
                .collect()
        } else {
            outdated
        };
        (filtered, warnings, Vec::new())
    } else {
        collect_selected_outdated(installer, formulas).await?
    };

    for warning in warnings {
        ui.warn(warning).map_err(ui_error)?;
    }

    outdated.sort_by(|a, b| a.name.cmp(&b.name));

    if outdated.is_empty() {
        let message = if casks_only {
            "All casks are up to date."
        } else if skipped.is_empty() {
            "All packages are up to date."
        } else {
            "All selected packages are up to date."
        };
        ui.info(message).map_err(ui_error)?;
        return Ok(());
    }

    let package_names: Vec<String> = outdated.iter().map(|pkg| pkg.name.clone()).collect();
    ui.heading(format!(
        "Upgrading {}...",
        style(package_names.join(", ")).bold()
    ))
    .map_err(ui_error)?;

    let mut upgraded_count = 0usize;
    let mut errors = Vec::new();

    for pkg in &outdated {
        let label = upgrade_label(pkg);
        ui.step_start(label).map_err(ui_error)?;

        let link = installer.is_linked(&pkg.name)? || installer.is_isolated_linked(&pkg.name)?;
        match installer
            .install_preserving_request_status(std::slice::from_ref(&pkg.name), link)
            .await
        {
            Ok(result) => {
                upgraded_count += result.installed;
                ui.step_ok().map_err(ui_error)?;
            }
            Err(err) => {
                ui.step_fail().map_err(ui_error)?;
                errors.push((pkg.name.clone(), err));
            }
        }
    }

    if !skipped.is_empty() {
        ui.note(format!("Already up to date: {}", skipped.join(", ")))
            .map_err(ui_error)?;
    }

    if errors.is_empty() {
        ui.blank_line().map_err(ui_error)?;
        ui.heading(format!(
            "Upgraded {} {}",
            style(upgraded_count).green().bold(),
            if upgraded_count == 1 {
                "package"
            } else {
                "packages"
            }
        ))
        .map_err(ui_error)?;
        return Ok(());
    }

    for (name, err) in &errors {
        ui.error(format!("Failed to upgrade {}: {}", style(name).bold(), err))
            .map_err(ui_error)?;
    }

    Err(errors.remove(0).1)
}

async fn collect_selected_outdated(
    installer: &brew_io::Installer,
    formulas: Vec<String>,
) -> Result<(Vec<brew_io::OutdatedPackage>, Vec<String>, Vec<String>), brew_core::Error> {
    let mut outdated = Vec::new();
    let mut skipped = Vec::new();

    for formula in formulas {
        let name = normalize_formula_name(&formula)?;
        match installer.is_outdated(&name).await? {
            Some(pkg) => outdated.push(pkg),
            None => skipped.push(name),
        }
    }

    Ok((outdated, Vec::new(), skipped))
}

fn upgrade_label(pkg: &brew_io::OutdatedPackage) -> String {
    if pkg.installed_version == pkg.current_version {
        format!(
            "{} {} {}",
            style(&pkg.name).bold(),
            style(&pkg.current_version).yellow(),
            style("(new build)").dim()
        )
    } else {
        format!(
            "{} {} {} {}",
            style(&pkg.name).bold(),
            style(&pkg.installed_version).red(),
            style("→").dim(),
            style(&pkg.current_version).green()
        )
    }
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::execute;
    use crate::ui::Ui;
    use brew_io::network::api::ApiClient;
    use brew_io::network::{ApiCache, CacheEntry};
    use brew_io::{BlobCache, Cellar, Database, Installer, Linker, Store};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs;
    use std::io::Write;
    use tar::Builder;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn create_bottle_tarball(formula_name: &str, version: &str) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{formula_name}/{version}/bin/{formula_name}"))
            .unwrap();
        let content = format!("#!/bin/sh\necho {formula_name} {version}");
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

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

    fn make_installer(
        root: &std::path::Path,
        prefix: &std::path::Path,
        server: &MockServer,
    ) -> Installer {
        fs::create_dir_all(root.join("db")).unwrap();
        fs::create_dir_all(root.join("cache")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", server.uri()))
            .unwrap()
            .with_tap_raw_base_url(server.uri())
            .with_cache(ApiCache::open(&root.join("cache/api.sqlite3")).unwrap());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(root).unwrap();
        let cellar = Cellar::new(root).unwrap();
        let linker = Linker::new(prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        )
    }

    #[tokio::test]
    async fn upgrade_supports_explicit_tap_formula_references() {
        let first_server = MockServer::start().await;
        let second_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let first_bottle = create_bottle_tarball("terraform", "1.10.0");
        let first_sha = sha256_hex(&first_bottle);
        let second_bottle = create_bottle_tarball("terraform", "1.11.0");
        let second_sha = sha256_hex(&second_bottle);

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb(
                &first_server.uri(),
                "1.10.0",
                &first_sha,
            )))
            .mount(&first_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/hashicorp/tap/terraform/blobs/sha256:{first_sha}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(first_bottle))
            .mount(&first_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb(
                &second_server.uri(),
                "1.11.0",
                &second_sha,
            )))
            .mount(&second_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/hashicorp/tap/terraform/blobs/sha256:{second_sha}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(second_bottle))
            .mount(&second_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");

        let mut installer = make_installer(&root, &prefix, &first_server);
        installer
            .install(&["hashicorp/tap/terraform".to_string()], true)
            .await
            .unwrap();

        let mut installer = make_installer(&root, &prefix, &second_server);
        let mut ui = Ui::new();
        execute(
            &mut installer,
            vec!["hashicorp/tap/terraform".to_string()],
            false,
            &mut ui,
        )
        .await
        .unwrap();

        let installed = installer.get_installed("hashicorp/tap/terraform").unwrap();
        assert_eq!(installed.name, "hashicorp/tap/terraform");
        assert_eq!(installed.version, "1.11.0");
    }

    #[tokio::test]
    async fn upgrade_refreshes_metadata_before_checking_outdated() {
        let first_server = MockServer::start().await;
        let second_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let first_bottle = create_bottle_tarball("jq", "1.0.0");
        let first_sha = sha256_hex(&first_bottle);
        let second_bottle = create_bottle_tarball("jq", "2.0.0");
        let second_sha = sha256_hex(&second_bottle);
        let tag = get_test_bottle_tag();

        let fresh_bulk = format!(
            r#"[{{"name":"jq","versions":{{"stable":"2.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/jq-2.0.0.{tag}.bottle.tar.gz","sha256":"{second_sha}"}}}}}}}}}}]"#,
            second_server.uri()
        );
        let stale_bulk = format!(
            r#"[{{"name":"jq","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/jq-1.0.0.{tag}.bottle.tar.gz","sha256":"{first_sha}"}}}}}}}}}}]"#,
            first_server.uri()
        );
        let first_formula = format!(
            r#"{{"name":"jq","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/jq-1.0.0.{tag}.bottle.tar.gz","sha256":"{first_sha}"}}}}}}}}}}"#,
            first_server.uri()
        );
        let second_formula = format!(
            r#"{{"name":"jq","versions":{{"stable":"2.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/jq-2.0.0.{tag}.bottle.tar.gz","sha256":"{second_sha}"}}}}}}}}}}"#,
            second_server.uri()
        );

        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(first_formula))
            .mount(&first_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/jq-1.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(first_bottle))
            .mount(&first_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fresh_bulk))
            .mount(&second_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/jq.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(second_formula))
            .mount(&second_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/jq-2.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(second_bottle))
            .mount(&second_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");

        let mut installer = make_installer(&root, &prefix, &first_server);
        installer.install(&["jq".to_string()], true).await.unwrap();

        let cache = ApiCache::open(&root.join("cache/api.sqlite3")).unwrap();
        cache
            .put(
                &format!("{}/formula.json", second_server.uri()),
                &CacheEntry {
                    etag: None,
                    last_modified: None,
                    body: stale_bulk,
                },
            )
            .unwrap();

        let mut installer = make_installer(&root, &prefix, &second_server);
        let mut ui = Ui::new();
        execute(&mut installer, Vec::new(), false, &mut ui)
            .await
            .unwrap();

        let installed = installer.get_installed("jq").unwrap();
        assert_eq!(installed.version, "2.0.0");
    }
}
