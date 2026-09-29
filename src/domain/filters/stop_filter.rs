//! Stop イベントフックフィルターの実装。
//!
//! - stage を昇順に実行する。各 stage のフックの条件は、その stage を始める直前に評価する
//!   （前の stage が作ったファイルを条件に使えるように）。
//! - 後続を止めるフック（`StopHook::gates_later_stages`）が失敗したら、同じ stage の他の
//!   フックは最後まで待ったうえで、後続の stage を実行しない。検査に落ちた差分を後続の
//!   git の自動コミット等が取り込み、Block を返した時点で commit・push まで済んでいる、
//!   という事態を防ぐ。
//! - そうして止まった停止では、継続中の停止を知らせてくるエージェント
//!   （`AgentProfile::stop_retry`）に限り、次の継続中の停止で 1 回だけ再試行する予定を
//!   状態の置き場所へ記録する。再試行では Block を返さない（無限ループを防ぐ）。

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::Filter;
use crate::config::StopHook;
use crate::domain::command::{
    TimedOutput, display_label, run_with_timeout_tracked, spawn_detached_with_env,
    spawn_piped_with_env,
};
use crate::domain::state_store::StateStore;
use crate::domain::{AgentProfile, Decision, HookEvent, HookInput, StopSessionKind, ToolInput};

/// プロセス間の再帰的な Stop フック実行を防止する環境変数。
/// claw-hooks が Stop フックを実行する際、子プロセスにこの環境変数を設定する。
/// 子プロセス（例: git-sc が起動する別の AI エージェント CLI）が別の claw-hooks
/// Stop イベントをトリガーした場合、この環境変数が継承され、Stop フックをスキップ
/// してループを断ち切る。
const STOP_ACTIVE_ENV: &str = "CLAW_HOOKS_STOP_ACTIVE";

/// 再試行の予定の記録を置く状態の種類（`StateStore` の `<kind>` ディレクトリ）。
const RETRY_STATE_KIND: &str = "stop-retry";

/// 再試行の予定の記録のキーの先頭要素。記録の意味を変えたら上げ、古い記録と混ざらないようにする。
const RETRY_KEY_VERSION: &str = "stop-retry-v1";

/// 再試行の予定の記録の形式の版。
const RETRY_RECORD_VERSION: u32 = 1;

/// 再試行の予定の記録を残す期間。これより古い記録は掃除される。
const RETRY_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 1 つの stage に並べるラベルの上限。超えた分は `and N more` にまとめる。
/// 要約が長いと、Block の文面の切り詰めで失敗の詳細が押し出されるため。
const MAX_LABELS_PER_STAGE: usize = 8;

/// NanoBuddy の吹き出しに並べる、実行しなかったフックのラベルの上限。
const MAX_SPEECH_LABELS: usize = 3;

/// 記録が現在の設定と合わず、再試行を見送ったときのユーザー向け通知。
const RETRY_SKIPPED_NOTICE: &str = "claw-hooks: the stop hook configuration changed after the failed stop, \
     so the scheduled retry was skipped. Later stages were not run.";

/// 記録が現在の設定と合わず、再試行を見送ったときの NanoBuddy の吹き出し。
const RETRY_SKIPPED_SPEECH: &str = "Stop hook retry skipped: config changed";

/// Stop イベントフックのフィルター。
pub struct StopHookFilter {
    hooks: Vec<StopHook>,
    nano_buddy: bool,
    timeout_secs: u64,
    /// 呼び出し元エージェントの性質（再試行を予定できるか・記録のキーの名前空間）
    agent: AgentProfile,
    /// 失敗した stage の再試行の予定を次の停止へ引き継ぐ記録の置き場所
    store: StateStore,
}

impl StopHookFilter {
    /// 新しい StopHookFilter を作成する。
    ///
    /// 状態の置き場所は無効（再試行を予定しない）で、`with_state` で渡す。
    pub fn new(hooks: Vec<StopHook>, nano_buddy: bool, timeout_secs: u64) -> Self {
        Self {
            hooks,
            nano_buddy,
            timeout_secs,
            agent: AgentProfile::default(),
            store: StateStore::disabled(),
        }
    }

    /// 呼び出し元エージェントと状態の置き場所を設定する。
    pub fn with_state(mut self, agent: AgentProfile, store: StateStore) -> Self {
        self.agent = agent;
        self.store = store;
        self
    }

    /// エージェントの最後のメッセージを子プロセスに渡す環境変数。
    /// git-sc などのツールがエージェントのコンテキストを使用してコミットメッセージを生成できるようにする。
    const AGENT_MESSAGE_ENV: &'static str = "CLAW_HOOKS_AGENT_MESSAGE";

    /// Stop hook のコマンドから、引数やディレクトリを含まない表示ラベルを作る。
    ///
    /// コマンド引数にはトークンなどの機密値が含まれ得るため、永続ログと
    /// エージェント向けエラーには実行ファイル名だけを使用する。制御文字は `?` に置き換え、
    /// 設定値に紛れた改行や ANSI エスケープが要約の行を偽装できないようにする。
    fn command_label(command: &str) -> String {
        crate::domain::parse_shell_tokens(command)
            .first()
            .map(|program| display_label(program))
            .unwrap_or_else(|| "<empty>".to_string())
    }

    /// 複数コマンドを安全な表示ラベルへ変換する。
    fn command_labels(commands: &[String]) -> Vec<String> {
        commands
            .iter()
            .map(|command| Self::command_label(command))
            .collect()
    }

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
        let label = display_label(program);

        debug!(
            "🛑 Executing stop hook: program={} arg_count={}",
            label,
            args.len()
        );

        let mut envs: Vec<(&str, &str)> = vec![(STOP_ACTIVE_ENV, "1")];
        if let Some(msg) = agent_message {
            envs.push((Self::AGENT_MESSAGE_ENV, msg));
        }

        let start = std::time::Instant::now();
        // 起動の失敗は `Failed to execute '<label>': <OS のエラー>` のまま返す
        // （呼び出し側が `Stop hook failed: <label>` の見出しを付ける）。
        let child = spawn_piped_with_env(program, args, &envs)?;
        let result = run_with_timeout_tracked(child, timeout_secs, &label);
        let elapsed = start.elapsed();
        info!(
            "⏰️ Stop hook [{}] completed in {:.2}s",
            label,
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
        let label = display_label(program);

        debug!(
            "🚀 Executing stop hook (fire-and-forget): program={} arg_count={}",
            label,
            args.len()
        );

        let mut envs: Vec<(&str, &str)> = vec![(STOP_ACTIVE_ENV, "1")];
        if let Some(msg) = agent_message {
            envs.push((Self::AGENT_MESSAGE_ENV, msg));
        }

        match spawn_detached_with_env(program, args, &envs) {
            Ok(pid) => info!("🚀 Detached stop hook [{}] started with pid={}", label, pid),
            Err(e) => {
                warn!(
                    "❌ Failed to spawn fire-and-forget stop hook '{}': {}",
                    label, e
                );
            }
        }
    }

    /// ストップフックコマンドの stdout/stderr 出力サイズをログに記録する。
    ///
    /// lint/typecheck のツール出力にはソース行の断片や file path などの機密が含まれ得るため、
    /// 永続ログにはサイズ（バイト数）のみを残す。
    /// 本文の確認は `--trace` フラグ（stderr 出力、ディスク非永続）で行う。
    fn log_output(label: &str, output: &Output) {
        let stdout_len = output.stdout.len();
        let stderr_len = output.stderr.len();
        if stdout_len > 0 {
            info!("Stop hook [{}] stdout: {} bytes", label, stdout_len);
        }
        if stderr_len > 0 {
            info!("Stop hook [{}] stderr: {} bytes", label, stderr_len);
        }
    }

