//! CLI引数パースとコマンド定義。

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Claude Code, Cursor, Windsurf, Gemini CLI 向けAIコーディングエージェントフックシステム
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

    /// 設定ファイルのパス
    #[arg(long, short = 'c', global = true)]
    pub config: Option<PathBuf>,

    /// デバッグログを有効化
    #[arg(long, global = true)]
    pub debug: bool,

    /// 非必須の出力を抑制
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,
}

/// AIコーディングエージェントごとの入出力フォーマット
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Claude Code フォーマット（デフォルト）
    #[default]
    Claude,
    /// Cursor フォーマット
    Cursor,
    /// Windsurf (Cascade) フォーマット
    Windsurf,
    /// Gemini CLI フォーマット
    Gemini,
}

/// 利用可能なサブコマンド
#[derive(Subcommand)]
pub enum Commands {
    /// stdin からフックイベントを処理（エイリアス: run）
    #[command(alias = "run")]
    Hook {
        /// AIコーディングエージェントごとの入出力フォーマット
        #[arg(long, short = 'f', default_value = "claude")]
        format: Format,

        /// トレースモード: デバッグ用に生の入力を stderr に出力
        #[arg(long, short = 't')]
        trace: bool,
    },
    /// デフォルト設定ファイルを生成
    Init {
        /// 設定ファイルの作成先パス
        #[arg(long, short = 'p')]
        path: Option<PathBuf>,
    },
    /// 設定ファイルを検証
    Check,
    /// バージョン情報を表示
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
