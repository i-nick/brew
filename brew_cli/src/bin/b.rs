use brew_cli::{
    cli::{Cli, Commands},
    commands,
    init::ensure_init,
    logging, self_update,
    ui::Ui,
    utils::{as_cask_name, get_root_path},
};
use brew_io::create_installer;
use clap::Parser;
use console::style;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    logging::init(cli.verbose, cli.quiet);

    let update_check = spawn_update_check(&cli);

    if let Err(e) = run(cli).await {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }

    if let Some(check) = update_check {
        print_update_notice(check).await;
    }
}

/// Look for a newer release in the background while the command runs.
/// The network is hit at most once a day (always for `b update`).
fn spawn_update_check(cli: &Cli) -> Option<tokio::task::JoinHandle<Option<self_update::Version>>> {
    let skip_command = matches!(
        cli.command,
        Commands::SelfUpdate { .. }
            | Commands::Completion { .. }
            | Commands::Run { .. }
            | Commands::Outdated { json: true }
    );
    let disabled = std::env::var_os("BREW_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty());
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if skip_command || disabled || cli.quiet || !interactive {
        return None;
    }

    let root = get_root_path(cli.root.clone());
    let force = matches!(cli.command, Commands::Update);
    Some(tokio::spawn(async move {
        self_update::newer_release(&root, self_update::releases_url(), force).await
    }))
}

async fn print_update_notice(check: tokio::task::JoinHandle<Option<self_update::Version>>) {
    // Don't hold up a fast command for a slow network; the next run retries.
    let Ok(Ok(Some(latest))) =
        tokio::time::timeout(std::time::Duration::from_millis(500), check).await
    else {
        return;
    };
    eprintln!(
        "\n{} b {} is available (you have {}). Run {} to update.",
        style("==>").cyan().bold(),
        style(latest).green().bold(),
        self_update::CURRENT_VERSION,
        style("b self-update").bold()
    );
}

async fn run(cli: Cli) -> Result<(), brew_core::Error> {
    let mut ui = Ui::new();

    if let Commands::Completion { shell } = &cli.command {
        return commands::completion::execute(*shell);
    }

    let root = get_root_path(cli.root.clone());

    if let Commands::Search { query } = &cli.command {
        return commands::search::execute(&root, query.clone(), &mut ui).await;
    }

    if let Commands::SelfUpdate { check, target } = &cli.command {
        return commands::self_update::execute(&root, *check, target.clone(), &mut ui).await;
    }

    if let Commands::Init { no_modify_path } = &cli.command {
        let prefix = cli.prefix.clone().unwrap_or_else(|| root.clone());
        return commands::init::execute(&root, &prefix, *no_modify_path, &mut ui);
    }

    // Mach-O binaries have fixed-size path fields so the prefix must be no
    // longer than the original Homebrew prefix (/opt/homebrew = 13 chars).
    // Using root directly (/opt/brew = 9 chars) keeps us within that limit.
    let prefix = cli.prefix.unwrap_or_else(|| root.clone());

    if requires_init(&cli.command) {
        ensure_init(&root, &prefix, cli.auto_init, &mut ui)?;
    }

    let mut installer = create_installer(&root, &prefix, cli.concurrency)?;

    match cli.command {
        Commands::Init { .. } => unreachable!(),
        Commands::Completion { .. } => unreachable!(),
        Commands::Search { .. } => unreachable!(),
        Commands::SelfUpdate { .. } => unreachable!(),
        Commands::Install {
            formulas,
            cask,
            no_link,
            build_from_source,
        } => {
            let formulas = if cask {
                formulas.iter().map(|name| as_cask_name(name)).collect()
            } else {
                formulas
            };
            commands::install::execute(
                &mut installer,
                formulas,
                no_link,
                build_from_source,
                &mut ui,
            )
            .await
        }
        Commands::Bundle { command } => {
            commands::bundle::execute(&mut installer, command, &mut ui).await
        }
        Commands::Upgrade { formulas, casks } => {
            commands::upgrade::execute(&mut installer, formulas, casks, &mut ui).await
        }
        Commands::Uninstall {
            formulas,
            all,
            cleanup,
            keep_data,
        } => {
            commands::uninstall::execute(&mut installer, formulas, all, cleanup, keep_data, &mut ui)
                .await
        }
        Commands::Cleanup { dry_run } => {
            commands::cleanup::execute(&mut installer, dry_run, &mut ui).await
        }
        Commands::Doctor { repair } => commands::doctor::execute(&mut installer, repair, &mut ui),
        Commands::List { all } => commands::list::execute(&mut installer, all),
        Commands::Info { formula } => commands::info::execute(&mut installer, formula),
        Commands::Gc => commands::gc::execute(&mut installer),
        Commands::Update => commands::update::execute(&mut installer),
        Commands::Outdated { json } => {
            commands::outdated::execute(&mut installer, cli.quiet, cli.verbose > 0, json).await
        }
        Commands::Reset { yes } => commands::reset::execute(&root, &prefix, yes, &mut ui),
        Commands::Run { formula, args } => {
            commands::run::execute(&mut installer, formula, args).await
        }
    }
}

fn requires_init(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::Init { .. }
            | Commands::Completion { .. }
            | Commands::Reset { .. }
            | Commands::Search { .. }
            | Commands::SelfUpdate { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::requires_init;
    use brew_cli::cli::Commands;

    #[test]
    fn search_does_not_require_init() {
        assert!(!requires_init(&Commands::Search {
            query: vec!["code".to_string()],
        }));
    }
}
