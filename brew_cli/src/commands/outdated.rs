use console::style;

pub async fn execute(
    installer: &mut brew_io::Installer,
    quiet: bool,
    verbose: bool,
    json: bool,
) -> Result<(), brew_core::Error> {
    let (outdated, warnings) = installer.check_outdated().await?;

    // Warnings always go to stderr (never pollute stdout, especially in --json mode)
    for warning in &warnings {
        eprintln!("{} {}", style("Warning:").yellow().bold(), warning);
    }

    if json {
        let json_output = build_json_output(&outdated);
        println!("{}", serde_json::to_string_pretty(&json_output).unwrap());
        return Ok(());
    }

    if outdated.is_empty() {
        if !quiet {
            println!(
                "{} All packages are up to date.",
                style("==>").cyan().bold()
            );
        }
        return Ok(());
    }

    for pkg in &outdated {
        println!("{}", format_outdated_line(pkg, quiet, verbose));
    }

    Ok(())
}

fn build_json_output(outdated: &[brew_io::OutdatedPackage]) -> Vec<serde_json::Value> {
    outdated
        .iter()
        .map(|pkg| {
            serde_json::json!({
                "name": pkg.name,
                "installed_versions": [pkg.installed_version],
                "current_version": pkg.current_version,
            })
        })
        .collect()
}

fn format_outdated_line(pkg: &brew_io::OutdatedPackage, quiet: bool, verbose: bool) -> String {
    if quiet {
        pkg.name.clone()
    } else if verbose {
        format!(
            "{} {} {} {}",
            pkg.name,
            style(&pkg.installed_version).red(),
            style("→").dim(),
            style(&pkg.current_version).green(),
        )
    } else {
        format!(
            "{} ({}) < {}",
            pkg.name, pkg.installed_version, pkg.current_version
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{build_json_output, format_outdated_line};

    fn tap_pkg() -> brew_io::OutdatedPackage {
        brew_io::OutdatedPackage {
            name: "hashicorp/tap/terraform".to_string(),
            installed_version: "1.10.0".to_string(),
            current_version: "1.11.0".to_string(),
            installed_sha256: "old".to_string(),
            current_sha256: "new".to_string(),
            is_source_build: false,
        }
    }

    #[test]
    fn format_outdated_line_preserves_full_tap_name() {
        let rendered = format_outdated_line(&tap_pkg(), false, false);
        assert!(rendered.starts_with("hashicorp/tap/terraform"));
    }

    #[test]
    fn build_json_output_preserves_full_tap_name() {
        let json = build_json_output(&[tap_pkg()]);
        assert_eq!(json[0]["name"], "hashicorp/tap/terraform");
    }
}
