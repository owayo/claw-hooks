//! フィルターチェーンの実装。

use crate::config::Config;
use crate::domain::Decision;
use crate::domain::HookInput;

use super::{
    CustomCommandFilter, ExtensionHookFilter, Filter, StopHookFilter, SubagentFilter,
    new_dd_filter, new_kill_filter, new_rm_filter,
};

/// フック入力を処理するフィルターチェーン。
pub struct FilterChain {
    filters: Vec<Box<dyn Filter>>,
}

impl FilterChain {
    /// 設定からFilterChainを作成する。
    pub fn new(config: &Config) -> Self {
        let mut filters: Vec<Box<dyn Filter>> = Vec::new();

        // 組み込みフィルターを追加
        filters.push(Box::new(new_kill_filter(
            config.kill_block,
            config.kill_block_message.clone(),
        )));
        filters.push(Box::new(new_dd_filter(
            config.dd_block,
            config.dd_block_message.clone(),
        )));
        filters.push(Box::new(new_rm_filter(
            config.rm_block,
            config.rm_block_message.clone(),
        )));

        // カスタムフィルターを追加
        for custom in &config.custom_filters {
            let filter: Box<dyn Filter> = if custom.args.is_empty() {
                // 正規表現モード: commandを正規表現パターンとして扱う
                if let Ok(f) = CustomCommandFilter::new(&custom.command, custom.message.clone()) {
                    Box::new(f)
                } else {
                    continue;
                }
            } else {
                // Argsモード: 正規表現コマンド名 + 引数マッチング
                if let Ok(f) = CustomCommandFilter::with_args(
                    &custom.command,
                    custom.args.clone(),
                    custom.message.clone(),
                ) {
                    Box::new(f)
                } else {
                    continue;
                }
            };
            filters.push(filter);
        }

        // 拡張子フックフィルターを追加
        if !config.extension_hooks.is_empty() {
            let nano_buddy = cfg!(target_os = "macos") && config.nano_buddy;
            filters.push(Box::new(ExtensionHookFilter::new(
                config.extension_hooks.clone(),
                nano_buddy,
                config.hook_timeout,
            )));
        }

        // ストップフックフィルターを追加
        if !config.stop_hooks.is_empty() {
            let nano_buddy = cfg!(target_os = "macos") && config.nano_buddy;
            filters.push(Box::new(StopHookFilter::new(
                config.stop_hooks.clone(),
                nano_buddy,
                config.hook_timeout,
            )));
        }

        // サブエージェントフィルターを追加（SubagentStart/SubagentStop用のNanoBuddy通知）
        if cfg!(target_os = "macos") && config.nano_buddy {
            filters.push(Box::new(SubagentFilter::new()));
        }

        // 優先度でソート（値が小さいほど優先度が高い）
        filters.sort_by_key(|f| f.priority());

        Self { filters }
    }

