mod action;
mod config;
mod engine;
mod keys;
mod platform;
mod service;
#[cfg(feature = "self-update")]
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
    /// Run kagi in the background from login onward.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Request the OS permissions kagi needs, and report what is missing.
    ///
    /// On macOS this is also what puts kagi into the Accessibility and Input
    /// Monitoring lists, so it can be ticked at all.
    Permissions {
        /// First clear the existing grants, then ask again.
        ///
        /// macOS ties a grant to the binary's signature, so a rebuilt kagi
        /// leaves a dead row that ticking does nothing for. `tccutil` cannot
        /// target an unbundled CLI, so this clears **every application's**
        /// Accessibility and Input Monitoring grant — the scripted equivalent
        /// of pressing `−` on the whole list.
        #[arg(long)]
        reset: bool,
    },
    /// Update kagi to the latest release.
    #[cfg(feature = "self-update")]
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

#[derive(Subcommand)]
enum ServiceAction {
    /// Register the login service and start it.
    ///
    /// launchd agent on macOS, systemd user unit on Linux, logon task on
    /// Windows. Always per-user; never needs root.
    Install,
    /// Stop the service and remove its registration.
    Uninstall,
    /// Show whether it is registered and running.
    Status,
    /// Start (or restart) the registered service.
    Start,
    /// Stop the running service without unregistering it.
    Stop,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Updating must work even when the config is missing or broken — that is
    // often the reason to update in the first place.
    #[cfg(feature = "self-update")]
    if let Some(Command::Update {
        check,
        yes,
        non_interactive,
    }) = cli.command
    {
        return updater::run_self_update(yes, check, non_interactive);
    }

    // Only `install` needs a config, and it wants one that actually compiles:
    // a service that dies on startup fails where nobody is watching.
    if let Some(Command::Service { action }) = &cli.command {
        return match action {
            ServiceAction::Install => {
                let path = match &cli.config {
                    Some(p) => p.clone(),
                    None => config::default_path()?,
                };
                Config::load(&path)?
                    .rules_for(std::env::consts::OS)
                    .with_context(|| format!("compiling {}", path.display()))?;
                service::install(cli.config.as_deref())?;
                // The service will fail until the OS grants capture, so ask
                // now, while a foreground process can still show the prompt.
                platform::request_permissions().map(|_| ())
            }
            ServiceAction::Uninstall => service::uninstall(),
            ServiceAction::Status => service::status(),
            ServiceAction::Start => service::start(),
            ServiceAction::Stop => service::stop(),
        };
    }

    if let Some(Command::Permissions { reset }) = cli.command {
        if reset {
            platform::reset_permissions()?;
        }
        return platform::request_permissions().map(|_| ());
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
            println!(
                "{}: {} rule(s) for {}",
                path.display(),
                rules.len(),
                std::env::consts::OS
            );
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
                    rule.actions
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
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
            #[cfg(feature = "self-update")]
            updater::spawn_auto_update();
            platform::run(Engine::new(rules), &cfg)
        }
        // Handled before the config is loaded.
        Command::Service { .. } | Command::Permissions { .. } => unreachable!("dispatched above"),
        #[cfg(feature = "self-update")]
        Command::Update { .. } => unreachable!("dispatched above"),
    }
}
