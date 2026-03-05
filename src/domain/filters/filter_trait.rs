//! フィルタートレイト定義。

use crate::domain::{Decision, HookInput};

/// コマンドフィルターのトレイト。
pub trait Filter: Send + Sync {
    /// このフィルターが指定された入力に適用されるか判定する。
    fn applies_to(&self, input: &HookInput) -> bool;

    /// フィルターを実行し、判定結果を返す。
    fn execute(&self, input: &HookInput) -> Decision;

    /// フィルターの優先度を取得する（値が小さいほど優先度が高い）。
    fn priority(&self) -> u32;
}