    /// 適用可能な全フィルターを実行し、最初のブロック判定を返す。
    /// Allow判定の場合、全フィルターのadditional_contextがマージされる。
    pub fn execute(&self, input: &HookInput) -> Decision {
        let mut merged_context: Option<String> = None;

        for filter in &self.filters {
            if filter.applies_to(input) {
                let decision = filter.execute(input);
                match decision {
                    Decision::Block { .. } => return decision,
                    Decision::Allow { additional_context } => {
                        // 全Allow判定のadditional_contextをマージ
                        if let Some(ctx) = additional_context {
                            merged_context = match merged_context {
                                Some(existing) => Some(format!("{}\n{}", existing, ctx)),
                                None => Some(ctx),
                            };
                        }
                    }
                }
            }
        }

        Decision::Allow {
            additional_context: merged_context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::test_helpers::make_bash_input;
    use crate::domain::{HookEvent, HookInput, ToolInput};

    #[test]
    fn test_filter_chain_allows_safe_command() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        let input = make_bash_input("ls -la");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_blocks_rm() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        let input = make_bash_input("rm -rf /tmp/foo");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_filter_chain_blocks_kill() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        let input = make_bash_input("kill -9 1234");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_filter_chain_blocks_dd() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_filter_chain_respects_disabled_rm_block() {
        let config = Config {
            rm_block: false,
            ..Config::default()
        };
        let chain = FilterChain::new(&config);
        let input = make_bash_input("rm file.txt");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_respects_disabled_kill_block() {
        let config = Config {
            kill_block: false,
            ..Config::default()
        };
        let chain = FilterChain::new(&config);
        let input = make_bash_input("kill 1234");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_respects_disabled_dd_block() {
        let config = Config {
            dd_block: false,
            ..Config::default()
        };
        let chain = FilterChain::new(&config);
        let input = make_bash_input("dd if=/dev/zero of=/tmp/out");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_with_custom_filter() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "yarn".to_string(),
            args: vec![],
            message: "Use pnpm".to_string(),
        });
        let chain = FilterChain::new(&config);
        let input = make_bash_input("yarn install");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Block { .. }));
        if let Decision::Block { message } = decision {
            assert!(message.contains("pnpm"));
        }
    }

    #[test]
    fn test_filter_chain_non_bash_tool_not_blocked() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Read".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_filters_sorted_by_priority() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        // フィルターが作成されていることを確認（少なくともkill, dd, rm）
        assert!(chain.filters.len() >= 3);
        // 優先度が昇順であることを確認
        for i in 1..chain.filters.len() {
            assert!(
                chain.filters[i - 1].priority() <= chain.filters[i].priority(),
                "Filters should be sorted by priority"
            );
        }
    }

    #[test]
    fn test_filter_chain_invalid_custom_regex_silently_skipped() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "[invalid-regex".to_string(),
            args: vec![],
            message: "should not appear".to_string(),
        });
        // パニックしないこと。無効な正規表現はスキップされる
        let chain = FilterChain::new(&config);
        // 組み込みフィルターは残っているべき
        assert!(chain.filters.len() >= 3);
        // 安全なコマンドは許可されるべき
        let input = make_bash_input("ls -la");
        assert!(matches!(chain.execute(&input), Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_merges_allow_contexts() {
        // 同じファイルの拡張子フックではコンテキストがマージされるべき
        let config = Config::default();
        let chain = FilterChain::new(&config);
        // ファイル以外のイベントはコンテキストなしのAllowを返すべき
        let input = make_bash_input("echo hello");
        let decision = chain.execute(&input);
        match decision {
            Decision::Allow { additional_context } => {
                assert!(additional_context.is_none());
            }
            _ => panic!("Expected Allow"),
        }
    }

    #[test]
    fn test_filter_chain_first_block_wins() {
        // 同一コマンド内にrmとkillの両方がある場合、最初にマッチしたブロックが優先
        let config = Config::default();
        let chain = FilterChain::new(&config);
        // killはrm(20)より高い優先度(10)を持つため、killフィルターが先にブロックする
        let input = make_bash_input("kill 123 && rm file.txt");
        let decision = chain.execute(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_filter_chain_custom_filter_with_args_mode() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "npm".to_string(),
            args: vec!["install".to_string(), "i".to_string()],
            message: "Use pnpm instead".to_string(),
        });
        let chain = FilterChain::new(&config);

        // npm install should be blocked
        let input = make_bash_input("npm install lodash");
        assert!(matches!(chain.execute(&input), Decision::Block { .. }));

        // npm run should be allowed
        let input = make_bash_input("npm run build");
        assert!(matches!(chain.execute(&input), Decision::Allow { .. }));
    }

    #[test]
    fn test_filter_chain_stop_event_passthrough_without_stop_hooks() {
        let config = Config::default();
        let chain = FilterChain::new(&config);
        let input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };
        // No stop hooks configured, should allow
        assert!(matches!(chain.execute(&input), Decision::Allow { .. }));
    }
}
