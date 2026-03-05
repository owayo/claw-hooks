//! 設定管理モジュール。
//!
//! TOML設定ファイルの読み込み、バリデーション、デフォルト生成を担当。

mod service;
mod types;
mod validation;

pub use types::Config;

// 他モジュールから利用するための再エクスポート
pub use service::ConfigService;
#[allow(unused_imports)]
pub(crate) use types::{CustomFilter, HookCondition, ProjectConfig, StopHook};
pub use validation::validate;
