//! Shared "download everything first" step for commands that install
//! packages one at a time (`upgrade`, `bundle`).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brew_io::{DownloadProgressCallback, InstallProgress};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};

use crate::ui::StdUi;

/// Downloads the bottles that installing `names` will need, at most
/// `installer.download_jobs()` packages at a time, so the one-by-one installs
/// that follow find them in the cache.
///
/// Best effort: a bottle that fails here is only warned about, because its
/// install downloads it again and reports the real error in context. Does
/// nothing for fewer than two packages or bottles, where a plain install is
/// already as fast.
pub async fn prefetch_bottles(
    installer: &brew_io::Installer,
    names: &[String],
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    if names.len() < 2 {
        return Ok(());
    }

    let requests = installer.bottles_to_prefetch(names).await;
    if requests.len() < 2 {
        return Ok(());
    }

    let jobs = installer.download_jobs().min(requests.len());
    ui.heading(format!(
        "Downloading {} bottles ({} at a time)...",
        style(requests.len()).bold(),
        jobs
    ))
    .map_err(ui_error)?;

    let bar = ProgressBar::new(requests.len() as u64);
    bar.set_style(
        ProgressStyle::default_spinner()
            .template("    {spinner:.cyan} [{pos}/{len}] {wide_msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    bar.enable_steady_tick(Duration::from_millis(80));

    // Names currently transferring, shown next to the counter.
    let active = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let show_active = |bar: &ProgressBar, active: &BTreeSet<String>| {
        bar.set_message(active.iter().cloned().collect::<Vec<_>>().join(", "));
    };

    let progress: DownloadProgressCallback = {
        let bar = bar.clone();
        let active = active.clone();
        Arc::new(move |event: InstallProgress| {
            if let InstallProgress::DownloadStarted { name, .. } = event {
                let mut active = active.lock().unwrap();
                active.insert(name);
                show_active(&bar, &active);
            }
        })
    };

    let failures = installer
        .prefetch_bottles(requests, Some(progress), |outcome| {
            let mut active = active.lock().unwrap();
            active.remove(&outcome.name);
            show_active(&bar, &active);
            bar.inc(1);
        })
        .await;
    bar.finish_and_clear();

    if !failures.is_empty() {
        let names: Vec<_> = failures.iter().map(|f| f.name.as_str()).collect();
        ui.warn(format!(
            "Could not download {} yet; retrying during install",
            names.join(", ")
        ))
        .map_err(ui_error)?;
    }

    Ok(())
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
