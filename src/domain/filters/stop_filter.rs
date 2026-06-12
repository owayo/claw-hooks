//! Stop イベントフックフィルターの実装。

use std::process::Output;

use tracing::{debug, info, warn};

use super::Filter;
use crate::config::StopHook;
use crate::domain::command::{
    TimedOutput, run_with_timeout_tracked, spawn_detached_with_env, spawn_piped_with_env,
};
use crate::domain::{Decision, HookEvent, HookInput};

/// プロセス間の再帰的な Stop フック実行を防止する環境変数。
/// claw-hooks が Stop フックを実行する際、子プロセスにこの環境変数を設定する。
/// 子プロセス（例: git-sc → Gemini CLI）が別の claw-hooks Stop イベントをトリガーした場合、
/// この環境変数が継承され、Stop フックをスキップしてループを断ち切る。
const STOP_ACTIVE_ENV: &str = "CLAW_HOOKS_STOP_ACTIVE";

/// Stop イベントフックのフィルター。
pub struct StopHookFilter {
    hooks: Vec<StopHook>,
    nano_buddy: bool,
    timeout_secs: u64,
}

impl StopHookFilter {
    /// 新しい StopHookFilter を作成する。
    pub fn new(hooks: Vec<StopHook>, nano_buddy: bool, timeout_secs: u64) -> Self {
        Self {
            hooks,
            nano_buddy,
            timeout_secs,
        }
    }

    /// エージェントの最後のメッセージを子プロセスに渡す環境変数。
    /// git-sc などのツールがエージェントのコンテキストを使用してコミットメッセージを生成できるようにする。
    const AGENT_MESSAGE_ENV: &'static str = "CLAW_HOOKS_AGENT_MESSAGE";

    /// タイムアウト付きで単一コマンド文字列を安全に実行する。
    /// シェル対応のトークナイザーでクォートされた引数を適切に処理する。
    /// 再帰ループ防止のため子プロセスに `CLAW_HOOKS_STOP_ACTIVE=1` を設定する。
    /// 必要に応じて `CLAW_HOOKS_AGENT_MESSAGE` にエージェントの最後のメッセージを設定する。
    fn execute_command_tracked(
        command: &str,
        timeout_secs: u64,
        agent_message: Option<&str>,
    ) -> Result<TimedOutput, String> {
        let parts = crate::domain::parse_shell_tokens(command);
        if parts.is_empty() {
            return Err("Empty command".to_string());
        }

        let program = &parts[0];
        let args = &parts[1..];

        debug!("🛑 Executing stop hook: {} {:?}", program, args);

        let mut envs: Vec<(&str, &str)> = vec![(STOP_ACTIVE_ENV, "1")];
        if let Some(msg) = agent_message {
            envs.push((Self::AGENT_MESSAGE_ENV, msg));
        }

        let start = std::time::Instant::now();
        let child = spawn_piped_with_env(program, args, &envs)
            .map_err(|e| format!("Failed to execute stop hook '{}': {}", command, e))?;
        let result = run_with_timeout_tracked(child, timeout_secs, command);
        let elapsed = start.elapsed();
        info!(
            "⏰️ Stop hook [{}] completed in {:.2}s",
            command,
            elapsed.as_secs_f64()
        );
        result
    }

    /// 単一のストップフックコマンドを実行し、キャプチャしたプロセス出力を返す。
    #[cfg(test)]
    fn execute_command(
        command: &str,
        timeout_secs: u64,
        agent_message: Option<&str>,
    ) -> Result<Output, String> {
        Self::execute_command_tracked(command, timeout_secs, agent_message).map(|r| r.output)
    }

