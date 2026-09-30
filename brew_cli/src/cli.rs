use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "b")]
#[command(about = "brew - A fast package manager for Apple Silicon Macs")]
#[command(version)]
pub struct Cli {
    #[arg(long, env = "BREW_ROOT")]
    pub root: Option<PathBuf>,

    #[arg(long, env = "BREW_PREFIX")]
    pub prefix: Option<PathBuf>,

    #[arg(
        long,
        default_value = "20",
        value_parser = parse_concurrency
    )]
    pub concurrency: usize,

    #[arg(long = "auto-init", global = true, env = "BREW_AUTO_INIT")]
    pub auto_init: bool,

    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

fn parse_concurrency(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid value '{}': expected a positive integer", value))?;
    if parsed == 0 {
        return Err("concurrency must be at least 1".to_string());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn accepts_positive_concurrency() {
        let cli = Cli::try_parse_from(["b", "--concurrency", "4", "list"]).unwrap();
        assert_eq!(cli.concurrency, 4);
    }

    #[test]
    fn rejects_zero_concurrency() {
        let result = Cli::try_parse_from(["b", "--concurrency", "0", "list"]);
        assert!(result.is_err());
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("at least 1"));
    }

    #[test]
    fn accepts_verbose_levels() {
        let cli = Cli::try_parse_from(["b", "-vv", "list"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(!cli.quiet);
    }

    #[test]
    fn rejects_quiet_with_verbose() {
        let result = Cli::try_parse_from(["b", "-v", "-q", "list"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_quiet_and_verbose_conflict() {
        let result = Cli::try_parse_from(["b", "outdated", "--quiet", "--verbose"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_quiet_and_json_conflict() {
        let result = Cli::try_parse_from(["b", "outdated", "--quiet", "--json"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_verbose_and_json_conflict() {
        let result = Cli::try_parse_from(["b", "outdated", "--verbose", "--json"]);
        assert!(result.is_err());
    }

    #[test]
    fn uninstall_accepts_remove_alias() {
        let cli = Cli::try_parse_from(["b", "remove", "jq"]).unwrap();
        assert!(matches!(cli.command, super::Commands::Uninstall { .. }));
    }

    #[test]
    fn install_accepts_i_alias() {
        let cli = Cli::try_parse_from(["b", "i", "jq"]).unwrap();
        assert!(matches!(cli.command, super::Commands::Install { .. }));
    }

    #[test]
    fn uninstall_accepts_rm_alias() {
        let cli = Cli::try_parse_from(["b", "rm", "jq"]).unwrap();
        assert!(matches!(cli.command, super::Commands::Uninstall { .. }));
    }

    #[test]
    fn list_accepts_all_flag() {
        let cli = Cli::try_parse_from(["b", "list", "--all"]).unwrap();
        assert!(matches!(cli.command, super::Commands::List { all: true }));
    }

    #[test]
    fn cleanup_accepts_dry_run_flag() {
        let cli = Cli::try_parse_from(["b", "cleanup", "--dry-run"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Cleanup { dry_run: true }
        ));
    }

    #[test]
    fn uninstall_accepts_cleanup_flag() {
        let cli = Cli::try_parse_from(["b", "uninstall", "jq", "--cleanup"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Uninstall { cleanup: true, .. }
        ));
    }

    #[test]
    fn uninstall_accepts_keep_data_flag() {
        let cli = Cli::try_parse_from(["b", "uninstall", "jq", "--keep-data"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::Uninstall {
                keep_data: true,
                ..
            }
        ));
    }

    #[test]
    fn upgrade_accepts_no_args() {
        let cli = Cli::try_parse_from(["b", "upgrade"]).unwrap();
        assert!(matches!(cli.command, super::Commands::Upgrade { .. }));
    }

    #[test]
    fn upgrade_casks_conflicts_with_explicit_packages() {
        let result = Cli::try_parse_from(["b", "upgrade", "--casks", "cask:zed"]);
        assert!(result.is_err());
    }

    #[test]
    fn self_update_accepts_check_and_version() {
        let cli = Cli::try_parse_from(["b", "self-update", "--check"]).unwrap();
        assert!(matches!(
            cli.command,
            super::Commands::SelfUpdate {
                check: true,
                target: None
            }
        ));

        let cli = Cli::try_parse_from(["b", "self-update", "0.2.1"]).unwrap();
        let super::Commands::SelfUpdate { target, .. } = cli.command else {
            panic!("expected self-update command");
        };
        assert_eq!(target.as_deref(), Some("0.2.1"));
    }

    #[test]
    fn search_accepts_single_term() {
        let cli = Cli::try_parse_from(["b", "search", "code"]).unwrap();
        assert!(matches!(cli.command, super::Commands::Search { .. }));
    }

    #[test]
    fn search_preserves_multiple_terms() {
        let cli = Cli::try_parse_from(["b", "search", "visual", "code"]).unwrap();
        let super::Commands::Search { query } = cli.command else {
            panic!("expected search command");
        };
        assert_eq!(query, vec!["visual".to_string(), "code".to_string()]);
    }
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(visible_alias = "i")]
    Install {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long)]
        no_link: bool,
        #[arg(long, short = 's')]
        build_from_source: bool,
    },
    Bundle {
        #[command(subcommand)]
        command: Option<BundleCommands>,
    },
    Upgrade {
        #[arg(conflicts_with = "casks", num_args = 0..)]
        formulas: Vec<String>,
        #[arg(long)]
        casks: bool,
    },
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
    },
    #[command(visible_aliases = ["remove", "rm"])]
    Uninstall {
        #[arg(required_unless_present = "all", num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        cleanup: bool,
        #[arg(long)]
        keep_data: bool,
    },
    Cleanup {
        #[arg(long)]
        dry_run: bool,
    },
    List {
        #[arg(long)]
        all: bool,
    },
    Info {
        formula: String,
    },
    Doctor {
        #[arg(long)]
        repair: bool,
    },
    Gc,
    Reset {
        #[arg(long, short = 'y')]
        yes: bool,
    },
    Init {
        #[arg(long)]
        no_modify_path: bool,
    },
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::shells::Shell,
    },
    #[command(disable_help_flag = true)]
    Run {
        formula: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Update,
    /// Update b and bx to the latest release
    SelfUpdate {
        /// Only report whether a newer release is available
        #[arg(long)]
        check: bool,
        /// Install this release (e.g. 0.2.1) instead of the latest; allows downgrades
        #[arg(value_name = "VERSION")]
        target: Option<String>,
    },
    Outdated {
        /// Output as JSON
        #[arg(long, conflicts_with_all = ["quiet", "verbose"])]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum BundleCommands {
    Install {
        #[arg(long, short = 'f', value_name = "FILE", default_value = "Brewfile")]
        file: PathBuf,
        #[arg(long)]
        no_link: bool,
    },
    Dump {
        #[arg(long, short = 'f', value_name = "FILE", default_value = "Brewfile")]
        file: PathBuf,
        #[arg(long)]
        force: bool,
    },
}
