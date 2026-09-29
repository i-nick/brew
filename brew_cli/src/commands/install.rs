use brew_io::{InstallProgress, IsolatedLinkReason, LinkOutcome, ProgressCallback};
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ui::StdUi;
use crate::utils::{normalize_formula_name, suggest_missing_formula_matches};

pub async fn execute(
    installer: &mut brew_io::Installer,
    formulas: Vec<String>,
    no_link: bool,
    build_from_source: bool,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let start = Instant::now();
    ui.heading(format!(
        "Installing {}...",
        style(formulas.join(", ")).bold()
    ))
    .map_err(ui_error)?;

    let mut normalized_names = Vec::new();
    let mut cask_names = Vec::new();
    for formula in &formulas {
        let name = normalize_formula_name(formula)?;
        if name.starts_with("cask:") {
            cask_names.push(name);
        } else {
            normalized_names.push(name);
        }
    }

    let mut installed_count = 0usize;

    if !normalized_names.is_empty() {
        let plan = match installer
            .plan_with_options(&normalized_names, build_from_source)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                suggest_missing_formula_matches(installer, &e).await;
                return Err(e);
            }
        };

        ui.heading(format!(
            "Resolving dependencies ({} packages)...",
            plan.items.len()
        ))
        .map_err(ui_error)?;
        for item in &plan.items {
            ui.bullet(format!(
                "{} {}",
                style(&item.formula.name).green(),
                style(&item.formula.versions.stable).dim()
            ))
            .map_err(ui_error)?;
        }

        let multi = MultiProgress::new();
        let bars: Arc<Mutex<HashMap<String, ProgressBar>>> = Arc::new(Mutex::new(HashMap::new()));

        let download_style = ProgressStyle::default_bar()
            .template("    {prefix:<16} {bar:25.cyan/dim} {bytes:>10}/{total_bytes:<10} {eta:>6}")
            .unwrap()
            .progress_chars("━━╸");

        let spinner_style = ProgressStyle::default_spinner()
            .template("    {prefix:<16} {spinner:.cyan} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

        let done_style = ProgressStyle::default_spinner()
            .template("    {prefix:<16} {msg}")
            .unwrap();

        ui.heading("Downloading and installing formulas...")
            .map_err(ui_error)?;

        let bars_clone = bars.clone();
        let multi_clone = multi.clone();
        let download_style_clone = download_style.clone();
        let spinner_style_clone = spinner_style.clone();
        let done_style_clone = done_style.clone();

        let progress_callback: Arc<ProgressCallback> = Arc::new(Box::new(move |event| {
            let mut bars = bars_clone.lock().unwrap();
            match event {
                InstallProgress::DownloadStarted { name, total_bytes } => {
                    let pb = if let Some(total) = total_bytes {
                        let pb = multi_clone.add(ProgressBar::new(total));
                        pb.set_style(download_style_clone.clone());
                        pb
                    } else {
                        let pb = multi_clone.add(ProgressBar::new_spinner());
                        pb.set_style(spinner_style_clone.clone());
                        pb.set_message("downloading...");
                        pb.enable_steady_tick(std::time::Duration::from_millis(80));
                        pb
                    };
                    pb.set_prefix(name.clone());
                    bars.insert(name, pb);
                }
                InstallProgress::DownloadProgress {
                    name,
                    downloaded,
                    total_bytes,
                } => {
                    if let Some(pb) = bars.get(&name)
                        && total_bytes.is_some()
                    {
                        pb.set_position(downloaded);
                    }
                }
                InstallProgress::DownloadCompleted { name, total_bytes } => {
                    if let Some(pb) = bars.get(&name) {
                        if total_bytes > 0 {
                            pb.set_position(total_bytes);
                        }
                        pb.set_style(spinner_style_clone.clone());
                        pb.set_message("unpacking...");
                        pb.enable_steady_tick(std::time::Duration::from_millis(80));
                    }
                }
                InstallProgress::UnpackStarted { name } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_message("unpacking...");
                    }
                }
                InstallProgress::UnpackCompleted { name } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_message("unpacked");
                    }
                }
                InstallProgress::LinkStarted { name } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_message("linking...");
                    }
                }
                InstallProgress::LinkCompleted { name } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_message("linked");
                    }
                }
                InstallProgress::LinkIsolated { name, prefix: _ } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_message("linked executables");
                    }
                }
                InstallProgress::LinkSkipped { name, reason } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_message(format!("keg-only ({})", reason));
                    }
                }
                InstallProgress::InstallCompleted { name } => {
                    if let Some(pb) = bars.get(&name) {
                        pb.set_style(done_style_clone.clone());
                        pb.set_message(format!("{} installed", style("✓").green()));
                        pb.finish();
                    }
                }
            }
        }));

        let result_val = installer
            .execute_with_progress(plan, !no_link, Some(progress_callback))
            .await;

        {
            let bars = bars.lock().unwrap();
            for pb in bars.values() {
                if !pb.is_finished() {
                    pb.finish();
                }
            }
        }

        let result = match result_val {
            Ok(r) => r,
            Err(e) => {
                suggest_missing_formula_matches(installer, &e).await;
                return Err(e);
            }
        };
        print_link_outcomes(ui, &result.links)?;
        installed_count += result.installed;
    }

    if !cask_names.is_empty() {
        ui.heading(format!(
            "Installing casks ({} packages)...",
            cask_names.len()
        ))
        .map_err(ui_error)?;
        let result = installer.install_casks(&cask_names, !no_link).await?;
        installed_count += result.installed;
    }

    let elapsed = start.elapsed();
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Installed {} packages in {:.2}s",
        style(installed_count).green().bold(),
        elapsed.as_secs_f64()
    ))
    .map_err(ui_error)?;

    Ok(())
}

fn print_link_outcomes(ui: &mut StdUi, links: &[LinkOutcome]) -> Result<(), brew_core::Error> {
    for link in links {
        let LinkOutcome::Isolated {
            name,
            prefix: _,
            reason,
            conflicts,
            requested,
        } = link
        else {
            continue;
        };

        match reason {
            IsolatedLinkReason::KegOnly(reason) => {
                if !requested && conflicts.is_empty() {
                    continue;
                }

                ui.blank_line().map_err(ui_error)?;
                if *requested {
                    ui.warn(format!(
                        "{} is keg-only ({}) and its executables were linked into the brew bin directory.",
                        style(name).bold(),
                        reason
                    ))
                    .map_err(ui_error)?;
                } else {
                    ui.warn(format!(
                        "{} is keg-only ({}) and some executable names could not be linked into the brew bin directory.",
                        style(name).bold(),
                        reason
                    ))
                    .map_err(ui_error)?;
                }
            }
            IsolatedLinkReason::Conflict => {
                if conflicts.is_empty() {
                    continue;
                }
                ui.blank_line().map_err(ui_error)?;
                ui.warn(format!(
                    "{} could not be fully linked into the shared prefix; support files were kept isolated and non-conflicting executables were linked into the brew bin directory.",
                    style(name).bold()
                ))
                .map_err(ui_error)?;
            }
        }

        if !conflicts.is_empty() {
            ui.println("Executable names left untouched because they already exist:")
                .map_err(ui_error)?;
            for c in conflicts {
                if let Some(ref owner) = c.owned_by {
                    ui.println(format!(
                        "  {} (symlink belonging to {})",
                        c.path.display(),
                        style(owner).yellow()
                    ))
                    .map_err(ui_error)?;
                } else {
                    ui.println(format!("  {}", c.path.display()))
                        .map_err(ui_error)?;
                }
            }
        }
    }
    Ok(())
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}