    /// fire-and-forget でコマンドを起動する（report=false 用）。
    /// stdout/stderr は破棄し、Hook 本体は子プロセスの完了を待たない。
    /// 決定にも Hook 応答時間にも影響させないための実行パス。
    fn execute_command_detached(command: &str, agent_message: Option<&str>) {
        let parts = crate::domain::parse_shell_tokens(command);
        if parts.is_empty() {
            warn!("Empty command for detached execution");
            return;
        }

        let program = &parts[0];
        let args = &parts[1..];

        debug!(
            "🚀 Executing stop hook (fire-and-forget): {} {:?}",
            program, args
        );

        let mut envs: Vec<(&str, &str)> = vec![(STOP_ACTIVE_ENV, "1")];
        if let Some(msg) = agent_message {
            envs.push((Self::AGENT_MESSAGE_ENV, msg));
        }

        match spawn_detached_with_env(program, args, &envs) {
            Ok(pid) => info!(
                "🚀 Detached stop hook [{}] started with pid={}",
                command, pid
            ),
            Err(e) => {
                warn!(
                    "❌ Failed to spawn fire-and-forget stop hook '{}': {}",
                    command, e
                );
            }
        }
    }

    /// ストップフックコマンドの stdout/stderr 出力をログに記録する。
    fn log_output(command: &str, output: &Output) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            info!("Stop hook [{}] stdout:\n{}", command, stdout.trim());
        }
        if !stderr.trim().is_empty() {
            info!("Stop hook [{}] stderr:\n{}", command, stderr.trim());
        }
    }

    /// コマンド出力（stdout + stderr）からブロック理由を構築する。
    fn build_reason(command: &str, output: &Output) -> String {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut reason = format!("Stop hook failed: {}\n", command);
        if !stdout.trim().is_empty() {
            reason.push_str(&stdout);
        }
        if !stderr.trim().is_empty() {
            if !stdout.trim().is_empty() {
                reason.push('\n');
            }
            reason.push_str(&stderr);
        }
        reason.trim().to_string()
    }

    /// ループ防止チェック。環境変数・stop_hook_active フラグ・loop_count を確認する。
    fn check_loop_prevention(input: &HookInput) -> LoopCheck {
        // クロスプロセス再帰ループ防止
        if std::env::var(STOP_ACTIVE_ENV).is_ok() {
            debug!(
                "⛔ {}=1 detected, skipping all stop hooks (cross-process loop prevention)",
                STOP_ACTIVE_ENV
            );
            return LoopCheck::Skip;
        }

        // エージェント内部フラグによるループ防止 + agent_message 抽出
        let agent_message = if let crate::domain::ToolInput::Stop(ref stop_input) = input.tool_input
        {
            if stop_input.stop_hook_active {
                debug!("🛑 stop_hook_active=true, skipping all stop hooks");
                return LoopCheck::Skip;
            }
            // Cursor は stop_hook_active を持たず、代わりに loop_count
            // （stop hook 由来の自動フォローアップ発火回数、0 始まり）を送る。
            // 1 以上なら既にフォローアップ済みなので、Claude の stop_hook_active と
            // 対称的に全 stop hook をスキップして無限ループを防ぐ。
            if let Some(count) = stop_input.loop_count {
                if count >= 1 {
                    debug!("🛑 loop_count={} (>=1), skipping all stop hooks", count);
                    return LoopCheck::Skip;
                }
            }
            stop_input.agent_message.clone()
        } else {
            None
        };

        LoopCheck::Continue(agent_message)
    }

    /// 条件チェックを通過したフックをステージ別にグループ化して収集する。
    fn collect_qualified_commands(&self) -> std::collections::BTreeMap<u8, Vec<QualifiedCommand>> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut stage_map: std::collections::BTreeMap<u8, Vec<QualifiedCommand>> =
            std::collections::BTreeMap::new();

        for hook in &self.hooks {
            if let Some(ref condition) = hook.condition {
                if !condition.is_satisfied(&cwd) {
                    debug!("Stop hook condition not met, skipping: {:?}", hook.commands);
                    continue;
                }
                debug!("Stop hook condition met, queuing: {:?}", hook.commands);
            }

            let stage = hook.stage_value();
            let report = hook.should_report();
            let entry = stage_map.entry(stage).or_default();
            for cmd in &hook.commands {
                entry.push(QualifiedCommand {
                    command: cmd.clone(),
                    report,
                });
            }
        }

        stage_map
    }

    /// 単一ステージ内のコマンドを並列実行し、失敗を収集する。
    fn execute_stage(
        stage: u8,
        commands: &[QualifiedCommand],
        timeout_secs: u64,
        agent_message: Option<&str>,
        failures: &mut Vec<String>,
    ) {
        debug!("▶ Executing stop hook stage {}", stage);

        let mut report_handles = Vec::new();

        for qc in commands {
            if !qc.report {
                Self::execute_command_detached(&qc.command, agent_message);
                continue;
            }

            let command = qc.command.clone();
            let agent_msg = agent_message.map(|s| s.to_string());
            let handle = std::thread::spawn(move || -> Option<String> {
                match Self::execute_command_tracked(&command, timeout_secs, agent_msg.as_deref()) {
                    Ok(result) => {
                        Self::log_output(&command, &result.output);
                        if result.timed_out {
                            // タイムアウトは異常終了 — 成功として扱わない
                            Some(format!(
                                "⏱ Stop hook timed out after {}s: {}",
                                timeout_secs, &command
                            ))
                        } else if result.output.status.success() {
                            None
                        } else {
                            Some(Self::build_reason(&command, &result.output))
                        }
                    }
                    Err(e) => Some(e),
                }
            });

            report_handles.push(handle);
        }

        for handle in report_handles {
            match handle.join() {
                Ok(Some(reason)) => failures.push(reason),
                Ok(None) => {}
                Err(_) => {
                    warn!("🛑 Stop hook thread panicked");
                    failures.push("Stop hook thread panicked".to_string());
                }
            }
        }
    }
}

