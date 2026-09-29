use console::style;
use std::env;
use std::process::Command;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("bx - Run a command from a formula without linking it");
        eprintln!();
        eprintln!("Usage: bx <formula> [args...]");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  bx jq --version");
        eprintln!("  bx wget https://example.com");
        std::process::exit(1);
    }

    let bx_path = env::current_exe().expect("failed to get current executable path");
    let bx_dir = bx_path
        .parent()
        .expect("failed to get parent directory of bx");
    let b_path = bx_dir.join("b");

    let mut cmd = Command::new(&b_path);
    cmd.arg("run").args(&args);

    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    eprintln!("{} {}", style("error:").red().bold(), err);
    std::process::exit(1);
}
