mod action;
mod config;
mod engine;
mod keys;
mod platform;

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
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
            platform::run(Engine::new(rules), &cfg)
        }
    }
}