/// ループ防止チェックの結果。
enum LoopCheck {
    /// ストップフックをスキップする。
    Skip,
    /// 続行する（オプションの agent_message 付き）。
    Continue(Option<String>),
}

/// 条件チェックを通過したコマンドと report フラグ。
struct QualifiedCommand {
    command: String,
    report: bool,
}

impl Filter for StopHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // Stop イベントにのみ適用
        input.event == HookEvent::Stop
    }

    fn execute(&self, input: &HookInput) -> Decision {
        // ループ防止チェック
        let agent_message = match Self::check_loop_prevention(input) {
            LoopCheck::Skip => return Decision::allow(),
            LoopCheck::Continue(msg) => msg,
        };

        // NanoBuddy 通知（全ストップフックの前に実行し、最初に到着させる）
        if self.nano_buddy {
            debug!("🐱 NanoBuddy stop notification");
            crate::notify::nano_buddy::notify_stop_hook();
        }

        // 条件チェックを通過したコマンドをステージ別に収集
        let stage_map = self.collect_qualified_commands();

        // ステージを順番に実行（1 → 5）、各ステージ内のコマンドは並列実行
        let mut failures: Vec<String> = Vec::new();
        for (stage, commands) in &stage_map {
            Self::execute_stage(
                *stage,
                commands,
                self.timeout_secs,
                agent_message.as_deref(),
                &mut failures,
            );
        }

        if failures.is_empty() {
            Decision::allow()
        } else {
            Decision::Block {
                message: failures.join("\n\n"),
            }
        }
    }

    fn priority(&self) -> u32 {
        100 // 低優先度 - 他のフィルターの後に実行
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ToolInput;

    #[test]
    fn test_stop_hook_filter_applies_to_stop_event() {
        let hooks = vec![StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        assert!(filter.applies_to(&stop_input));
    }

    #[test]
    fn test_stop_hook_filter_does_not_apply_to_other_events() {
        let hooks = vec![StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let bash_input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "ls".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&bash_input));
    }

    #[test]
    fn test_stop_hook_filter_execute_returns_allow() {
        let hooks = vec![StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    // === Edge Case Tests ===

    #[test]
    fn test_execute_command_empty_is_error() {
        assert!(StopHookFilter::execute_command("   ", 60, None).is_err());
    }

    #[test]
    fn test_execute_command_with_quoted_args() {
        assert!(StopHookFilter::execute_command("echo 'hello world'", 60, None).is_ok());
    }

    #[test]
    fn test_execute_ignores_hook_failure_and_allows() {
        // 存在しないコマンドは失敗するが、フィルターはAllowを返すべき
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-command-xyz-abc-123".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_priority() {
        let filter = StopHookFilter::new(vec![], false, 60);
        assert_eq!(filter.priority(), 100);
    }

    // === Loop prevention ===

    #[test]
    fn test_stop_hook_active_true_skips_all_hooks() {
        let hooks = vec![StopHook {
            commands: vec!["echo should-not-run".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: true,
                ..Default::default()
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_stop_hook_active_false_runs_hooks() {
        let hooks = vec![StopHook {
            commands: vec!["echo running".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
                ..Default::default()
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    /// loop_count 指定の Stop 入力を作るテストヘルパー。
    fn make_stop_input_with_loop_count(loop_count: Option<u32>) -> HookInput {
        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count,
                response: None,
                stop_hook_active: false,
                ..Default::default()
            }),
            session_id: None,
        }
    }

    /// 失敗する report=true フック。実行されれば Block、スキップされれば Allow になるため、
    /// ループ防止によるスキップと実行を Decision で区別できる。
    fn make_failing_report_hook() -> Vec<StopHook> {
        vec![StopHook {
            commands: vec!["sh -c 'echo loop-test-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: Some(true),
        }]
    }

    #[test]
    fn test_loop_count_one_skips_all_hooks() {
        // Cursor: loop_count >= 1 は既にフォローアップ済みを意味するため、
        // 失敗するフックでも実行されず Allow になる（無限ループ防止）
        let filter = StopHookFilter::new(make_failing_report_hook(), false, 10);
        let decision = filter.execute(&make_stop_input_with_loop_count(Some(1)));
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "loop_count=1 should skip hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_loop_count_large_skips_all_hooks() {
        let filter = StopHookFilter::new(make_failing_report_hook(), false, 10);
        let decision = filter.execute(&make_stop_input_with_loop_count(Some(5)));
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "loop_count=5 should skip hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_loop_count_zero_runs_hooks() {
        // loop_count=0 は初回の Stop なのでフックは実行される（失敗が Block で返る）
        let filter = StopHookFilter::new(make_failing_report_hook(), false, 10);
        let decision = filter.execute(&make_stop_input_with_loop_count(Some(0)));
        assert!(
            matches!(decision, Decision::Block { .. }),
            "loop_count=0 should run hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_loop_count_none_runs_hooks() {
        // loop_count を送らないエージェント（Claude 等）では従来どおり実行される
        let filter = StopHookFilter::new(make_failing_report_hook(), false, 10);
        let decision = filter.execute(&make_stop_input_with_loop_count(None));
        assert!(
            matches!(decision, Decision::Block { .. }),
            "loop_count=None should run hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_condition_none_hook_always_allows_even_on_failure() {
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-command-xyz".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
                ..Default::default()
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    // === Conditional hooks ===

    fn make_stop_input() -> HookInput {
        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
                ..Default::default()
            }),
            session_id: None,
        }
    }

    fn wait_for_path(path: &std::path::Path, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        path.exists()
    }

    #[test]
    fn test_conditional_hook_file_not_found_skips() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["false".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("nonexistent-file-xyz-abc.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_file_exists_command_succeeds() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["true".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_file_exists_command_fails_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo lint-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("lint-error"),
                    "Expected lint-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_conditional_hook_fake_timeout_message_with_exit_124_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'echo \"[Command timed out after 2s: fake]\" >&2; exit 124'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("timed out after 2s: fake"),
                    "Expected fake timeout stderr in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision for fake timeout output"),
        }
    }

    #[test]
    fn test_conditional_hook_command_not_found_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-lint-tool-xyz-123".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_multiple_hooks_both_fail_collects_all() {
        use crate::config::HookCondition;
        // すべての条件付きフックが並列実行され、両方の失敗が収集される
        let hooks = vec![
            StopHook {
                commands: vec!["sh -c 'echo first-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
            },
            StopHook {
                commands: vec!["sh -c 'echo second-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("first-error"),
                    "Expected first-error in message, got: {}",
                    message
                );
                assert!(
                    message.contains("second-error"),
                    "Expected second-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_mixed_hooks_unconditional_then_conditional() {
        use crate::config::HookCondition;
        let hooks = vec![
            StopHook {
                commands: vec!["echo unconditional".to_string()],
                condition: None,
                stage: None,
                report: None,
            },
            StopHook {
                commands: vec!["true".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_stdout_in_block_reason() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'echo stdout-content; echo stderr-content >&2; exit 1'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("stdout-content") || message.contains("stderr-content"),
                    "Expected output in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_conditional_hook_multiple_commands_all_succeed() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["true".to_string(), "echo ok".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_multiple_commands_second_fails_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec![
                "true".to_string(),
                "sh -c 'echo second-cmd-error >&2; exit 1'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("second-cmd-error"),
                    "Expected second-cmd-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_conditional_hook_multiple_commands_both_fail_collects_all() {
        use crate::config::HookCondition;
        // すべてのコマンドが並列実行され、両方の失敗が収集される
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'echo first-error >&2; exit 1'".to_string(),
                "sh -c 'echo second-error >&2; exit 1'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("first-error"),
                    "Expected first-error in message, got: {}",
                    message
                );
                assert!(
                    message.contains("second-error"),
                    "Expected second-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_unconditional_hook_multiple_commands_fire_and_forget() {
        let hooks = vec![StopHook {
            commands: vec![
                "echo first".to_string(),
                "nonexistent-command-xyz".to_string(),
                "echo third".to_string(),
            ],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_non_report_hook_does_not_affect_decision() {
        // Non-report hooks: 失敗しても決定には影響しない（Allow を返す）。
        // ログ出力のためプロセス完了は待つが、決定はブロックしない。
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo non-report-output >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let decision = filter.execute(&make_stop_input());

        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_non_report_hook_respects_timeout() {
        // Non-report hooks もタイムアウトで強制終了される
        let hooks = vec![StopHook {
            commands: vec!["sleep 30".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 5,
            "non-report hook should be killed by timeout: took {:?}",
            elapsed
        );
    }

    // === Timeout tests ===

    #[test]
    fn test_execute_command_timeout_kills_process() {
        // sleep 10 should be killed after 2 second timeout
        let start = std::time::Instant::now();
        let result = StopHookFilter::execute_command_tracked("sleep 10", 2, None);
        let elapsed = start.elapsed();

        // タイムアウト時は明示的なタイムアウト出力を返すべき
        assert!(result.is_ok(), "Timeout should return Ok");
        let result = result.unwrap();
        assert!(result.timed_out, "Output should indicate timeout result");
        let output = result.output;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should have timed out in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_execute_command_completes_before_timeout() {
        let result = StopHookFilter::execute_command("echo hello", 60, None);
        assert!(result.is_ok(), "Should complete before timeout");
    }

    #[test]
    fn test_stop_hook_timeout_unconditional() {
        // Unconditional hook with timeout should allow (fire-and-forget, timeout just warns)
        let hooks = vec![StopHook {
            commands: vec!["sleep 10".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 5,
            "Unconditional hook should timeout in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_timeout_conditional_reports_failure() {
        use crate::config::HookCondition;
        // report=true の条件付きフックがタイムアウトした場合、Block を返すべき
        let hooks = vec![StopHook {
            commands: vec!["sleep 10".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None, // condition あり → should_report() = true
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(
            matches!(decision, Decision::Block { .. }),
            "タイムアウトした report=true フックは Block を返すべき, got: {:?}",
            decision
        );
        if let Decision::Block { message } = &decision {
            assert!(
                message.contains("timed out"),
                "タイムアウトメッセージが含まれるべき: {}",
                message
            );
        }
        assert!(
            elapsed.as_secs() < 5,
            "タイムアウトは約2秒で発生すべき, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_execute_command_timeout_returns_timeout_notice() {
        // 出力後にハングするコマンド: タイムアウト時はOkと通知を返すべき
        let result = StopHookFilter::execute_command_tracked(
            "sh -c 'echo before-timeout; sleep 30'",
            2,
            None,
        );
        // タイムアウトはOkとして扱われる
        assert!(result.is_ok(), "Timeout should return Ok");
        let result = result.unwrap();
        assert!(result.timed_out, "Expected tracked timeout output");
        let output = result.output;
        // stdout is empty on timeout (reader threads are not joined to avoid blocking)
        assert!(
            output.stdout.is_empty(),
            "Stdout should be empty on timeout"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
    }

    #[test]
    fn test_stop_hook_timeout_mixed_fast_and_slow_conditional() {
        use crate::config::HookCondition;
        // 2つの条件付きコマンドが並列実行: 1つは高速成功、1つはタイムアウト
        // タイムアウトしたコマンドは失敗として報告される
        let hooks = vec![StopHook {
            commands: vec!["true".to_string(), "sleep 10".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None, // condition あり → should_report() = true
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(
            matches!(decision, Decision::Block { .. }),
            "タイムアウトした report=true フックは Block を返すべき, got: {:?}",
            decision
        );
        if let Decision::Block { message } = &decision {
            assert!(
                message.contains("timed out"),
                "タイムアウトメッセージが含まれるべき: {}",
                message
            );
        }
        assert!(
            elapsed.as_secs() < 5,
            "タイムアウトは約2秒で発生すべき, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_timeout_mixed_fast_and_slow_unconditional() {
        // 無条件: 1つは高速、1つは低速。両方ともブロックせずに完了/タイムアウトすべき
        let hooks = vec![StopHook {
            commands: vec!["echo fast".to_string(), "sleep 10".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Unconditional hooks always allow"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_timeout_process_is_killed() {
        // マーカーファイルを使用: sleepプロセスはファイル作成前にkillされるべき
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-timeout-kill-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        // コマンド: sleep 10後にマーカー作成。正しくkillされればマーカーは存在しない
        let cmd = format!("sh -c 'sleep 10; echo done > {}'", marker_path);
        let result = StopHookFilter::execute_command_tracked(&cmd, 2, None);
        assert!(result.is_ok(), "Timeout should return Ok");
        assert!(result.unwrap().timed_out, "Expected timeout kill");

        // ゾンビ/孤児プロセスのクリーンアップを待つ
        std::thread::sleep(std::time::Duration::from_millis(500));

        assert!(
            !marker.exists(),
            "Marker file should not exist because process was killed before sleep finished"
        );

        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn test_stop_hook_custom_timeout_value() {
        // Verify custom timeout value is respected (3s timeout, 1s command succeeds)
        let hooks = vec![StopHook {
            commands: vec!["sleep 1".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 3);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 3,
            "1s command should complete before 3s timeout: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_report_false_failure_does_not_block() {
        // report=false の失敗フックは Block を返さない（fire-and-forget）
        let hooks = vec![StopHook {
            commands: vec!["false".to_string()],
            condition: None,
            stage: None,
            report: Some(false),
        }];
        let filter = StopHookFilter::new(hooks, false, 5);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "report=false のフック失敗は Allow を返すべき, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_stop_hook_multi_stage_collects_failures_across_stages() {
        use crate::config::HookCondition;
        // stage 1 は成功、stage 5 は失敗 → 失敗メッセージは stage 5 のもののみ
        let hooks = vec![
            StopHook {
                commands: vec!["true".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: Some(true),
            },
            StopHook {
                commands: vec!["sh -c 'echo stage5-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(5),
                report: Some(true),
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 10);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Block { .. }),
            "stage 5 の失敗により Block を返すべき, got: {:?}",
            decision
        );
        if let Decision::Block { message } = &decision {
            assert!(
                message.contains("stage5-error"),
                "stage 5 のエラーメッセージが含まれるべき: {}",
                message
            );
        }
    }

    // === Agent message propagation tests ===

    #[test]
    fn test_execute_command_passes_agent_message_env() {
        // agent_message指定時にCLAW_HOOKS_AGENT_MESSAGEが設定されることを検証
        let result = StopHookFilter::execute_command(
            "sh -c 'echo $CLAW_HOOKS_AGENT_MESSAGE'",
            60,
            Some("test agent message"),
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("test agent message"),
            "Expected agent message in stdout, got: {}",
            stdout
        );
    }

    #[test]
    fn test_execute_command_no_agent_message_env_when_none() {
        // agent_messageがNoneの場合、CLAW_HOOKS_AGENT_MESSAGEが未設定であることを検証
        let result = StopHookFilter::execute_command(
            "sh -c 'echo \"${CLAW_HOOKS_AGENT_MESSAGE:-unset}\"'",
            60,
            None,
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("unset"),
            "Expected 'unset' in stdout when no agent_message, got: {}",
            stdout
        );
    }

    #[test]
    fn test_stop_hook_propagates_agent_message_to_child() {
        use crate::config::HookCondition;
        // エージェントメッセージ環境変数をechoする条件付きフック
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'test \"$CLAW_HOOKS_AGENT_MESSAGE\" = \"hello from agent\"'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                agent_message: Some("hello from agent".to_string()),
                ..Default::default()
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Expected Allow (env var should match), got: {:?}",
            decision
        );
    }

    // === Stage ordering tests ===

    #[test]
    fn test_stage_ordering_lower_stage_runs_first() {
        use crate::config::HookCondition;
        // Stage 1 creates a marker file, Stage 3 checks for it.
        // If stage ordering works, marker will exist when stage 3 runs.
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-stage-order-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![
            StopHook {
                commands: vec![format!("sh -c 'echo done > {}'", marker_path)],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: None,
            },
            StopHook {
                commands: vec![format!(
                    "sh -c 'test -f {} || (echo stage-order-failed >&2; exit 1)'",
                    marker_path
                )],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(3),
                report: Some(true),
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Stage 3 should see marker from stage 1, got: {:?}",
            decision
        );
        let _ = std::fs::remove_file(marker);
    }

    // === Report behavior tests ===

    #[test]
    fn test_report_false_ignores_failure() {
        use crate::config::HookCondition;
        // report=falseのフックは失敗してもブロックしないべき
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo report-off-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: Some(false),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "report=false should not block, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_report_true_without_condition_blocks_on_failure() {
        // 条件なしでreport=trueのフックは失敗時にブロックすべき
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo explicit-report-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: Some(true),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("explicit-report-error"),
                    "Expected error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision for report=true hook failure"),
        }
    }

    #[test]
    fn test_default_report_no_condition_allows_on_failure() {
        // Hook without condition and without explicit report (defaults to false)
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo no-report-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "No condition + no report should default to fire-and-forget, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_default_report_with_condition_blocks_on_failure() {
        use crate::config::HookCondition;
        // Hook with condition and without explicit report (defaults to true)
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo default-report-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("default-report-error"),
                    "Expected error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision for conditional hook"),
        }
    }

    #[test]
    fn test_fire_and_forget_detached_process_continues_after_decision() {
        // report=false は Hook 応答を待たせず、子プロセスだけが後続で完了する。
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-ff-complete-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![StopHook {
            commands: vec![format!(
                "sh -c 'echo ff-output; echo done > {}'",
                marker_path
            )],
            condition: None,
            stage: None,
            report: Some(false),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "report=false の Stop hook は子プロセス完了を待たないべき"
        );

        assert!(
            wait_for_path(&marker, std::time::Duration::from_secs(3)),
            "Detached process should have completed and created marker file"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn test_fire_and_forget_does_not_wait_for_slow_process() {
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-drain-test-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![StopHook {
            commands: vec![format!("sh -c 'sleep 1; echo done > {}'", marker_path)],
            condition: None,
            stage: None,
            report: Some(false),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "slow report=false hook should not delay the decision"
        );
        assert!(
            !marker.exists(),
            "Decision 直後は遅延コマンドの完了を待っていないこと"
        );

        assert!(
            wait_for_path(&marker, std::time::Duration::from_secs(3)),
            "detached command should still complete later"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn test_fire_and_forget_spawn_failure_does_not_panic() {
        // 存在しないコマンドの spawn 失敗でパニックしない
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-command-xyz-ff-test".to_string()],
            condition: None,
            stage: None,
            report: Some(false),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Spawn failure should not cause panic or block"
        );
    }

    #[test]
    fn test_mixed_stages_and_reports() {
        use crate::config::HookCondition;
        // Stage 1: report=false (fire-and-forget)
        // Stage 3: report=true (should block)
        let hooks = vec![
            StopHook {
                commands: vec!["sh -c 'echo stage1-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: Some(false),
            },
            StopHook {
                commands: vec!["sh -c 'echo stage3-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(3),
                report: Some(true),
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("stage3-error"),
                    "Expected stage3-error in message, got: {}",
                    message
                );
                assert!(
                    !message.contains("stage1-error"),
                    "Should not contain stage1-error (report=false), got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    // === クロスプロセスループ防止テスト ===

    #[test]
    fn test_cross_process_loop_prevention_env_var() {
        // CLAW_HOOKS_STOP_ACTIVE=1 が設定されている場合、すべてのフックがスキップされる
        // SAFETY: このテストはシングルスレッドで実行され、環境変数は直後に復元される
        unsafe {
            std::env::set_var(STOP_ACTIVE_ENV, "1");
        }

        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo should-not-run >&2; exit 1'".to_string()],
            condition: Some(crate::config::HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: Some(true),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());

        // SAFETY: 環境変数の復元
        unsafe {
            std::env::remove_var(STOP_ACTIVE_ENV);
        }

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "環境変数によりフックがスキップされ Allow を返すべき, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_stop_active_env_propagated_to_child() {
        // execute_command_tracked が子プロセスに CLAW_HOOKS_STOP_ACTIVE=1 を設定することを検証
        let result = StopHookFilter::execute_command(
            &format!("sh -c 'echo ${}' ", STOP_ACTIVE_ENV),
            60,
            None,
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.trim() == "1",
            "子プロセスに CLAW_HOOKS_STOP_ACTIVE=1 が設定されるべき, got: {}",
            stdout.trim()
        );
    }

    #[test]
    fn test_build_reason_stdout_only() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256), // exit code 1
            stdout: b"lint error found".to_vec(),
            stderr: Vec::new(),
        };
        let reason = StopHookFilter::build_reason("cargo clippy", &output);
        assert!(reason.contains("lint error found"));
        assert!(reason.contains("cargo clippy"));
    }

    #[test]
    fn test_build_reason_stderr_only() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: b"compile error".to_vec(),
        };
        let reason = StopHookFilter::build_reason("cargo build", &output);
        assert!(reason.contains("compile error"));
    }

    #[test]
    fn test_build_reason_both_stdout_and_stderr() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: b"stdout content".to_vec(),
            stderr: b"stderr content".to_vec(),
        };
        let reason = StopHookFilter::build_reason("cmd", &output);
        assert!(reason.contains("stdout content"));
        assert!(reason.contains("stderr content"));
    }

    #[test]
    fn test_build_reason_empty_output() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let reason = StopHookFilter::build_reason("cmd", &output);
        assert!(reason.contains("Stop hook failed: cmd"));
    }
}
