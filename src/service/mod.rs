//! ビジネスロジックのオーケストレーションを含むサービス層。

mod adapter;
mod hook_service;

// ライブラリAPIとしての利用に備え未使用を許容
#[allow(unused)]
pub use adapter::FormatAdapter;

pub use hook_service::HookService;
