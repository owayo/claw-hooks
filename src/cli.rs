//! CLI argument parsing and command definitions.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// AI coding agent hook system for Claude Code, Cursor, and Windsurf
#[derive(Parser)]
#[command(
    name = "claw-hooks",
    version,
    about = "AI coding agent hook system for Claude Code, Cursor, and Windsurf",
    long_about = "A CLI tool that filters dangerous commands, suggests safer alternatives, \
                  and executes extension-based hooks for AI coding agents."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to configuration file
    #[arg(long, short = 'c', global = true)]
    pub config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(long, global = true)]
    pub debug: bool,

    /// Suppress non-essential output
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,
}

/// Input/output format for different AI coding agents
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Claude Code format (default)
    #[default]
    Claude,
    /// Cursor format
    Cursor,
    /// Windsurf (Cascade) format
    Windsurf,
}

/// Available subcommands
#[derive(Subcommand)]
pub enum Commands {
    /// Process hook events from stdin (alias: run)
    #[command(alias = "run")]
    Hook {
        /// Input/output format for different AI coding agents
        #[arg(long, short = 'f', default_value = "claude")]
        format: Format,
    },
    /// Generate default configuration file
    Init {
        /// Path where to create the configuration file
        #[arg(long, short = 'p')]
        path: Option<PathBuf>,
    },
    /// Validate configuration file
    Check,
    /// Display version information
    Version,
}
