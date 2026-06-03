//! claw-hooks: AIコーディングエージェント用フックシステム
//!
//! AIコーディングエージェント (Claude Code, Cursor, Windsurf) と連携し、
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

    // 設定ファイルの読み込み
    let config = ConfigService::load(cli.config.as_deref())?;

    // デバッグモード時にロギングを初期化。
    // ガードはコマンド実行が終わるまで保持し、プロセス終了の直前に明示的に drop して
    // 非同期ログのバッファを確実にフラッシュする（drop しないと終了直前のログが欠落する）。
    let logger_guard = if cli.debug || config.debug {
        Some(domain::logger::init(&config)?)
    } else {
        None
    };

    // コマンド実行（終了コードを集約し、ログのフラッシュ後にプロセス終了する）
    let exit_code: i32 = match cli.command {
        Commands::Hook { format, trace } => {
            let service = HookService::new(config, format, trace);
            service.run()?
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
            0
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
        Commands::Version => {
            println!("claw-hooks {}", env!("CARGO_PKG_VERSION"));
            0
        }
    };

    // 非同期ログ（tracing-appender）のバッファを確実にフラッシュするため、
    // プロセス終了の直前にロガーガードを明示的に drop する。
    drop(logger_guard);
    std::process::exit(exit_code);
}
