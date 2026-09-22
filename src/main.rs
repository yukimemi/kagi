mod action;
mod config;
mod engine;
mod keys;
mod platform;
mod updater;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use engine::Engine;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "kagi",
    version,
    about = "Cross-platform key mapper with first-class IME control"
)]
struct Cli {
    /// Config path. Defaults to $KAGI_CONFIG, then ~/.config/kagi/kagi.toml.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Capture keys and apply the config (default).
    Run,
    /// Parse the config and print the rules that apply here.
    Check,
    /// Print key events as they arrive, to discover key names.
    Watch,
    /// Update kagi to the latest release.
    Update {
        /// Report whether a newer release exists, then stop.
        #[arg(long)]
        check: bool,
        /// Install without asking.
        #[arg(short, long)]
        yes: bool,
        /// Never prompt; requires `--yes` to actually install.
        #[arg(long)]
        non_interactive: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Updating must work even when the config is missing or broken — that is
    // often the reason to update in the first place.
    if let Some(Command::Update { check, yes, non_interactive }) = cli.command {
        return updater::run_self_update(yes, check, non_interactive);
    }

    let path = match cli.config {
        Some(p) => p,
        None => config::default_path()?,
    };
    let cfg = Config::load(&path)?;
    let rules = cfg
        .rules_for(std::env::consts::OS)
        .with_context(|| format!("compiling {}", path.display()))?;

    match cli.command.unwrap_or(Command::Run) {
        Command::Check => {
            println!("{}: {} rule(s) for {}", path.display(), rules.len(), std::env::consts::OS);
            for rule in &rules {
                let flags = match (rule.passthrough, rule.wildcard_mods) {
                    (true, true) => "~*",
                    (true, false) => "~",
                    (false, true) => "*",
                    (false, false) => "",
                };
                let actions = if rule.actions.is_empty() {
                    "<swallow>".to_string()
                } else {
                    rule.actions.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(" ")
                };
                print!("  {flags}{} -> {actions}", rule.trigger);
                match &rule.description {
                    Some(d) => println!("   # {d}"),
                    None => println!(),
                }
            }
            Ok(())
        }
        Command::Watch => platform::watch(&cfg),
        Command::Run => {
            if rules.is_empty() {
                eprintln!("kagi: no rules for {}; nothing to do", std::env::consts::OS);
            }
            // `run` never returns, so a "new version available" banner would
            // never print. Update silently instead; it applies next launch.
            updater::spawn_auto_update();
            platform::run(Engine::new(rules), &cfg)
        }
        // Handled before the config is loaded.
        Command::Update { .. } => unreachable!("dispatched above"),
    }
}