    /// コマンド出力（stdout + stderr）からブロック理由を構築する。
    fn build_reason(label: &str, output: &Output) -> String {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut reason = format!("Stop hook failed: {}\n", label);
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

    /// 停止の種類を判定する（ループ防止）。
    ///
    /// `stop_active` は環境変数 `CLAW_HOOKS_STOP_ACTIVE` があるか。claw-hooks の Stop フックが
    /// 起動したプロセスの中の停止なので、再試行を含めて何も実行しない（最優先）。
    fn classify_stop(input: &HookInput, stop_active: bool) -> StopPhase {
        // クロスプロセス再帰ループ防止
        if stop_active {
            debug!(
                "⛔ {}=1 detected, skipping all stop hooks (cross-process loop prevention)",
                STOP_ACTIVE_ENV
            );
            return StopPhase::Skip;
        }

        let ToolInput::Stop(ref stop_input) = input.tool_input else {
            return StopPhase::First {
                agent_message: None,
            };
        };
        // Cursor は stop_hook_active を持たず、代わりに loop_count
        // （stop hook 由来の自動フォローアップ発火回数、0 始まり）を送る。
        // 1 は初回の Block によるフォローアップ後の停止（Claude の stop_hook_active と対称で、
        // 再試行の対象）。2 以上は再試行の機会も過ぎているので、全 stop hook をスキップして
        // 無限ループを防ぐ。
        if let Some(count) = stop_input.loop_count
            && count >= 2
        {
            debug!("🛑 loop_count={} (>=2), skipping all stop hooks", count);
            return StopPhase::Skip;
        }

        let agent_message = stop_input.agent_message.clone();
        if stop_input.stop_hook_active || stop_input.loop_count.is_some_and(|count| count >= 1) {
            StopPhase::Continuing { agent_message }
        } else {
            StopPhase::First { agent_message }
        }
    }

    /// Stop を処理する。`stop_active` は環境変数 `CLAW_HOOKS_STOP_ACTIVE` があるか
    /// （`Filter::execute` が読む。テストはプロセスの環境変数を書き換えずに済むよう直接渡す）。
    fn execute_stop(&self, input: &HookInput, stop_active: bool) -> Decision {
        // Stop を発火したセッションの種別（メイン / 委譲エージェント）
        let session_kind = match &input.tool_input {
            ToolInput::Stop(stop) => stop.session_kind,
            _ => StopSessionKind::Primary,
        };

        match Self::classify_stop(input, stop_active) {
            StopPhase::Skip => Decision::allow(),
            StopPhase::First { agent_message } => {
                self.execute_first_stop(input, session_kind, agent_message.as_deref())
            }
            StopPhase::Continuing { agent_message } => {
                self.execute_continuing_stop(input, session_kind, agent_message.as_deref())
            }
        }
    }

    /// 初回の停止: stage を順に実行し、失敗があれば Block を返す。
    ///
    /// 後続を止めるフックの失敗で実行しなかったフックがあり、再試行の対象なら、次の継続中の
    /// 停止で 1 回だけ再試行する予定を記録する（記録できたときだけ Block の要約でそう伝える）。
    fn execute_first_stop(
        &self,
        input: &HookInput,
        session_kind: StopSessionKind,
        agent_message: Option<&str>,
    ) -> Decision {
        let retry_key = self.retry_key(input, session_kind);
        // 前のターンで予定したまま使われなかった再試行（Block の後にユーザーが中断した等）を
        // 消す。残すと、このターンとは無関係な継続中の停止で古い予定が使われる。
        if let Some(key) = retry_key.as_deref() {
            self.store.remove(RETRY_STATE_KIND, key);
        }

        // NanoBuddy 通知（全ストップフックの前に実行し、最初に到着させる）。
        // teammate 等のエージェントセッションの Stop では通知しない（通知スパム防止。
        // エージェントの完了通知は SubagentStop 経由で別途行われる）。
        if self.nano_buddy && session_kind == StopSessionKind::Primary {
            debug!("🐱 NanoBuddy stop notification");
            crate::notify::nano_buddy::notify_stop_hook();
        }

        let run = self.run_stages(session_kind, RunPlan::All, agent_message);

        let mut retry_scheduled = false;
        if let Some(stopped) = &run.stopped
            && !stopped.not_run.is_empty()
        {
            warn!(
                "⚠️ Stop hook stage {} failed; later stages were not run: {}",
                stopped.stage,
                format_stage_groups(&stopped.not_run)
            );
            if let Some(key) = retry_key.as_deref() {
                retry_scheduled = self.schedule_retry(key, stopped.stage);
            }
        }

        if run.failures.is_empty() {
            Decision::allow()
        } else {
            Decision::Block {
                message: first_stop_block_message(&run, retry_scheduled),
            }
        }
    }

    /// 継続中の停止（Block で作業を続けさせた後の停止）: 初回の停止で予定した再試行が
    /// あれば 1 回だけ実行する。
    ///
    /// ここでは Block を返さない。返すと再試行の失敗が再び作業を続けさせ、停止のたびに
    /// 同じ失敗を繰り返す。再試行の失敗はユーザー向けの通知（`user_notice`）とログで知らせる。
    fn execute_continuing_stop(
        &self,
        input: &HookInput,
        session_kind: StopSessionKind,
        agent_message: Option<&str>,
    ) -> Decision {
        let Some(key) = self.retry_key(input, session_kind) else {
            debug!("🛑 Continuing stop without retry support, skipping all stop hooks");
            return Decision::allow();
        };
        // 記録は取り出すと同時に消える。この後の継続中の停止（他のフックが作業を続けさせた、
        // 再試行を使い切った等）はすべてスキップになり、再試行は 1 回に限られる。
        let Some(contents) = self.store.take(RETRY_STATE_KIND, &key) else {
            debug!("🛑 Continuing stop without a scheduled retry, skipping all stop hooks");
            return Decision::allow();
        };
        let Some(failed_stage) = self.scheduled_retry_stage(&contents) else {
            // 設定が変わった（または記録が壊れている）ので、どのフックが未実行かを決められない。
            // 後続の副作用を推測で始めず、再試行を見送ったことを知らせる。
            warn!(
                "⚠️ Stop hook retry skipped: the scheduled retry does not match the current \
                 stop hook configuration; later stages were not run"
            );
            self.notify_speech(RETRY_SKIPPED_SPEECH);
            return Decision::allow_with_notice(RETRY_SKIPPED_NOTICE.to_string());
        };

        info!("🔁 Retrying stop hooks (failed stage {})", failed_stage);
        let run = self.run_stages(session_kind, RunPlan::Retry { failed_stage }, agent_message);
        for failure in run.failures.iter().filter(|failure| !failure.gate) {
            warn!(
                "⚠️ Stop hook [{}] at stage {} failed during the retry (it does not gate later stages)",
                failure.label, failure.stage
            );
        }

        match &run.stopped {
            Some(stopped) => {
                let notice = retry_failed_notice(&run, stopped);
                warn!("⚠️ {}", notice);
                self.notify_speech(&retry_failed_speech(stopped));
                Decision::allow_with_notice(notice)
            }
            None => {
                info!("Stop hook retry passed");
                Decision::allow()
            }
        }
    }

    /// 再試行の予定の記録のキー。再試行の対象でなければ `None`。
    ///
    /// 対象は、継続中の停止を知らせてくるエージェント（`stop_retry`）の、空白でない
    /// セッション ID を持つメインセッションの停止だけ。キーにはセッション種別を含めないため、
    /// 委譲セッション（teammate 等）の停止で予定を作ったり消費したりすると、同じ
    /// セッション ID を持つ停止と予定を取り違え得る。
    fn retry_key(&self, input: &HookInput, session_kind: StopSessionKind) -> Option<String> {
        if !self.agent.stop_retry || session_kind != StopSessionKind::Primary {
            return None;
        }
        let session_id = input
            .session_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())?;
        Some(StateStore::key(&[
            RETRY_KEY_VERSION,
            self.agent.id,
            session_id,
        ]))
    }

    /// Stop フックの設定の指紋。再試行の予定を作った時点と設定が変わったかを見分ける。
    fn config_fingerprint(&self) -> String {
        StateStore::key(&[&format!("{:?}", self.hooks)])
    }

    /// 再試行の予定を記録する。記録できたら true。
    fn schedule_retry(&self, key: &str, failed_stage: u8) -> bool {
        let record = RetryRecord {
            version: RETRY_RECORD_VERSION,
            failed_stage,
            fingerprint: self.config_fingerprint(),
        };
        let written = serde_json::to_vec(&record).is_ok_and(|contents| {
            self.store
                .put(RETRY_STATE_KIND, key, &contents, RETRY_MAX_AGE)
        });
        if written {
            info!(
                "🔁 Stop hook retry scheduled for the next stop (failed stage {})",
                failed_stage
            );
        } else {
            debug!("Stop hook retry was not scheduled (the state store is unavailable)");
        }
        written
    }

    /// 記録された再試行の予定を読み、現在の設定で使えるなら失敗した stage を返す。
    ///
    /// 壊れている・形式の版が違う・設定の指紋が違う記録は使わない（`None`）。
    fn scheduled_retry_stage(&self, contents: &[u8]) -> Option<u8> {
        let record: RetryRecord = serde_json::from_slice(contents).ok()?;
        (record.version == RETRY_RECORD_VERSION && record.fingerprint == self.config_fingerprint())
            .then_some(record.failed_stage)
    }

    /// NanoBuddy に吹き出しで知らせる（`nano_buddy` が有効なときだけ）。
    ///
    /// 呼ぶのは再試行の結果だけで、再試行はメインセッションの停止に限る（`retry_key`）ため、
    /// 委譲セッションの停止で吹き出しが出ることはない。
    fn notify_speech(&self, text: &str) {
        if self.nano_buddy {
            debug!("🐱 NanoBuddy speech notification");
            crate::notify::nano_buddy::notify_speech(text);
        }
    }

    /// stage を昇順に実行する。後続を止めるフックが失敗した stage で打ち切り、
    /// それより後の stage のうち実行しなかったフックを結果に残す。
    fn run_stages(
        &self,
        session_kind: StopSessionKind,
        plan: RunPlan,
        agent_message: Option<&str>,
    ) -> StopRun {
        let cwd = std::env::current_dir().unwrap_or_default();
        let stages = self.stage_plan(session_kind, plan);

        let mut failures = Vec::new();
        for (&stage, hooks) in &stages {
            let entries = Self::prepare_stage(stage, hooks, &cwd);
            failures.extend(Self::execute_stage(
                stage,
                entries,
                self.timeout_secs,
                agent_message,
            ));
            if failures
                .iter()
                .any(|failure| failure.stage == stage && failure.gate)
            {
                let not_run = Self::not_run_groups(&stages, stage, &cwd);
                return StopRun {
                    failures,
                    stopped: Some(GateStop { stage, not_run }),
                };
            }
        }
        StopRun {
            failures,
            stopped: None,
        }
    }

    /// 実行対象のフックを stage ごとにまとめる（stage 昇順、stage 内は設定順）。
    ///
    /// 条件はここでは評価しない（各 stage を始める直前に `prepare_stage` で評価する）。
    fn stage_plan(
        &self,
        session_kind: StopSessionKind,
        plan: RunPlan,
    ) -> BTreeMap<u8, Vec<&StopHook>> {
        let mut stages: BTreeMap<u8, Vec<&StopHook>> = BTreeMap::new();
        for hook in &self.hooks {
            // セッション種別によるフィルタ: teammate 等の委譲エージェントセッションでは
            // session_scope が delegated / all のフックだけを実行する（デフォルト primary は
            // メインセッション専用 — 通知スパム・重複 lint・並列 git コミットのレース防止）。
            if !hook.session_scope.includes(session_kind) {
                debug!(
                    "Stop hook session_scope {:?} excludes {:?} session, skipping: {:?}",
                    hook.session_scope,
                    session_kind,
                    Self::command_labels(&hook.commands)
                );
                continue;
            }
            if !plan.includes(hook) {
                continue;
            }
            stages.entry(hook.stage_value()).or_default().push(hook);
        }
        stages
    }

    /// フックの条件を評価する（条件が無ければ常に満たす）。
    fn evaluate_condition(
        hook: &StopHook,
        cwd: &Path,
    ) -> Result<bool, crate::config::ConditionError> {
        hook.condition
            .as_ref()
            .map_or(Ok(true), |condition| condition.evaluate(cwd))
    }

