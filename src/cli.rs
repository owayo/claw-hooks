//! CLI argument parsing and command definitions.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// AI coding agent hook system for Claude Code, Cursor, Windsurf, and Gemini CLI
#[derive(Parser)]
#[command(
    name = "claw-hooks",
    version,
    about = "AI coding agent hook system for Claude Code, Cursor, Windsurf, and Gemini CLI",
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
    /// Gemini CLI format
    Gemini,
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

        /// Trace mode: output raw input to stderr for debugging
        #[arg(long, short = 't')]
        trace: bool,
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn test_parse_hook_default_format() {
        let cli = Cli::try_parse_from(["claw-hooks", "hook"]).unwrap();
        match cli.command {
            Commands::Hook { format, trace } => {
                assert_eq!(format, Format::Claude);
                assert!(!trace);
            }
            _ => panic!("Expected Hook command"),
        }
    }

    #[test]
    fn test_parse_hook_cursor_format() {
        let cli = Cli::try_parse_from(["claw-hooks", "hook", "-f", "cursor"]).unwrap();
        match cli.command {
            Commands::Hook { format, .. } => assert_eq!(format, Format::Cursor),
            _ => panic!("Expected Hook command"),
        }
    }

    #[test]
    fn test_parse_hook_windsurf_format() {
        let cli = Cli::try_parse_from(["claw-hooks", "hook", "--format", "windsurf"]).unwrap();
        match cli.command {
            Commands::Hook { format, .. } => assert_eq!(format, Format::Windsurf),
            _ => panic!("Expected Hook command"),
        }
    }

    #[test]
    fn test_parse_hook_gemini_format() {
        let cli = Cli::try_parse_from(["claw-hooks", "hook", "-f", "gemini"]).unwrap();
        match cli.command {
            Commands::Hook { format, .. } => assert_eq!(format, Format::Gemini),
            _ => panic!("Expected Hook command"),
        }
    }

    #[test]
    fn test_parse_hook_trace_flag() {
        let cli = Cli::try_parse_from(["claw-hooks", "hook", "--trace"]).unwrap();
        match cli.command {
            Commands::Hook { trace, .. } => assert!(trace),
            _ => panic!("Expected Hook command"),
        }
    }

    #[test]
    fn test_parse_hook_alias_run() {
        let cli = Cli::try_parse_from(["claw-hooks", "run"]).unwrap();
        assert!(matches!(cli.command, Commands::Hook { .. }));
    }

    #[test]
    fn test_parse_init_no_path() {
        let cli = Cli::try_parse_from(["claw-hooks", "init"]).unwrap();
        match cli.command {
            Commands::Init { path } => assert!(path.is_none()),
            _ => panic!("Expected Init command"),
        }
    }

    #[test]
    fn test_parse_init_with_path() {
        let cli = Cli::try_parse_from(["claw-hooks", "init", "-p", "/tmp/config.toml"]).unwrap();
        match cli.command {
            Commands::Init { path } => {
                assert_eq!(path.unwrap(), PathBuf::from("/tmp/config.toml"));
            }
            _ => panic!("Expected Init command"),
        }
    }

    #[test]
    fn test_parse_check() {
        let cli = Cli::try_parse_from(["claw-hooks", "check"]).unwrap();
        assert!(matches!(cli.command, Commands::Check));
    }

    #[test]
    fn test_parse_version() {
        let cli = Cli::try_parse_from(["claw-hooks", "version"]).unwrap();
        assert!(matches!(cli.command, Commands::Version));
    }

    #[test]
    fn test_global_config_option() {
        let cli = Cli::try_parse_from(["claw-hooks", "-c", "/tmp/my-config.toml", "hook"]).unwrap();
        assert_eq!(cli.config.unwrap(), PathBuf::from("/tmp/my-config.toml"));
    }

    #[test]
    fn test_global_debug_flag() {
        let cli = Cli::try_parse_from(["claw-hooks", "--debug", "hook"]).unwrap();
        assert!(cli.debug);
    }

    #[test]
    fn test_global_quiet_flag() {
        let cli = Cli::try_parse_from(["claw-hooks", "-q", "hook"]).unwrap();
        assert!(cli.quiet);
    }

    #[test]
    fn test_invalid_format_rejected() {
        let result = Cli::try_parse_from(["claw-hooks", "hook", "-f", "invalid"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_no_subcommand_rejected() {
        let result = Cli::try_parse_from(["claw-hooks"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_format_default_is_claude() {
        assert_eq!(Format::default(), Format::Claude);
    }
}
