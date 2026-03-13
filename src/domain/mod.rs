//! コアビジネスロジックを含むドメイン層。
//!
//! このモジュールには以下が含まれる:
//! - フック処理用の入出力データ型
//! - Filter トレイトとその実装
//! - シェルコマンドパーサー
//! - ローテーション付きロガー

pub mod command;
pub mod filters;
pub mod logger;
pub mod normalize;
pub mod parser;
#[cfg(test)]
pub mod test_helpers;
mod types;

pub use filters::FilterChain;
pub use types::{Decision, HookEvent, HookInput, ToolInput};

#[allow(unused)]
pub use types::{BashInput, FileOperationInput, HookOutput, StopInput, SubagentInput};

pub use normalize::{normalize_lint_output, truncate_output};
pub use parser::parse_shell_tokens;