    /// stage を始める直前に、その stage のフックの条件を評価して実行する内容を決める（設定順）。
    ///
    /// 評価できなかった条件（ファイルの有無を確かめられない等）は、後続を止めるフックなら
    /// 失敗、それ以外はスキップにする。「満たさない」へ潰すと、必須の検査が黙って
    /// 飛ばされたまま後続の stage が走る。
    fn prepare_stage(stage: u8, hooks: &[&StopHook], cwd: &Path) -> Vec<StageEntry> {
        let mut entries = Vec::new();
        for hook in hooks {
            let gate = hook.gates_later_stages();
            match Self::evaluate_condition(hook, cwd) {
                Ok(true) => {
                    if hook.condition.is_some() {
                        debug!(
                            "Stop hook condition met, queuing: {:?}",
                            Self::command_labels(&hook.commands)
                        );
                    }
                    let report = hook.should_report();
                    entries.extend(hook.commands.iter().map(|command| {
                        StageEntry::Run(QualifiedCommand {
                            command: command.clone(),
                            label: Self::command_label(command),
                            report,
                            gate,
                        })
                    }));
                }
                Ok(false) => {
                    debug!(
                        "Stop hook condition not met, skipping: {:?}",
                        Self::command_labels(&hook.commands)
                    );
                }
                Err(error) if gate => {
                    warn!(
                        "⚠️ Stop hook {:?} at stage {} failed: {}",
                        Self::command_labels(&hook.commands),
                        stage,
                        error
                    );
                    let mut labels = Vec::new();
                    for command in &hook.commands {
                        push_unique(&mut labels, Self::command_label(command));
                    }
                    entries.extend(labels.into_iter().map(|label| {
                        StageEntry::Failed(HookFailure {
                            stage,
                            reason: format!("Stop hook failed: {}\n{}", label, error),
                            label,
                            gate,
                        })
                    }));
                }
                Err(error) => {
                    warn!(
                        "⚠️ Stop hook {:?} at stage {} skipped: {}",
                        Self::command_labels(&hook.commands),
                        stage,
                        error
                    );
                }
            }
        }
        entries
    }

    /// `after` より後の stage のうち、gate の失敗で実行しなかったフックのラベルを stage ごとに返す。
    ///
    /// 条件を今評価し、満たす（`Ok(true)`）か評価できない（`Err`）フックだけを挙げる。
    /// 満たさないフックは、その stage に到達していても実行されなかったため挙げない。
    fn not_run_groups(
        stages: &BTreeMap<u8, Vec<&StopHook>>,
        after: u8,
        cwd: &Path,
    ) -> Vec<(u8, Vec<String>)> {
        stages
            .range((Bound::Excluded(after), Bound::Unbounded))
            .filter_map(|(&stage, hooks)| {
                let mut labels = Vec::new();
                for hook in hooks {
                    if matches!(Self::evaluate_condition(hook, cwd), Ok(false)) {
                        continue;
                    }
                    for command in &hook.commands {
                        push_unique(&mut labels, Self::command_label(command));
                    }
                }
                (!labels.is_empty()).then_some((stage, labels))
            })
            .collect()
    }

    /// 単一ステージ内のコマンドを並列実行し、失敗を設定順に返す。
    ///
    /// `report = false` のコマンドは detached で起動するだけで結果を取らない（後続を止める
    /// 条件にもならない）。結果を待つコマンドはスレッドで並列に実行し、すべて終わるまで待つ。
    /// 後続を止める失敗があっても、同じ stage の他のコマンドは最後まで実行する。
    fn execute_stage(
        stage: u8,
        entries: Vec<StageEntry>,
        timeout_secs: u64,
        agent_message: Option<&str>,
    ) -> Vec<HookFailure> {
        debug!("▶ Executing stop hook stage {}", stage);

        let mut slots = Vec::new();
        for entry in entries {
            let qc = match entry {
                StageEntry::Failed(failure) => {
                    slots.push(StageSlot::Failed(failure));
                    continue;
                }
                StageEntry::Run(qc) => qc,
            };
            if !qc.report {
                Self::execute_command_detached(&qc.command, agent_message);
                continue;
            }

            let command = qc.command;
            let label = qc.label.clone();
            let agent_msg = agent_message.map(str::to_string);
            let spawned = std::thread::Builder::new().spawn(move || {
                Self::run_reported(&command, &label, timeout_secs, agent_msg.as_deref())
            });
            match spawned {
                Ok(handle) => slots.push(StageSlot::Running {
                    label: qc.label,
                    gate: qc.gate,
                    handle,
                }),
                // スレッドを作れなければコマンドは実行されていない。結果が分からないものを
                // 成功とは扱わず、失敗として後続を止める判定に含める。
                Err(e) => {
                    warn!(
                        "🛑 Failed to start a thread for stop hook [{}] ({:?})",
                        qc.label,
                        e.kind()
                    );
                    slots.push(StageSlot::Failed(HookFailure {
                        stage,
                        reason: format!(
                            "Stop hook failed: {}\nfailed to start the hook thread",
                            qc.label
                        ),
                        label: qc.label,
                        gate: qc.gate,
                    }));
                }
            }
        }

        let mut failures = Vec::new();
        for slot in slots {
            match slot {
                StageSlot::Failed(failure) => failures.push(failure),
                StageSlot::Running {
                    label,
                    gate,
                    handle,
                } => match handle.join() {
                    Ok(None) => {}
                    Ok(Some(reason)) => failures.push(HookFailure {
                        stage,
                        label,
                        gate,
                        reason,
                    }),
                    Err(_) => {
                        warn!("🛑 Stop hook thread panicked: [{}]", label);
                        failures.push(HookFailure {
                            stage,
                            reason: format!(
                                "Stop hook failed: {}\nStop hook thread panicked",
                                label
                            ),
                            label,
                            gate,
                        });
                    }
                },
            }
        }
        failures
    }

    /// 結果を待つコマンドを実行し、失敗なら理由（`Stop hook failed: <label>\n<出力>`）を返す。
    fn run_reported(
        command: &str,
        label: &str,
        timeout_secs: u64,
        agent_message: Option<&str>,
    ) -> Option<String> {
        match Self::execute_command_tracked(command, timeout_secs, agent_message) {
            Ok(result) => {
                Self::log_output(label, &result.output);
                if result.timed_out {
                    // タイムアウトは異常終了 — 成功として扱わない。
                    // 本文は command 側が組み立てたもの（実際に待った秒数の通知と、
                    // 打ち切り時点までに読めた出力）をそのまま使う。ここで
                    // `timeout_secs` から文面を作り直すと、孫プロセスがパイプを
                    // 保持したケースで実際の待ち時間（drain grace）ではなく
                    // hook_timeout の値が表示され、しかも取得済みの診断が捨てられる。
                    Some(Self::build_reason(label, &result.output))
                } else if result.output.status.success() {
                    None
                } else {
                    Some(Self::build_reason(label, &result.output))
                }
            }
            // 起動の失敗（コマンドが見つからない等）。本文は起動時のエラーそのもの。
            Err(e) => Some(format!("Stop hook failed: {}\n{}", label, e)),
        }
    }
}

/// 停止の種類（ループ防止と再試行の判定に使う）。
enum StopPhase {
    /// フックを一切実行しない。
    Skip,
    /// 初回の停止: すべてのフックを実行する。
    First { agent_message: Option<String> },
    /// 継続中の停止（Stop の Block で作業を続けさせた後の停止）:
    /// 再試行の予定が記録されているときだけ、1 回だけ再試行する。
    Continuing { agent_message: Option<String> },
}

/// どのフックを実行するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunPlan {
    /// 初回の停止: すべてのフック。
    All,
    /// 再試行: `failed_stage` までは後続を止めるフックだけ（検査のやり直し。`report = false`
    /// や `gate = false` のフックは初回に実行・報告済み）、それより後の stage はすべてのフック
    /// （初回に実行しなかったもの）。
    Retry { failed_stage: u8 },
}

impl RunPlan {
    /// このフックを実行対象に含めるか。
    fn includes(self, hook: &StopHook) -> bool {
        match self {
            RunPlan::All => true,
            RunPlan::Retry { failed_stage } => {
                hook.stage_value() > failed_stage || hook.gates_later_stages()
            }
        }
    }
}

/// 条件チェックを通過したコマンド。
struct QualifiedCommand {
    command: String,
    /// 引数とディレクトリを除いたログ・エラー表示用ラベル。
    label: String,
    report: bool,
    /// 失敗したら後続の stage を止めるか（`StopHook::gates_later_stages`）。
    gate: bool,
}

/// stage の中で実行する内容（設定順）。
enum StageEntry {
    /// 実行するコマンド。
    Run(QualifiedCommand),
    /// 実行する前に失敗が決まったもの（後続を止めるフックの条件を評価できなかった）。
    Failed(HookFailure),
}

/// 実行を始めた stage の中身（結果を集める順 = 設定順）。
enum StageSlot {
    /// 結果を待っているコマンド。
    Running {
        label: String,
        gate: bool,
        handle: std::thread::JoinHandle<Option<String>>,
    },
    /// 失敗が決まっているもの。
    Failed(HookFailure),
}

/// 失敗した 1 つのフックのコマンド。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HookFailure {
    stage: u8,
    label: String,
    /// 後続の stage を止める失敗か。
    gate: bool,
    /// エージェントへ返す詳細（`Stop hook failed: <label>\n<出力>`）。
    reason: String,
}

/// gate の失敗で後続の stage を実行しなかったこと。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateStop {
    /// 後続を止める失敗があった stage。
    stage: u8,
    /// 実行しなかった stage とそのラベル（stage 昇順。挙げる基準は `not_run_groups`）。
    not_run: Vec<(u8, Vec<String>)>,
}

/// stage を順に実行した結果。
#[derive(Debug)]
struct StopRun {
    /// 失敗（stage 昇順、stage 内は設定順）。
    failures: Vec<HookFailure>,
    /// gate の失敗で後続の stage を止めた場合の情報。
    stopped: Option<GateStop>,
}

/// 再試行の予定の記録（状態の置き場所に JSON で置く。コマンドやセッション ID は持たない）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryRecord {
    /// 記録の形式の版（`RETRY_RECORD_VERSION`）。
    version: u32,
    /// 後続を止める失敗があった stage。
    failed_stage: u8,
    /// 記録した時点の Stop フックの設定の指紋（`StopHookFilter::config_fingerprint`）。
    fingerprint: String,
}

/// ラベルを重複なく追加する（最初に出た順を保つ）。
fn push_unique(labels: &mut Vec<String>, label: String) {
    if !labels.contains(&label) {
        labels.push(label);
    }
}

/// `(stage, ラベル)` の並びを stage ごとにまとめる（stage 昇順、stage 内は出た順で重複を除く）。
fn group_by_stage<'a>(items: impl IntoIterator<Item = (u8, &'a str)>) -> Vec<(u8, Vec<String>)> {
    let mut groups: BTreeMap<u8, Vec<String>> = BTreeMap::new();
    for (stage, label) in items {
        push_unique(groups.entry(stage).or_default(), label.to_string());
    }
    groups.into_iter().collect()
}

/// ラベルを `a, b, c` の形に並べる。上限を超えた分は `and N more` にまとめる。
fn format_labels(labels: &[String]) -> String {
    if labels.len() <= MAX_LABELS_PER_STAGE {
        return labels.join(", ");
    }
    format!(
        "{}, and {} more",
        labels[..MAX_LABELS_PER_STAGE].join(", "),
        labels.len() - MAX_LABELS_PER_STAGE
    )
}

