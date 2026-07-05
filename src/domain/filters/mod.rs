//! コマンドフィルタリングシステム。

/// 各フィルターの優先度定数。
///
/// 値が小さいほど優先度が高く、`FilterChain` は昇順で実行するため、
/// 危険コマンド系（kill/dd/rm）が先に評価され、最初のブロックが勝つ。
/// `EXTENSION` と `STOP` は意図的に同値（100）だが、両者は互いに排他的な
/// イベント（AfterFileEdit と Stop）にしか適用されないため、両者間の順序は
/// 結果に影響しない。
pub(crate) mod priority {
    /// kill/pkill/killall/taskkill
    pub(crate) const KILL: u32 = 10;
    /// dd
    pub(crate) const DD: u32 = 15;
    /// rm/rmdir/del/erase
    pub(crate) const RM: u32 = 20;
    /// ユーザー定義のカスタムフィルター
    pub(crate) const CUSTOM: u32 = 50;
    /// サブエージェント通知（観測用）
    pub(crate) const SUBAGENT: u32 = 90;
    /// 拡張子フック（AfterFileEdit のみ）。STOP と同値だがイベントが排他のため順序は無関係。
    pub(crate) const EXTENSION: u32 = 100;
    /// ストップフック（Stop のみ）。EXTENSION と同値だがイベントが排他のため順序は無関係。
    pub(crate) const STOP: u32 = 100;
}

pub mod builtin_filter;
mod chain;
mod custom_filter;
mod dd_filter;
mod extension_filter;
mod filter_trait;
mod kill_filter;
mod rm_filter;
mod stop_filter;
mod subagent_filter;

pub use chain::FilterChain;
pub use custom_filter::CustomCommandFilter;
pub use dd_filter::new_dd_filter;
pub use extension_filter::ExtensionHookFilter;
pub use filter_trait::Filter;
pub use kill_filter::new_kill_filter;
pub use rm_filter::new_rm_filter;
pub use stop_filter::StopHookFilter;
pub use subagent_filter::SubagentFilter;
