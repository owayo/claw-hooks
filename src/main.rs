//! claw-hooks: AI coding agent hook system
//!
//! A CLI tool that integrates with AI coding agents (Claude Code, Cursor, Windsurf)
//! to filter dangerous commands, suggest safer alternatives, and execute extension-based hooks.

mod cli;
mod config;
mod domain;
mod service;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};
use config::ConfigService;
use service::HookService;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load configuration
    let config = ConfigService::load(cli.config.as_deref())?;

    // Initialize logging if debug mode
    if cli.debug || config.debug {
        domain::logger::init(&config)?;
    }

    // Execute command
    match cli.command {
        Commands::Hook { format } => {
            let service = HookService::new(config, format);
            service.run()?;
        }
        Commands::Init { path } => {
            let config_path = if let Some(p) = path {
                ConfigService::generate_at(&p)?;
                p
            } else {
                ConfigService::generate_default()?;
                ConfigService::default_path()
            };
            if !cli.quiet {
                eprintln!("Configuration file created at: {}", config_path.display());
            }
        }
        Commands::Check => {
            config::validate(&config)?;
            if !cli.quiet {
                eprintln!("Configuration is valid.");
            }
        }
        Commands::Version => {
            println!("claw-hooks {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}
