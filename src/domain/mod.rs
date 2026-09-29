//! コアビジネスロジックを含むドメイン層。
//!
//! このモジュールには以下が含まれる:
//! - フック処理用の入出力データ型
//! - Filter トレイトとその実装
//! - シェルコマンドパーサー
//! - ローテーション付きロガー
//! - フック呼び出しをまたいで残す状態の置き場所

pub mod command;
pub mod filters;
pub mod invocation;
pub mod logger;
pub mod normalize;
pub mod parser;
pub mod state_store;
#[cfg(test)]
pub mod test_helpers;
mod types;

pub use filters::FilterChain;
pub use types::{AgentProfile, Decision, HookEvent, HookInput, ToolInput};

#[allow(unused)]
pub use types::{
    BashInput, FileOperationInput, HookOutput, StopInput, StopSessionKind, SubagentInput,
};

pub use normalize::{is_noop_success_output, normalize_lint_output, truncate_output};
pub use parser::parse_shell_tokens;