/// stage ごとのラベルを `stage 1 [a, b]; stage 3 [c]` の形に並べる。
fn format_stage_groups(groups: &[(u8, Vec<String>)]) -> String {
    groups
        .iter()
        .map(|(stage, labels)| format!("stage {} [{}]", stage, format_labels(labels)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// 初回の停止で返す Block の文面を組み立てる。
///
/// 先頭に要約（失敗した stage とラベル、実行しなかった stage、再試行の予定）、空行、
/// 従来の失敗の詳細の順に並べる。Block の文面はアダプターで先頭から切り詰められる
/// （既定 1000 文字）ため、長い lint の出力に押し出されないよう要約を必ず先頭に置く。
fn first_stop_block_message(run: &StopRun, retry_scheduled: bool) -> String {
    let failed = group_by_stage(
        run.failures
            .iter()
            .map(|failure| (failure.stage, failure.label.as_str())),
    );
    let mut summary = vec![format!(
        "Stop hooks failed: {}.",
        format_stage_groups(&failed)
    )];
    if let Some(stopped) = &run.stopped
        && !stopped.not_run.is_empty()
    {
        summary.push(format!(
            "Not run because a stage {} hook failed: {}.",
            stopped.stage,
            format_stage_groups(&stopped.not_run)
        ));
        if retry_scheduled {
            summary.push(format!(
                "One retry is scheduled: at the next stop, the reported hooks up to stage {} \
                 run again, and the stages that were not run start if they pass.",
                stopped.stage
            ));
        }
    }
    let details: Vec<&str> = run
        .failures
        .iter()
        .map(|failure| failure.reason.as_str())
        .collect();
    format!("{}\n\n{}", summary.join("\n"), details.join("\n\n"))
}

/// 再試行が後続を止める失敗で終わったときの、ユーザー向け通知を組み立てる。
fn retry_failed_notice(run: &StopRun, stopped: &GateStop) -> String {
    let mut failed = Vec::new();
    for failure in run
        .failures
        .iter()
        .filter(|failure| failure.stage == stopped.stage && failure.gate)
    {
        push_unique(&mut failed, failure.label.clone());
    }
    let mut notice = format!(
        "claw-hooks: stop hook retry failed at stage {} [{}].",
        stopped.stage,
        format_labels(&failed)
    );
    if !stopped.not_run.is_empty() {
        notice.push_str(&format!(
            " Not run: {}.",
            format_stage_groups(&stopped.not_run)
        ));
    }
    notice.push_str(" No further retry is scheduled.");
    notice
}

/// 再試行が後続を止める失敗で終わったときの、NanoBuddy の吹き出しの文面を組み立てる。
fn retry_failed_speech(stopped: &GateStop) -> String {
    let mut labels = Vec::new();
    for (_, stage_labels) in &stopped.not_run {
        for label in stage_labels {
            push_unique(&mut labels, label.clone());
        }
    }
    if labels.is_empty() {
        return format!("Stop hook retry failed at stage {}", stopped.stage);
    }
    labels.truncate(MAX_SPEECH_LABELS);
    format!("Stop hook retry failed: {} not run", labels.join(", "))
}

impl Filter for StopHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // Stop イベントにのみ適用
        input.event == HookEvent::Stop
    }

    fn execute(&self, input: &HookInput) -> Decision {
        self.execute_stop(input, std::env::var(STOP_ACTIVE_ENV).is_ok())
    }

    fn priority(&self) -> u32 {
        super::priority::STOP // 低優先度 - 他のフィルターの後に実行
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_stop_hook_filter_applies_to_stop_event() {
        let hooks = vec![StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let bash_input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "ls".to_string(),
                timeout: None,
                cwd: None,
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
            gate: None,
            session_scope: Default::default(),
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

    // === エッジケースのテスト ===

    #[test]
    fn test_execute_command_empty_is_error() {
        assert!(StopHookFilter::execute_command("   ", 60, None).is_err());
    }

    #[test]
    fn test_command_label_does_not_expose_arguments_or_directories() {
        assert_eq!(
            StopHookFilter::command_label(
                "/Users/private/bin/tool --token top-secret --path /private/project"
            ),
            "tool"
        );
        assert_eq!(
            StopHookFilter::command_label("sh -c 'curl -H Authorization:top-secret'"),
            "sh"
        );
        assert_eq!(StopHookFilter::command_label("   "), "<empty>");
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
            gate: None,
            session_scope: Default::default(),
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

    // === ループ防止のテスト ===

    #[test]
    fn test_stop_hook_active_true_skips_all_hooks() {
        // 継続中の停止は、再試行の予定が記録されていなければ何も実行しない
        // （この filter は状態を持たないので予定は無い）
        let hooks = vec![StopHook {
            commands: vec!["echo should-not-run".to_string()],
            condition: None,
            stage: None,
            report: None,
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
        }]
    }

    // === セッション種別（メイン / 委譲エージェント）のテスト ===

    /// 指定したセッション種別の Stop 入力を作るテストヘルパー。
    fn make_stop_input_with_session_kind(kind: StopSessionKind) -> HookInput {
        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                session_kind: kind,
                ..Default::default()
            }),
            session_id: None,
        }
    }

    /// 指定した session_scope で失敗する report=true フックを作るテストヘルパー。
    /// 実行されれば Block、スキップされれば Allow になる。
    fn make_failing_report_hook_with_scope(
        scope: crate::config::StopSessionScope,
    ) -> Vec<StopHook> {
        vec![StopHook {
            commands: vec!["sh -c 'echo scope-test-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: Some(true),
            gate: None,
            session_scope: scope,
        }]
    }

    #[test]
    fn test_delegated_session_skips_primary_scope_hooks() {
        // teammate 等のエージェントセッションでは、デフォルト（primary）のフックは
        // 実行されない（失敗するフックでも Allow = スキップの証拠）
        use crate::config::StopSessionScope;
        let filter = StopHookFilter::new(
            make_failing_report_hook_with_scope(StopSessionScope::Primary),
            false,
            10,
        );
        let decision = filter.execute(&make_stop_input_with_session_kind(
            StopSessionKind::Delegated,
        ));
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "delegated session should skip primary-scope hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_delegated_session_runs_all_scope_hooks() {
        use crate::config::StopSessionScope;
        let filter = StopHookFilter::new(
            make_failing_report_hook_with_scope(StopSessionScope::All),
            false,
            10,
        );
        let decision = filter.execute(&make_stop_input_with_session_kind(
            StopSessionKind::Delegated,
        ));
        assert!(
            matches!(decision, Decision::Block { .. }),
            "delegated session should run all-scope hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_delegated_session_runs_delegated_scope_hooks() {
        use crate::config::StopSessionScope;
        let filter = StopHookFilter::new(
            make_failing_report_hook_with_scope(StopSessionScope::Delegated),
            false,
            10,
        );
        let decision = filter.execute(&make_stop_input_with_session_kind(
            StopSessionKind::Delegated,
        ));
        assert!(
            matches!(decision, Decision::Block { .. }),
            "delegated session should run delegated-scope hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_primary_session_skips_delegated_scope_hooks() {
        use crate::config::StopSessionScope;
        let filter = StopHookFilter::new(
            make_failing_report_hook_with_scope(StopSessionScope::Delegated),
            false,
            10,
        );
        let decision = filter.execute(&make_stop_input_with_session_kind(StopSessionKind::Primary));
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "primary session should skip delegated-scope hooks, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_primary_session_runs_primary_scope_hooks() {
        use crate::config::StopSessionScope;
        let filter = StopHookFilter::new(
            make_failing_report_hook_with_scope(StopSessionScope::Primary),
            false,
            10,
        );
        let decision = filter.execute(&make_stop_input_with_session_kind(StopSessionKind::Primary));
        assert!(
            matches!(decision, Decision::Block { .. }),
            "primary session should run primary-scope hooks (default behavior), got: {:?}",
            decision
        );
    }

    #[test]
    fn test_loop_count_one_skips_all_hooks() {
        // Cursor: loop_count = 1 は初回の Block によるフォローアップ後の停止。再試行の予定が
        // 記録されていなければ（この filter は状態を持たない）、失敗するフックでも実行されず
        // Allow になる（無限ループ防止）
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
            gate: None,
            session_scope: Default::default(),
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

    // === 条件付きフックのテスト ===

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

    /// `sh -c '...'` のスクリプトに埋め込むパス。スクリプトの中では `\` がエスケープになるので、
    /// Windows のパスは `/` 区切りにする (sh は `C:/...` を読める)。フックのコマンド文字列を囲む
    /// `'` はエスケープする。
    fn sh_script_path(path: &std::path::Path) -> String {
        path.to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''")
    }

    #[test]
    fn test_conditional_hook_file_not_found_skips() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["false".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("nonexistent-file-xyz-abc.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
                gate: None,
                session_scope: Default::default(),
            },
            StopHook {
                commands: vec!["sh -c 'echo second-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
                gate: None,
                session_scope: Default::default(),
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
                gate: None,
                session_scope: Default::default(),
            },
            StopHook {
                commands: vec!["true".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
                gate: None,
                session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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

    // === タイムアウトのテスト ===

    #[test]
    fn test_execute_command_timeout_kills_process() {
        // sleep 10 は2秒のタイムアウト後に終了させる
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
        // 無条件フックは非同期実行のため、タイムアウトしても警告のみで許可する
        let hooks = vec![StopHook {
            commands: vec!["sleep 10".to_string()],
            condition: None,
            stage: None,
            report: None,
            gate: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None, // condition あり → should_report() = true
            gate: None,
            session_scope: Default::default(),
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
        // ブロック回避のため読み取りスレッドを join しないので、タイムアウト時の stdout は空になる
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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None, // condition あり → should_report() = true
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
        let marker_path = sh_script_path(&marker);
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
        // 個別指定した3秒のタイムアウト内に、1秒のコマンドが成功することを確認する
        let hooks = vec![StopHook {
            commands: vec!["sleep 1".to_string()],
            condition: None,
            stage: None,
            report: None,
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: Some(true),
                gate: None,
                session_scope: Default::default(),
            },
            StopHook {
                commands: vec!["sh -c 'echo stage5-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(5),
                report: Some(true),
                gate: None,
                session_scope: Default::default(),
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

    // === エージェントメッセージ伝播のテスト ===

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
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
            gate: None,
            session_scope: Default::default(),
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

    // === ステージ順序のテスト ===

    #[test]
    fn test_stage_ordering_lower_stage_runs_first() {
        use crate::config::HookCondition;
        // ステージ1でマーカーファイルを作り、ステージ3で存在を確認する。
        // ステージ順が正しければ、ステージ3の実行時点でマーカーが存在する。
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-stage-order-{}", std::process::id()));
        let marker_path = sh_script_path(&marker);
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![
            StopHook {
                commands: vec![format!("sh -c 'echo done > {}'", marker_path)],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: None,
                gate: None,
                session_scope: Default::default(),
            },
            StopHook {
                commands: vec![format!(
                    "sh -c 'test -f {} || (echo stage-order-failed >&2; exit 1)'",
                    marker_path
                )],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(3),
                report: Some(true),
                gate: None,
                session_scope: Default::default(),
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

    // === report 動作のテスト ===

    #[test]
    fn test_report_false_ignores_failure() {
        use crate::config::HookCondition;
        // report=falseのフックは失敗してもブロックしないべき
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo report-off-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: Some(false),
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
        // 条件も明示的な report もないフックは false が既定値になる
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo no-report-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: None,
            gate: None,
            session_scope: Default::default(),
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
        // 条件があり明示的な report がないフックは true が既定値になる
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo default-report-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: None,
            gate: None,
            session_scope: Default::default(),
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
        let marker_path = sh_script_path(&marker);
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![StopHook {
            commands: vec![format!(
                "sh -c 'echo ff-output; echo done > {}'",
                marker_path
            )],
            condition: None,
            stage: None,
            report: Some(false),
            gate: None,
            session_scope: Default::default(),
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
        let marker_path = sh_script_path(&marker);
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![StopHook {
            commands: vec![format!("sh -c 'sleep 1; echo done > {}'", marker_path)],
            condition: None,
            stage: None,
            report: Some(false),
            gate: None,
            session_scope: Default::default(),
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
            gate: None,
            session_scope: Default::default(),
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
        // ステージ1: report=false（非同期実行）
        // ステージ3: report=true（ブロック対象）
        let hooks = vec![
            StopHook {
                commands: vec!["sh -c 'echo stage1-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: Some(false),
                gate: None,
                session_scope: Default::default(),
            },
            StopHook {
                commands: vec!["sh -c 'echo stage3-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(3),
                report: Some(true),
                gate: None,
                session_scope: Default::default(),
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
        // CLAW_HOOKS_STOP_ACTIVE=1 が設定されている場合、すべてのフックがスキップされる。
        // 環境変数の有無は `execute_stop` へ直接渡す。プロセスの環境変数を書き換えると、
        // 並列に走る他のテストの停止までスキップされて不安定になる（実際の環境変数を読む経路は
        // tests/stop_hooks_test.rs が子プロセスで確かめる）。
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo should-not-run >&2; exit 1'".to_string()],
            condition: Some(crate::config::HookCondition {
                command_exists: None,
                file_not_exists: None,
                command_not_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: Some(true),
            gate: None,
            session_scope: Default::default(),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute_stop(&make_stop_input(), true);

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "環境変数によりフックがスキップされ Allow を返すべき, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_stop_active_env_skips_scheduled_retry() {
        // 環境変数は再試行より優先する。予定が記録されていても実行せず、予定も消費しない。
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        assert!(matches!(
            filter.execute(&stop_input_for(Some("s1"), false)),
            Decision::Block { .. }
        ));
        fx.fix();

        let notice = expect_allow(filter.execute_stop(&stop_input_for(Some("s1"), true), true));
        assert_eq!(notice, None);
        assert_eq!(fx.checks(), 1, "検査は再実行されない");

        // 予定は残っているので、環境変数の無い継続中の停止では再試行される
        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(notice, None);
        assert_eq!(fx.checks(), 2);
        assert!(fx.wait_for_effects(1), "後続の stage が起動するべき");
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

    /// 終了コード 1 の `ExitStatus` を作る。`from_raw` の引数は OS ごとに意味が違う
    /// (Unix は wait のステータスで終了コードは上位バイト、Windows は終了コードそのもの)。
    fn exit_code_one() -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(256)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(1)
        }
    }

    #[test]
    fn test_build_reason_stdout_only() {
        let output = std::process::Output {
            status: exit_code_one(),
            stdout: b"lint error found".to_vec(),
            stderr: Vec::new(),
        };
        let reason = StopHookFilter::build_reason("cargo clippy", &output);
        assert!(reason.contains("lint error found"));
        assert!(reason.contains("cargo clippy"));
    }

    #[test]
    fn test_build_reason_stderr_only() {
        let output = std::process::Output {
            status: exit_code_one(),
            stdout: Vec::new(),
            stderr: b"compile error".to_vec(),
        };
        let reason = StopHookFilter::build_reason("cargo build", &output);
        assert!(reason.contains("compile error"));
    }

    #[test]
    fn test_build_reason_both_stdout_and_stderr() {
        let output = std::process::Output {
            status: exit_code_one(),
            stdout: b"stdout content".to_vec(),
            stderr: b"stderr content".to_vec(),
        };
        let reason = StopHookFilter::build_reason("cmd", &output);
        assert!(reason.contains("stdout content"));
        assert!(reason.contains("stderr content"));
    }

    #[test]
    fn test_build_reason_empty_output() {
        let output = std::process::Output {
            status: exit_code_one(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let reason = StopHookFilter::build_reason("cmd", &output);
        assert!(reason.contains("Stop hook failed: cmd"));
    }

    // === gate（失敗した stage より後を実行しない）と再停止での再試行のテスト ===
    //
    // 「フックが動いた回数」を 1 つにまとめず、検査の実行回数・後続（副作用）の起動回数・
    // Block の有無・ユーザー向け通知の有無を別々に確かめる。回数はマーカーファイルの行数で数える。

    /// Allow を期待し、ユーザー向け通知を返す（Block なら失敗）。
    fn expect_allow(decision: Decision) -> Option<String> {
        match decision {
            Decision::Allow {
                additional_context,
                user_notice,
            } => {
                assert!(
                    additional_context.is_none(),
                    "Stop フックはエージェント向けの補足を返さない: {additional_context:?}"
                );
                user_notice
            }
            Decision::Block { message } => panic!("Allow を期待したが Block: {message}"),
        }
    }

    /// Block を期待し、文面を返す（Allow なら失敗）。
    fn expect_block(decision: Decision) -> String {
        match decision {
            Decision::Block { message } => message,
            other => panic!("Block を期待したが {other:?}"),
        }
    }

    /// 再試行を予定できるエージェント（Claude Code）。
    fn retry_agent() -> AgentProfile {
        AgentProfile {
            id: "claude-code",
            pre_command_context: true,
            stop_retry: true,
        }
    }

    /// セッション ID と stop_hook_active を指定した Stop 入力（Claude Code / Codex 相当）。
    fn stop_input_for(session_id: Option<&str>, stop_hook_active: bool) -> HookInput {
        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                stop_hook_active,
                ..Default::default()
            }),
            session_id: session_id.map(str::to_string),
        }
    }

    /// セッション ID と loop_count を指定した Stop 入力（Cursor 相当）。
    fn cursor_stop_input(session_id: &str, loop_count: u32) -> HookInput {
        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                loop_count: Some(loop_count),
                ..Default::default()
            }),
            session_id: Some(session_id.to_string()),
        }
    }

    /// stage・report・gate を指定した、条件なしのフック。
    fn stage_hook(command: String, stage: u8, report: bool, gate: Option<bool>) -> StopHook {
        StopHook {
            commands: vec![command],
            condition: None,
            stage: Some(stage),
            report: Some(report),
            gate,
            session_scope: Default::default(),
        }
    }

    /// 実行のたびにマーカーへ 1 行追記するコマンド（起動回数を数える）。
    fn count_command(marker: &Path) -> String {
        format!("sh -c 'echo run >> {}'", sh_script_path(marker))
    }

    /// 実行回数をマーカーに数え、`ok` があれば成功・無ければ `check-failed` を出して失敗する検査。
    fn check_command(counter: &Path, ok: &Path) -> String {
        format!(
            "sh -c 'echo run >> {}; test -f {} || {{ echo check-failed >&2; exit 1; }}'",
            sh_script_path(counter),
            sh_script_path(ok)
        )
    }

    /// 実行回数をマーカーに数え、常に `always-failed` を出して失敗するコマンド。
    fn failing_count_command(counter: &Path) -> String {
        format!(
            "sh -c 'echo run >> {}; echo always-failed >&2; exit 1'",
            sh_script_path(counter)
        )
    }

    /// マーカーの行数（実行回数）。マーカーが無ければ 0。
    fn run_count(marker: &Path) -> usize {
        std::fs::read_to_string(marker)
            .map(|contents| contents.lines().count())
            .unwrap_or(0)
    }

    /// detached のフックの起動を、マーカーの行数が `expected` に届くまで待つ。
    fn wait_for_runs(marker: &Path, expected: usize) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            if run_count(marker) >= expected {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        run_count(marker) >= expected
    }

    /// detached のフックの起動回数が `expected` のまま増えないことを確かめる。
    ///
    /// 起動しなかったことは「待っても現れない」でしか確かめられず、固定の時間だけ待つと遅い
    /// マシンでは確認が空振りする。判定の後に同じ経路（detached の stop フック）で対照の
    /// マーカーを作らせ、対照が現れてからさらに猶予を置いて数える。対象が判定の中で起動されて
    /// いれば、後から起動した対照が現れる頃には書き終えている。
    fn assert_detached_runs_settled(marker: &Path, expected: usize, scratch: &Path) {
        static CONTROL_SEQ: AtomicUsize = AtomicUsize::new(0);
        let control = scratch.join(format!(
            "control-{}.log",
            CONTROL_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let control_filter = StopHookFilter::new(
            vec![stage_hook(count_command(&control), 5, false, None)],
            false,
            10,
        );
        expect_allow(control_filter.execute(&make_stop_input()));
        assert!(
            wait_for_runs(&control, 1),
            "対照の detached フックが起動しなかった"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(run_count(marker), expected, "detached のフックの起動回数");
    }

    /// 検査（stage 1、report = true、gate は既定）と副作用（stage 5、report = false）の 2 段構成。
    /// 検査は `ok` ファイルがあれば通る（`fix` がエージェントの修正に相当する）。
    struct GateFixture {
        dir: tempfile::TempDir,
    }

    impl GateFixture {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().unwrap(),
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn check_hook(&self) -> StopHook {
            stage_hook(
                check_command(&self.path("checks.log"), &self.path("ok")),
                1,
                true,
                None,
            )
        }

        fn effect_hook(&self) -> StopHook {
            stage_hook(count_command(&self.path("effects.log")), 5, false, None)
        }

        fn hooks(&self) -> Vec<StopHook> {
            vec![self.check_hook(), self.effect_hook()]
        }

        fn state(&self) -> StateStore {
            StateStore::at(self.path("state"))
        }

        fn filter_for(&self, hooks: Vec<StopHook>, agent: AgentProfile) -> StopHookFilter {
            StopHookFilter::new(hooks, false, 10).with_state(agent, self.state())
        }

        fn filter_with_state(&self) -> StopHookFilter {
            self.filter_for(self.hooks(), retry_agent())
        }

        /// 検査が通るようにする（エージェントが問題を直したことに相当）。
        fn fix(&self) {
            std::fs::write(self.path("ok"), "").unwrap();
        }

        /// 検査の実行回数。
        fn checks(&self) -> usize {
            run_count(&self.path("checks.log"))
        }

        /// 副作用の起動を `expected` 回まで待つ。
        fn wait_for_effects(&self, expected: usize) -> bool {
            wait_for_runs(&self.path("effects.log"), expected)
        }

        /// 副作用の起動回数が `expected` のまま増えないことを確かめる。
        fn assert_effects_settled(&self, expected: usize) {
            assert_detached_runs_settled(&self.path("effects.log"), expected, self.dir.path());
        }
    }

    #[test]
    fn test_gate_failure_does_not_start_later_detached_stage() {
        // stage 1 の report = true が失敗 → stage 5 の report = false（副作用）を起動しない
        let fx = GateFixture::new();
        let filter = StopHookFilter::new(fx.hooks(), false, 10);
        let message = expect_block(filter.execute(&make_stop_input()));
        assert!(message.contains("check-failed"), "{message}");
        assert_eq!(fx.checks(), 1, "検査は 1 回実行される");
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_gate_success_starts_later_stage() {
        // stage 1 の report = true が成功 → stage 5 を起動する
        let fx = GateFixture::new();
        fx.fix();
        let filter = StopHookFilter::new(fx.hooks(), false, 10);
        assert_eq!(expect_allow(filter.execute(&make_stop_input())), None);
        assert_eq!(fx.checks(), 1);
        assert!(fx.wait_for_effects(1), "stage 5 が起動するべき");
        fx.assert_effects_settled(1);
    }

    #[test]
    fn test_one_of_two_reported_hooks_failing_stops_later_stage() {
        // stage 1 に report = true が 2 つあり片方だけ失敗 → 2 つとも実行され、失敗は 1 件、
        // stage 5 は起動しない
        let fx = GateFixture::new();
        fx.fix();
        let failing = stage_hook(
            failing_count_command(&fx.path("failing.log")),
            1,
            true,
            None,
        );
        let filter =
            StopHookFilter::new(vec![fx.check_hook(), failing, fx.effect_hook()], false, 10);
        let message = expect_block(filter.execute(&make_stop_input()));
        assert_eq!(fx.checks(), 1, "通る検査も最後まで実行される");
        assert_eq!(
            run_count(&fx.path("failing.log")),
            1,
            "失敗する検査も実行される"
        );
        assert_eq!(
            message.matches("Stop hook failed: ").count(),
            1,
            "失敗は 1 件: {message}"
        );
        assert!(message.contains("always-failed"), "{message}");
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_report_false_failure_does_not_gate_later_stage() {
        // stage 1 の report = false が失敗 → stage 5 を起動する（detached は結果を取らない）
        let fx = GateFixture::new();
        let detached_failure = stage_hook(
            failing_count_command(&fx.path("failing.log")),
            1,
            false,
            None,
        );
        let filter = StopHookFilter::new(vec![detached_failure, fx.effect_hook()], false, 10);
        assert_eq!(expect_allow(filter.execute(&make_stop_input())), None);
        assert!(wait_for_runs(&fx.path("failing.log"), 1));
        assert!(fx.wait_for_effects(1), "stage 5 が起動するべき");
    }

    #[test]
    fn test_gate_false_failure_blocks_but_starts_later_stage() {
        // stage 1 の gate = false の report = true が失敗 → Block は返すが、stage 5 は起動する
        let fx = GateFixture::new();
        let mut advisory = fx.check_hook();
        advisory.gate = Some(false);
        let filter = StopHookFilter::new(vec![advisory, fx.effect_hook()], false, 10);
        let message = expect_block(filter.execute(&make_stop_input()));
        assert_eq!(
            message, "Stop hooks failed: stage 1 [sh].\n\nStop hook failed: sh\ncheck-failed",
            "実行しなかった stage が無いので 2 行目は出さない"
        );
        assert_eq!(fx.checks(), 1);
        assert!(
            fx.wait_for_effects(1),
            "gate = false の失敗では後続を止めない"
        );
    }

    #[test]
    fn test_block_message_starts_with_summary_of_failed_and_not_run_stages() {
        // Block の文面の先頭に、失敗した stage と、実行しなかった stage とラベルが入る。
        // 実行しなかった stage のフックは起動しないので、存在しないプログラム名にしてある。
        let fx = GateFixture::new();
        let hooks = vec![
            fx.check_hook(),
            stage_hook("claw-hooks-test-unrun-a --flag".to_string(), 3, true, None),
            stage_hook(
                "/opt/tools/claw-hooks-test-unrun-b --all --yes".to_string(),
                5,
                false,
                None,
            ),
        ];
        let filter = StopHookFilter::new(hooks, false, 10);
        let message = expect_block(filter.execute(&make_stop_input()));
        assert_eq!(
            message,
            "Stop hooks failed: stage 1 [sh].\n\
             Not run because a stage 1 hook failed: stage 3 [claw-hooks-test-unrun-a]; \
             stage 5 [claw-hooks-test-unrun-b].\n\
             \n\
             Stop hook failed: sh\ncheck-failed"
        );
    }

    #[test]
    fn test_condition_is_evaluated_right_before_its_stage() {
        // 前の stage が作ったファイルを、後の stage の条件に使える
        let fx = GateFixture::new();
        let created = fx.path("created-by-stage-1");
        let producer = stage_hook(
            format!("sh -c 'echo x > {}'", sh_script_path(&created)),
            1,
            true,
            None,
        );
        let mut consumer = stage_hook(count_command(&fx.path("consumer.log")), 3, true, None);
        consumer.condition = Some(crate::config::HookCondition {
            file_exists: Some(created.to_string_lossy().into_owned()),
            ..Default::default()
        });
        let filter = StopHookFilter::new(vec![producer, consumer], false, 10);
        assert_eq!(expect_allow(filter.execute(&make_stop_input())), None);
        assert_eq!(
            run_count(&fx.path("consumer.log")),
            1,
            "stage 3 の条件は stage 1 の後に評価される"
        );
    }

    /// 評価すると `NotADirectory` になる条件（通常ファイルの下のパスを指す）。
    #[cfg(unix)]
    fn unevaluable_condition(fx: &GateFixture) -> crate::config::HookCondition {
        let file = fx.path("regular-file");
        std::fs::write(&file, "").unwrap();
        crate::config::HookCondition {
            file_exists: Some(file.join("child").to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_condition_error_on_gate_hook_fails_and_stops_later_stages() {
        // 後続を止めるフックの条件を評価できない → 実行せずに失敗とし、後続の stage も止める
        let fx = GateFixture::new();
        fx.fix();
        let mut check = fx.check_hook();
        check.condition = Some(unevaluable_condition(&fx));
        let filter = StopHookFilter::new(vec![check, fx.effect_hook()], false, 10);
        let message = expect_block(filter.execute(&make_stop_input()));
        assert_eq!(
            message,
            "Stop hooks failed: stage 1 [sh].\n\
             Not run because a stage 1 hook failed: stage 5 [sh].\n\
             \n\
             Stop hook failed: sh\ncould not evaluate condition.file_exists (not a directory)"
        );
        assert_eq!(fx.checks(), 0, "条件を評価できないフックは実行しない");
        fx.assert_effects_settled(0);
    }

    #[cfg(unix)]
    #[test]
    fn test_condition_error_on_non_gate_hooks_skips_them() {
        // gate = false の検査と report = false のフックは、条件を評価できなければスキップする
        // （失敗にはせず、後続も止めない）
        let fx = GateFixture::new();
        let mut advisory = stage_hook(
            count_command(&fx.path("advisory.log")),
            1,
            true,
            Some(false),
        );
        advisory.condition = Some(unevaluable_condition(&fx));
        let mut detached = stage_hook(count_command(&fx.path("detached.log")), 1, false, None);
        detached.condition = Some(unevaluable_condition(&fx));
        let filter = StopHookFilter::new(vec![advisory, detached, fx.effect_hook()], false, 10);
        assert_eq!(expect_allow(filter.execute(&make_stop_input())), None);
        assert_eq!(run_count(&fx.path("advisory.log")), 0);
        assert!(fx.wait_for_effects(1), "後続は止めない");
        assert_detached_runs_settled(&fx.path("detached.log"), 0, fx.dir.path());
    }

    #[cfg(unix)]
    #[test]
    fn test_not_run_lists_hooks_whose_condition_holds_or_cannot_be_evaluated() {
        // 実行しなかったフックとして挙げるのは、条件を今評価して満たすか評価できないものだけ
        let fx = GateFixture::new();
        let mut unmet = stage_hook("claw-hooks-test-unmet".to_string(), 3, false, None);
        unmet.condition = Some(crate::config::HookCondition {
            file_exists: Some(fx.path("missing").to_string_lossy().into_owned()),
            ..Default::default()
        });
        let mut unknown = stage_hook("claw-hooks-test-unknown".to_string(), 4, false, None);
        unknown.condition = Some(unevaluable_condition(&fx));
        let plain = stage_hook("claw-hooks-test-plain".to_string(), 5, false, None);
        let filter = StopHookFilter::new(vec![fx.check_hook(), unmet, unknown, plain], false, 10);
        let message = expect_block(filter.execute(&make_stop_input()));
        assert!(
            message.starts_with(
                "Stop hooks failed: stage 1 [sh].\n\
                 Not run because a stage 1 hook failed: stage 4 [claw-hooks-test-unknown]; \
                 stage 5 [claw-hooks-test-plain].\n\n"
            ),
            "{message}"
        );
    }

    #[test]
    fn test_retry_after_fix_starts_skipped_stage_without_block() {
        // 再停止（初回の Block の後の stop_hook_active = true）で検査が通る
        // → stage 5 を起動し、Block しない
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();

        let message = expect_block(filter.execute(&stop_input_for(Some("s1"), false)));
        assert!(
            message.starts_with(
                "Stop hooks failed: stage 1 [sh].\n\
                 Not run because a stage 1 hook failed: stage 5 [sh].\n\
                 One retry is scheduled: at the next stop, the reported hooks up to stage 1 run \
                 again, and the stages that were not run start if they pass.\n\n"
            ),
            "{message}"
        );
        assert_eq!(fx.checks(), 1);
        fx.assert_effects_settled(0);

        fx.fix();
        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(notice, None, "再試行が通れば通知しない");
        assert_eq!(fx.checks(), 2, "検査をやり直す");
        assert!(fx.wait_for_effects(1), "実行しなかった stage を起動する");

        // 再試行は 1 回だけ（予定は消費済み）
        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(notice, None);
        assert_eq!(fx.checks(), 2, "2 回目の継続中の停止では何も実行しない");
        fx.assert_effects_settled(1);
    }

    #[test]
    fn test_retry_failure_does_not_start_skipped_stage_and_notifies_without_block() {
        // 再停止で検査が通らない → stage 5 を起動せず、Block もしない（通知だけ）
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        expect_block(filter.execute(&stop_input_for(Some("s1"), false)));

        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(
            notice.as_deref(),
            Some(
                "claw-hooks: stop hook retry failed at stage 1 [sh]. Not run: stage 5 [sh]. \
                 No further retry is scheduled."
            )
        );
        assert_eq!(fx.checks(), 2, "検査をやり直す");
        fx.assert_effects_settled(0);

        // 再試行は 1 回だけ
        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(notice, None);
        assert_eq!(fx.checks(), 2);
    }

    #[test]
    fn test_retry_failure_at_later_stage_reports_remaining_stages() {
        // 再試行で、初回に実行しなかった stage の検査が失敗 → それより後は起動しない
        let fx = GateFixture::new();
        let later_check = stage_hook(failing_count_command(&fx.path("later.log")), 3, true, None);
        let filter = fx.filter_for(
            vec![fx.check_hook(), later_check, fx.effect_hook()],
            retry_agent(),
        );
        expect_block(filter.execute(&stop_input_for(Some("s1"), false)));
        assert_eq!(run_count(&fx.path("later.log")), 0);

        fx.fix();
        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(
            notice.as_deref(),
            Some(
                "claw-hooks: stop hook retry failed at stage 3 [sh]. Not run: stage 5 [sh]. \
                 No further retry is scheduled."
            )
        );
        assert_eq!(fx.checks(), 2);
        assert_eq!(run_count(&fx.path("later.log")), 1);
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_retry_reruns_only_gate_hooks_up_to_failed_stage() {
        // 再試行では、失敗した stage までは後続を止めるフックだけをやり直す。
        // gate = false の検査・report = false のフック（初回に実行・報告済み）は繰り返さない
        let fx = GateFixture::new();
        let advisory = stage_hook(
            failing_count_command(&fx.path("advisory.log")),
            1,
            true,
            Some(false),
        );
        let detached = stage_hook(count_command(&fx.path("detached.log")), 1, false, None);
        let filter = fx.filter_for(
            vec![fx.check_hook(), advisory, detached, fx.effect_hook()],
            retry_agent(),
        );

        let message = expect_block(filter.execute(&stop_input_for(Some("s1"), false)));
        assert!(
            message.starts_with("Stop hooks failed: stage 1 [sh].\n"),
            "{message}"
        );
        assert_eq!(
            message.matches("Stop hook failed: ").count(),
            2,
            "{message}"
        );
        assert_eq!(fx.checks(), 1);
        assert_eq!(run_count(&fx.path("advisory.log")), 1);
        assert!(wait_for_runs(&fx.path("detached.log"), 1));

        fx.fix();
        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("s1"), true))),
            None
        );
        assert_eq!(fx.checks(), 2, "後続を止める検査はやり直す");
        assert_eq!(
            run_count(&fx.path("advisory.log")),
            1,
            "gate = false の検査はやり直さない"
        );
        assert!(fx.wait_for_effects(1));
        assert_detached_runs_settled(&fx.path("detached.log"), 1, fx.dir.path());
    }

    #[test]
    fn test_retry_non_gate_failure_is_logged_without_notice() {
        // 再試行の中の gate = false の失敗はログだけ（Block も通知もしない）
        let fx = GateFixture::new();
        let advisory = stage_hook(
            failing_count_command(&fx.path("advisory.log")),
            3,
            true,
            Some(false),
        );
        let filter = fx.filter_for(
            vec![fx.check_hook(), advisory, fx.effect_hook()],
            retry_agent(),
        );
        let message = expect_block(filter.execute(&stop_input_for(Some("s1"), false)));
        assert!(
            message.contains("Not run because a stage 1 hook failed: stage 3 [sh]; stage 5 [sh]."),
            "{message}"
        );
        assert_eq!(run_count(&fx.path("advisory.log")), 0);

        fx.fix();
        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("s1"), true))),
            None
        );
        assert_eq!(
            run_count(&fx.path("advisory.log")),
            1,
            "実行しなかった stage の検査を実行する"
        );
        assert!(
            fx.wait_for_effects(1),
            "gate = false の失敗では後続を止めない"
        );
    }

    #[test]
    fn test_continuing_stop_without_record_runs_nothing() {
        // 記録の無い継続中の停止（他のフックが作業を続けさせた等）は何も実行しない
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("s1"), true))),
            None
        );
        assert_eq!(fx.checks(), 0);
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_retry_skipped_with_notice_when_config_changed() {
        // 初回の停止の後に設定が変わった → 再試行せず（フックは動かさない）、通知する
        let fx = GateFixture::new();
        expect_block(
            fx.filter_with_state()
                .execute(&stop_input_for(Some("s1"), false)),
        );
        fx.fix();

        let mut changed = fx.hooks();
        changed[1].stage = Some(4);
        let filter = fx.filter_for(changed, retry_agent());
        let notice = expect_allow(filter.execute(&stop_input_for(Some("s1"), true)));
        assert_eq!(notice.as_deref(), Some(RETRY_SKIPPED_NOTICE));
        assert_eq!(fx.checks(), 1, "フックは動かさない");
        fx.assert_effects_settled(0);

        // 予定は消費済みなので、次の継続中の停止では通知もしない
        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("s1"), true))),
            None
        );
    }

    #[test]
    fn test_corrupt_retry_record_skips_retry_with_notice() {
        let fx = GateFixture::new();
        let key = StateStore::key(&[RETRY_KEY_VERSION, "claude-code", "s1"]);
        assert!(
            fx.state()
                .put(RETRY_STATE_KIND, &key, b"{not json", RETRY_MAX_AGE)
        );
        let notice = expect_allow(
            fx.filter_with_state()
                .execute(&stop_input_for(Some("s1"), true)),
        );
        assert_eq!(notice.as_deref(), Some(RETRY_SKIPPED_NOTICE));
        assert_eq!(fx.checks(), 0);
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_schedule_retry_records_only_version_stage_and_fingerprint() {
        // 記録の中身は形式の版・失敗した stage・設定の指紋だけ（コマンドやセッション ID を持たない）
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        expect_block(filter.execute(&stop_input_for(Some("s1"), false)));
        let key = StateStore::key(&[RETRY_KEY_VERSION, "claude-code", "s1"]);
        let contents = fx
            .state()
            .take(RETRY_STATE_KIND, &key)
            .expect("再試行の予定が記録される");
        let record: serde_json::Value = serde_json::from_slice(&contents).unwrap();
        assert_eq!(
            record,
            serde_json::json!({
                "version": 1,
                "failed_stage": 1,
                "fingerprint": filter.config_fingerprint(),
            })
        );
    }

    #[test]
    fn test_scheduled_retry_stage_rejects_mismatched_records() {
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        let fingerprint = filter.config_fingerprint();
        let record = |version: u32, fingerprint: &str| {
            format!(r#"{{"version":{version},"failed_stage":3,"fingerprint":"{fingerprint}"}}"#)
        };
        assert_eq!(
            filter.scheduled_retry_stage(record(1, &fingerprint).as_bytes()),
            Some(3)
        );
        assert_eq!(
            filter.scheduled_retry_stage(record(2, &fingerprint).as_bytes()),
            None,
            "形式の版が違う"
        );
        assert_eq!(
            filter.scheduled_retry_stage(record(1, "0000000000000000").as_bytes()),
            None,
            "設定の指紋が違う"
        );
        let extra = format!(
            r#"{{"version":1,"failed_stage":3,"fingerprint":"{fingerprint}","extra":true}}"#
        );
        assert_eq!(
            filter.scheduled_retry_stage(extra.as_bytes()),
            None,
            "未知のフィールド"
        );
        assert_eq!(filter.scheduled_retry_stage(b""), None, "空");
    }

    #[test]
    fn test_cursor_loop_count_one_retries_scheduled_stop() {
        // Cursor: loop_count = 1 + 予定あり → 再試行
        let fx = GateFixture::new();
        let cursor = AgentProfile {
            id: "cursor",
            pre_command_context: false,
            stop_retry: true,
        };
        let filter = fx.filter_for(fx.hooks(), cursor);
        let message = expect_block(filter.execute(&cursor_stop_input("c1", 0)));
        assert!(message.contains("One retry is scheduled"), "{message}");

        fx.fix();
        assert_eq!(
            expect_allow(filter.execute(&cursor_stop_input("c1", 1))),
            None
        );
        assert_eq!(fx.checks(), 2);
        assert!(fx.wait_for_effects(1), "実行しなかった stage を起動する");
    }

    #[test]
    fn test_cursor_loop_count_two_skips_even_with_record() {
        // Cursor: loop_count = 2 → 予定があっても何も実行しない
        let fx = GateFixture::new();
        let cursor = AgentProfile {
            id: "cursor",
            pre_command_context: false,
            stop_retry: true,
        };
        let filter = fx.filter_for(fx.hooks(), cursor);
        expect_block(filter.execute(&cursor_stop_input("c1", 0)));

        fx.fix();
        assert_eq!(
            expect_allow(filter.execute(&cursor_stop_input("c1", 2))),
            None
        );
        assert_eq!(fx.checks(), 1, "loop_count >= 2 では何も実行しない");
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_agent_without_stop_retry_does_not_schedule_retry() {
        // Windsurf / Grok / Antigravity（stop_retry = false）は予定を作らない。gate は同じく効く
        let fx = GateFixture::new();
        let windsurf = AgentProfile {
            id: "windsurf",
            pre_command_context: false,
            stop_retry: false,
        };
        let filter = fx.filter_for(fx.hooks(), windsurf);
        let message = expect_block(filter.execute(&stop_input_for(Some("w1"), false)));
        assert!(
            message.contains("Not run because a stage 1 hook failed: stage 5 [sh]."),
            "{message}"
        );
        assert!(!message.contains("One retry is scheduled"), "{message}");
        let key = StateStore::key(&[RETRY_KEY_VERSION, "windsurf", "w1"]);
        assert_eq!(
            fx.state().take(RETRY_STATE_KIND, &key),
            None,
            "予定を記録しない"
        );
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_retry_not_scheduled_without_session_id() {
        // セッション ID が無い（空白だけを含む）停止は、予定を作らず継続中の停止も実行しない
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        for session_id in [None, Some("   ")] {
            let message = expect_block(filter.execute(&stop_input_for(session_id, false)));
            assert!(message.contains("Not run because"), "{message}");
            assert!(!message.contains("One retry is scheduled"), "{message}");
        }
        fx.fix();
        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("   "), true))),
            None
        );
        assert_eq!(fx.checks(), 2);
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_delegated_session_does_not_schedule_retry() {
        // 委譲セッション（teammate 等）の停止でも gate は効くが、再試行は予定しない
        let fx = GateFixture::new();
        let hooks = fx
            .hooks()
            .into_iter()
            .map(|mut hook| {
                hook.session_scope = crate::config::StopSessionScope::All;
                hook
            })
            .collect();
        let filter = fx.filter_for(hooks, retry_agent());
        let mut input = stop_input_for(Some("s1"), false);
        if let ToolInput::Stop(ref mut stop) = input.tool_input {
            stop.session_kind = StopSessionKind::Delegated;
        }
        let message = expect_block(filter.execute(&input));
        assert!(message.contains("Not run because"), "{message}");
        assert!(!message.contains("One retry is scheduled"), "{message}");
        fx.assert_effects_settled(0);
    }

    #[test]
    fn test_first_stop_clears_unused_retry_record() {
        // Block の後に継続中の停止が来ないまま次のターンになった（ユーザーの中断等）
        // → 次の初回の停止が古い予定を消す
        let fx = GateFixture::new();
        let filter = fx.filter_with_state();
        expect_block(filter.execute(&stop_input_for(Some("s1"), false)));
        fx.fix();

        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("s1"), false))),
            None
        );
        assert_eq!(fx.checks(), 2);
        assert!(fx.wait_for_effects(1));

        // 古い予定は消えているので、継続中の停止では何も実行しない
        assert_eq!(
            expect_allow(filter.execute(&stop_input_for(Some("s1"), true))),
            None
        );
        assert_eq!(fx.checks(), 2);
        fx.assert_effects_settled(1);
    }

    #[test]
    fn test_classify_stop_phases() {
        let phase = |input: &HookInput, stop_active: bool| match StopHookFilter::classify_stop(
            input,
            stop_active,
        ) {
            StopPhase::Skip => "skip",
            StopPhase::First { .. } => "first",
            StopPhase::Continuing { .. } => "continuing",
        };
        assert_eq!(phase(&stop_input_for(Some("s1"), false), false), "first");
        assert_eq!(
            phase(&stop_input_for(Some("s1"), true), false),
            "continuing"
        );
        assert_eq!(phase(&stop_input_for(Some("s1"), true), true), "skip");
        assert_eq!(phase(&cursor_stop_input("c1", 0), false), "first");
        assert_eq!(phase(&cursor_stop_input("c1", 1), false), "continuing");
        assert_eq!(phase(&cursor_stop_input("c1", 2), false), "skip");
        assert_eq!(phase(&cursor_stop_input("c1", 5), false), "skip");
    }

    #[test]
    fn test_retry_key_requires_support_session_id_and_primary_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let filter = StopHookFilter::new(Vec::new(), false, 10)
            .with_state(retry_agent(), StateStore::at(dir.path()));
        let input = stop_input_for(Some("s1"), false);
        assert_eq!(
            filter.retry_key(&input, StopSessionKind::Primary),
            Some(StateStore::key(&[RETRY_KEY_VERSION, "claude-code", "s1"]))
        );
        assert_eq!(filter.retry_key(&input, StopSessionKind::Delegated), None);
        assert_eq!(
            filter.retry_key(
                &stop_input_for(Some(" \t"), false),
                StopSessionKind::Primary
            ),
            None
        );
        assert_eq!(
            filter.retry_key(&stop_input_for(None, false), StopSessionKind::Primary),
            None
        );
        let unsupported = StopHookFilter::new(Vec::new(), false, 10);
        assert_eq!(
            unsupported.retry_key(&input, StopSessionKind::Primary),
            None
        );

        // エージェントが違えば、同じセッション ID でも別の記録になる
        let cursor = StopHookFilter::new(Vec::new(), false, 10).with_state(
            AgentProfile {
                id: "cursor",
                pre_command_context: false,
                stop_retry: true,
            },
            StateStore::at(dir.path()),
        );
        assert_ne!(
            cursor.retry_key(&input, StopSessionKind::Primary),
            filter.retry_key(&input, StopSessionKind::Primary)
        );
    }

    fn failure(stage: u8, label: &str, gate: bool) -> HookFailure {
        HookFailure {
            stage,
            label: label.to_string(),
            gate,
            reason: format!("Stop hook failed: {label}\noutput of {label}"),
        }
    }

    #[test]
    fn test_format_labels_caps_each_stage() {
        let labels: Vec<String> = (1..=11).map(|i| format!("t{i}")).collect();
        assert_eq!(
            format_labels(&labels[..8]),
            "t1, t2, t3, t4, t5, t6, t7, t8"
        );
        assert_eq!(
            format_labels(&labels),
            "t1, t2, t3, t4, t5, t6, t7, t8, and 3 more"
        );
        assert_eq!(
            format_stage_groups(&[
                (1, vec!["a".to_string(), "b".to_string()]),
                (3, vec!["c".to_string()])
            ]),
            "stage 1 [a, b]; stage 3 [c]"
        );
    }

    #[test]
    fn test_group_by_stage_sorts_stages_and_dedups_labels_in_order() {
        let groups = group_by_stage([
            (3, "cargo"),
            (1, "astro-sight"),
            (1, "noslop"),
            (1, "astro-sight"),
            (3, "cargo"),
        ]);
        assert_eq!(
            groups,
            vec![
                (1, vec!["astro-sight".to_string(), "noslop".to_string()]),
                (3, vec!["cargo".to_string()]),
            ]
        );
    }

    #[test]
    fn test_first_stop_block_message_layout() {
        // 先頭に要約（失敗・実行しなかった stage・再試行の予定）、空行、失敗の詳細の順
        let run = StopRun {
            failures: vec![
                failure(1, "astro-sight", true),
                failure(1, "noslop", false),
                failure(1, "astro-sight", true),
            ],
            stopped: Some(GateStop {
                stage: 1,
                not_run: vec![
                    (3, vec!["cargo".to_string()]),
                    (5, vec!["git-sc".to_string()]),
                ],
            }),
        };
        assert_eq!(
            first_stop_block_message(&run, true),
            "Stop hooks failed: stage 1 [astro-sight, noslop].\n\
             Not run because a stage 1 hook failed: stage 3 [cargo]; stage 5 [git-sc].\n\
             One retry is scheduled: at the next stop, the reported hooks up to stage 1 run \
             again, and the stages that were not run start if they pass.\n\
             \n\
             Stop hook failed: astro-sight\noutput of astro-sight\n\n\
             Stop hook failed: noslop\noutput of noslop\n\n\
             Stop hook failed: astro-sight\noutput of astro-sight"
        );
        // 再試行を予定できなければ 3 行目は出さない
        assert!(
            first_stop_block_message(&run, false)
                .starts_with("Stop hooks failed: stage 1 [astro-sight, noslop].\nNot run because a stage 1 hook failed: stage 3 [cargo]; stage 5 [git-sc].\n\nStop hook failed: astro-sight\n")
        );
    }

    #[test]
    fn test_first_stop_block_message_without_not_run_stages() {
        // 実行しなかった stage が無ければ（最後の stage の失敗等）2 行目・3 行目は出さない
        let run = StopRun {
            failures: vec![failure(3, "noslop", false), failure(5, "cargo", true)],
            stopped: Some(GateStop {
                stage: 5,
                not_run: Vec::new(),
            }),
        };
        assert_eq!(
            first_stop_block_message(&run, false),
            "Stop hooks failed: stage 3 [noslop]; stage 5 [cargo].\n\n\
             Stop hook failed: noslop\noutput of noslop\n\n\
             Stop hook failed: cargo\noutput of cargo"
        );
    }

    #[test]
    fn test_retry_failed_notice_and_speech() {
        let stopped = GateStop {
            stage: 1,
            not_run: vec![
                (3, vec!["cargo".to_string()]),
                (
                    5,
                    vec![
                        "git-sc".to_string(),
                        "cargo".to_string(),
                        "notify".to_string(),
                        "extra".to_string(),
                    ],
                ),
            ],
        };
        let run = StopRun {
            failures: vec![failure(1, "astro-sight", true), failure(1, "noslop", false)],
            stopped: Some(stopped.clone()),
        };
        assert_eq!(
            retry_failed_notice(&run, &stopped),
            "claw-hooks: stop hook retry failed at stage 1 [astro-sight]. \
             Not run: stage 3 [cargo]; stage 5 [git-sc, cargo, notify, extra]. \
             No further retry is scheduled."
        );
        // 吹き出しは実行しなかったラベルを重複なく最大 3 個
        assert_eq!(
            retry_failed_speech(&stopped),
            "Stop hook retry failed: cargo, git-sc, notify not run"
        );

        // 実行しなかった stage が無ければ `Not run` の文を省く
        let last = GateStop {
            stage: 5,
            not_run: Vec::new(),
        };
        let run = StopRun {
            failures: vec![failure(5, "git-sc", true)],
            stopped: Some(last.clone()),
        };
        assert_eq!(
            retry_failed_notice(&run, &last),
            "claw-hooks: stop hook retry failed at stage 5 [git-sc]. No further retry is scheduled."
        );
        assert_eq!(
            retry_failed_speech(&last),
            "Stop hook retry failed at stage 5"
        );
    }

    #[test]
    fn test_command_label_replaces_control_characters() {
        // 設定値に紛れた改行・ESC が要約の行を偽装しないよう `?` に置き換える
        assert_eq!(
            StopHookFilter::command_label("\"/opt/bin/to\nol\" --flag"),
            "to?ol"
        );
    }
}
