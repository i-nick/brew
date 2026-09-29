use console::style;

pub fn execute(installer: &mut brew_io::Installer) -> Result<(), brew_core::Error> {
    let removed = installer.clear_api_cache()?;
    if removed == 0 {
        println!("{} No cached entries to clear.", style("==>").cyan().bold());
    } else {
        println!(
            "{} Cleared {} cached formula {}.",
            style("==>").cyan().bold(),
            style(removed).green().bold(),
            if removed == 1 { "entry" } else { "entries" }
        );
    }
    println!("{}", style("Run `b outdated` to check for updates.").dim());
    Ok(())
}
