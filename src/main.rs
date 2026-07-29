//! claw-hooks: AIコーディングエージェント用フックシステム
//!
//! AIコーディングエージェント (Claude Code, Cursor, Windsurf, Antigravity, Codex) と連携し、
//! 危険なコマンドのフィルタリング、安全な代替手段の提案、拡張子ベースのフック実行を行うCLIツール。

mod cli;
mod config;
mod domain;
mod notify;
mod service;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};
use config::ConfigService;
use service::HookService;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 設定を使用しないコマンドは先に処理する。設定ファイルが壊れていても
    // `init` で再生成でき、`version` で実行ファイルの情報を確認できるようにする。
    match &cli.command {
        Commands::Init { path } => {
            let config_path = if let Some(path) = path {
                ConfigService::generate_at(path)?;
                path.clone()
            } else {
                ConfigService::generate_default()?;
                ConfigService::default_path()
            };
            if !cli.quiet {
                eprintln!("Configuration file created at: {}", config_path.display());
            }
            return Ok(());
        }
        Commands::Version => {
            println!("claw-hooks {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Commands::Hook { .. } | Commands::Check => {}
    }

    // 設定ファイルの読み込み
    let config = ConfigService::load(cli.config.as_deref())?;

    // デバッグモード時に同期ファイルロギングを初期化。
    let logger_guard = if cli.debug || config.debug {
        Some(domain::logger::init(&config)?)
    } else {
        None
    };

    // コマンド実行（フック判定の終了コードを集約する）
    let exit_code: i32 = match cli.command {
        Commands::Hook { format, trace } => {
            let service = HookService::new(config, format, trace);
            service.run()?
        }
        Commands::Check => {
            config::validate(&config)?;
            if !cli.quiet {
                eprintln!("Configuration is valid.");
                // プロジェクト設定が見つかった場合の情報表示
                if let Some(project_path) = ConfigService::find_project_config() {
                    eprintln!("Project config found: {}", project_path.display());
                    match ConfigService::load_project_config(&project_path) {
                        Ok(_) => eprintln!("Project config is valid."),
                        Err(e) => eprintln!("Project config error: {}", e),
                    }
                }
            }
            0
        }
        Commands::Init { .. } | Commands::Version => 0,
    };

    // process::exit はデストラクタを実行しないため、ローテーション worker を先に完了させる。
    drop(logger_guard);
    // Hook 固有の終了コードを維持してプロセスを終了する。
    std::process::exit(exit_code);
}
