//! claw-hooks: AIコーディングエージェント用フックシステム
//!
//! AIコーディングエージェント (Claude Code, Cursor, Windsurf, Antigravity, Codex, Grok) と
//! 連携し、危険なコマンドのフィルタリング、安全な代替手段の提案、
//! 拡張子ベースのフック実行を行うCLIツール。

mod cli;
mod config;
mod domain;
mod notify;
mod service;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};
use config::{Config, ConfigService};
use domain::logger::LoggingGuard;
use service::HookService;

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 設定を使用しないコマンドは先に処理する。設定ファイルが壊れていても
    // `init` で再生成でき、`version` で実行ファイルの情報を確認できるようにする。
    match &cli.command {
        Commands::Init { path } => return run_init(path.as_deref(), cli.quiet),
        Commands::Version => {
            println!("claw-hooks {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Commands::Hook { .. } | Commands::Check => {}
    }

    let config = load_config(&cli)?;
    // ロガーのガードは終了直前に drop する必要があるため、ここで保持する。
    let logger_guard = init_logging(&cli, &config);

    // コマンド実行（フック判定の終了コードを集約する）
    let exit_code: i32 = match cli.command {
        Commands::Hook {
            format,
            trace,
            ref event,
        } => run_hook(config, format, trace, event.clone()),
        Commands::Check => run_check(&config, cli.quiet)?,
        Commands::Init { .. } | Commands::Version => 0,
    };

    // process::exit はデストラクタを実行しないため、ローテーション worker を先に完了させる。
    drop(logger_guard);
    // Hook 固有の終了コードを維持してプロセスを終了する。
    std::process::exit(exit_code);
}

/// `init` サブコマンド: デフォルト設定ファイルを生成する。
fn run_init(path: Option<&std::path::Path>, quiet: bool) -> Result<()> {
    let config_path = match path {
        Some(path) => {
            ConfigService::generate_at(path)?;
            path.to_path_buf()
        }
        None => {
            ConfigService::generate_default()?;
            ConfigService::default_path()
        }
    };
    if !quiet {
        eprintln!("Configuration file created at: {}", config_path.display());
    }
    Ok(())
}

/// 設定ファイルを読み込む。
///
/// `hook` サブコマンドでは、設定エラーを `?` で伝播させてはいけない。
/// exit 1 + stdout 空は Codex / Antigravity では「フック失敗＝判定を無視」と
/// 解釈されるため、設定ファイルの TOML タイポ 1 つで危険コマンドのブロックが
/// 全て無効化されてしまう（フェイルオープン）。エージェント別の拒否応答を返して
/// そのままプロセスを終了する（ロガー未初期化なので drop すべきガードは無い）。
fn load_config(cli: &Cli) -> Result<Config> {
    match ConfigService::load(cli.config.as_deref()) {
        Ok(config) => Ok(config),
        Err(error) => {
            if let Commands::Hook { format, trace, .. } = cli.command {
                let exit_code = HookService::emit_config_error(format, trace, &error);
                std::process::exit(exit_code);
            }
            Err(error)
        }
    }
}

/// デバッグモード時に同期ファイルロギングを初期化する。
///
/// ロギングは診断用でセキュリティ制御ではないため、初期化失敗で終了してはいけない
/// （設定エラーと同じ理由で exit 1 はフェイルオープンを招く）。
/// 警告のみ出してログなしで継続する。
fn init_logging(cli: &Cli, config: &Config) -> Option<LoggingGuard> {
    if !(cli.debug || config.debug) {
        return None;
    }
    match domain::logger::init(config) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("claw-hooks failed to initialize logging: {:#}", error);
            None
        }
    }
}

/// `hook` サブコマンド: stdin のフックイベントを処理して終了コードを返す。
///
/// `HookService::run` の内部エラー（stdin の I/O 失敗、出力の書き込み失敗など）も
/// `?` で伝播させない。伝播させると exit 1 + stdout 空になり、
/// Codex / Antigravity ではフェイルオープンするため、汎用の拒否応答に倒す。
fn run_hook(config: Config, format: cli::Format, trace: bool, event: Option<String>) -> i32 {
    let service = HookService::new(config, format, trace).with_event_override(event);
    match service.run() {
        Ok(exit_code) => exit_code,
        Err(error) => HookService::emit_runtime_error(format, trace, &error),
    }
}

/// `check` サブコマンド: 設定ファイルを検証して結果を表示する。
fn run_check(config: &Config, quiet: bool) -> Result<i32> {
    config::validate(config)?;
    if !quiet {
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
    Ok(0)
}
