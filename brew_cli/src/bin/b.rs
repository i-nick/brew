use brew_cli::{
    cli::{Cli, Commands},
    commands,
    init::ensure_init,
    logging,
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

    if let Err(e) = run(cli).await {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }
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
