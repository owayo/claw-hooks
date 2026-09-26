//! command hooks（シェルコマンド中の特定プログラムの呼び出しを外部の判定器へ渡すフック）の実装。
//!
//! 設定した名前のプログラム呼び出し（例: `gws`）をパーサの IR（[`crate::domain::invocation`]）
//! から取り出し、呼び出しごとに判定器（例: `noslop hook command`）を起動して stdin に JSON を
//! 1 行渡す。判定器は終了コードで答える: 0 = 何もしない（stdout があればエージェントへの補足）、
//! 2 = 拒否、それ以外（起動失敗・タイムアウトを含む）= 失敗で、設定の `on_error` に従う。
//!
//! 元のコマンド文字列全体は判定器に渡さない。一致した呼び出し以外の秘密や本文を含み得るため。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Instant;

use serde::Serialize;
use tracing::{debug, info, warn};

use super::Filter;
use crate::config::{CommandHook, CommandHookErrorPolicy};
use crate::domain::command::{program_label, run_with_timeout_tracked, spawn_piped_with_input};
use crate::domain::invocation::{Analysis, CommandLineAnalysis, Invocation, StdinSource};
use crate::domain::normalize::strip_ansi_codes;
use crate::domain::parser::ShellParser;
use crate::domain::{AgentProfile, Decision, HookEvent, HookInput, ToolInput};

/// 判定器の環境に追加し、claw-hooks 自身の環境にあれば command hooks を丸ごと飛ばす環境変数。
///
/// 判定器が AI エージェントなど claw-hooks を経由するコマンドを起動すると、その中の
/// シェルコマンドでまた判定器が起動し、再帰が止まらなくなる。Stop フックの
/// `CLAW_HOOKS_STOP_ACTIVE` と同じく、子孫へ継承される環境変数で断ち切る。
const COMMAND_HOOK_ACTIVE_ENV: &str = "CLAW_HOOKS_COMMAND_HOOK_ACTIVE";

/// 1 回のフックイベントで判定器を起動する回数の上限。
///
/// 判定器はコマンドの実行前に同期で走り、その間エージェントは待たされる。同じプログラムを
/// 大量に呼ぶコマンド（展開されたループ・生成されたスクリプト）で待ち時間とプロセスの起動が
/// 際限なく増えないよう頭打ちにする。超えた分は起動せず、その hook の `on_error` に従う。
/// 1 回のイベントの最悪の所要時間は「この回数 × 各 hook の `timeout`」に収まる。
const MAX_JUDGE_RUNS: usize = 32;

/// 上限（`MAX_JUDGE_RUNS`）を超えて判定器を起動しなかったときの説明。
const SKIPPED_TOO_MANY: &str = "command hook skipped: too many invocations to check";

/// 判定器への入力 JSON の版。
const REQUEST_VERSION: u32 = 1;

/// 判定器が拒否の理由を何も出力しなかったときの定型文。
const DEFAULT_DENY_REASON: &str = "blocked by command hook";

/// command hooks のフィルター。
///
/// 判定器の時間はグローバル設定にしか書けない各 hook の `timeout` だけで決め、
/// `hook_timeout` は受け取らない。`hook_timeout` は未信頼のプロジェクト設定
/// （`.claw-hooks.toml`）から上書きできるため、判定器の時間に使うと `hook_timeout = 1` を
/// 置くだけで判定器を時間切れにでき、`on_error = "allow"` の判定器を素通りさせられる。
pub struct CommandHookFilter {
    /// 設定順の command hook（`run` は argv に分割済み）。
    hooks: Vec<PreparedHook>,
    /// 呼び出し元エージェントの性質（判定器へ渡す `agent` と `context_delivery`）。
    agent: AgentProfile,
    /// claw-hooks 自身が判定器の子孫として動いているか（`COMMAND_HOOK_ACTIVE_ENV` の有無）。
    inside_judge: bool,
}

/// 起動の準備を済ませた command hook。
struct PreparedHook {
    /// 照合に使う正規化済みのプログラム名（`CommandHook::command_key`）。
    key: String,
    /// 判定器のプログラム（`run` の先頭の語）。
    program: String,
    /// 判定器の引数（`run` の 2 語目以降）。
    args: Vec<String>,
    /// ログとエージェント向けの表示ラベル（プログラム名の basename。ディレクトリは含めない）。
    label: String,
    /// 判定器 1 回あたりのタイムアウト秒数。
    timeout_secs: u64,
    /// 判定器が失敗したときの扱い。
    on_error: CommandHookErrorPolicy,
}

impl PreparedHook {
    /// 設定の 1 項目から起動の準備をする。起動しようがない項目は警告して `None` を返す。
    ///
    /// 設定検証（`validate_values`）が同じ条件を弾くので通常は起きない。ここで無視するのは
    /// 検証をすり抜けた場合に、空の argv を起動しようとして毎回失敗させないため。
    /// ログには項目の番号だけを残し、`command` / `run` の中身は残さない。
    fn new(index: usize, hook: &CommandHook) -> Option<Self> {
        let key = hook.command_key();
        if key.is_empty() {
            warn!(
                "⚠️ command_hooks[{}]: `command` is empty; the hook is ignored",
                index
            );
            return None;
        }
        let mut argv = crate::domain::parse_shell_tokens(&hook.run);
        if argv.is_empty() {
            warn!(
                "⚠️ command_hooks[{}]: `run` has no program; the hook is ignored",
                index
            );
            return None;
        }
        let program = argv.remove(0);
        let label = program_label(&program).to_string();
        Some(Self {
            key,
            program,
            args: argv,
            label,
            timeout_secs: hook.timeout_secs(),
            on_error: hook.on_error,
        })
    }
}

/// 判定器 1 回分の結果。
enum Verdict {
    /// 何もしない（exit 0、stdout が空白のみ）。
    Pass,
    /// エージェントへの補足（exit 0 の stdout）。
    Advice(String),
    /// 拒否（exit 2）。中身は理由。
    Deny(String),
    /// 判定器の失敗。中身は `command hook failed: ` に続く説明（数値と定型文だけ）。
    Failed(String),
    /// 1 回のイベントでの起動回数の上限（`MAX_JUDGE_RUNS`）に達したため起動しなかった。
    Skipped,
}

/// 1 回のイベントに閉じた実行管理と結果。上限は hook ごとではなくイベント全体に適用する。
#[derive(Default)]
struct JudgeState {
    seen: HashSet<(usize, String)>,
    matched: usize,
    runs: usize,
    skipped: usize,
    advice: Vec<String>,
}

impl JudgeState {
    /// 同じ (hook, 入力) は 1 回だけ実行する。重複は実行枠を消費しない。
    fn run_once(
        &mut self,
        index: usize,
        hook: &PreparedHook,
        event: &EventContext<'_>,
        invocation: &Invocation,
    ) -> Option<Verdict> {
        self.matched += 1;
        let request = match event.request_json(invocation) {
            Ok(request) => request,
            // 文字列・真偽値・数値だけの構造体なので実際には失敗しない
            Err(_) => return Some(Verdict::Failed("its input could not be built".to_string())),
        };
        if !self.seen.insert((index, request.clone())) {
            return None;
        }
        if self.runs >= MAX_JUDGE_RUNS {
            return Some(Verdict::Skipped);
        }
        self.runs += 1;
        Some(CommandHookFilter::run_judge(hook, &event.dir, request))
    }

    /// 結果を蓄積する。拒否だけを即座に返し、それ以降の判定器を起動させない。
    fn record(&mut self, hook: &PreparedHook, verdict: Verdict) -> Option<Decision> {
        match verdict {
            Verdict::Pass => {}
            Verdict::Advice(text) => {
                debug!(
                    "💬 Command hook [{}] returned advice ({} bytes)",
                    hook.label,
                    text.len()
                );
                self.advice.push(format!("[{}] {}", hook.label, text));
            }
            Verdict::Deny(reason) => {
                info!(
                    "🚫 Command hook [{}] denied the command (reason {} bytes)",
                    hook.label,
                    reason.len()
                );
                return Some(Decision::Block {
                    message: format!("[{}] {}", hook.label, reason),
                });
            }
            Verdict::Failed(desc) => {
                let message = format!("command hook failed: {desc}");
                match hook.on_error {
                    CommandHookErrorPolicy::Block => {
                        return Some(CommandHookFilter::block_on_error(hook, &message));
                    }
                    CommandHookErrorPolicy::Allow => warn!(
                        "⚠️ Command hook [{}] {} (on_error=allow, the command is allowed)",
                        hook.label, message
                    ),
                }
            }
            Verdict::Skipped => match hook.on_error {
                CommandHookErrorPolicy::Block => {
                    return Some(CommandHookFilter::block_on_error(hook, SKIPPED_TOO_MANY));
                }
                // 起動しない分は件数が多くなり得るので、警告は最後にまとめて 1 回出す
                CommandHookErrorPolicy::Allow => self.skipped += 1,
            },
        }
        None
    }

    /// 拒否が無かったイベントを完了し、集約した補足だけを返す。
    fn finish(self) -> Decision {
        if self.skipped > 0 {
            warn!(
                "⚠️ {} command hook check(s) not run: {} (on_error=allow, the command is allowed)",
                self.skipped, SKIPPED_TOO_MANY
            );
        }
        debug!(
            "🪝 Command hooks: matched={} runs={} advice={}",
            self.matched,
            self.runs,
            self.advice.len()
        );
        if self.advice.is_empty() {
            Decision::allow()
        } else {
            Decision::allow_with_context(self.advice.join("\n"))
        }
    }
}

/// 判定器の作業ディレクトリ。
enum JudgeDir {
    /// エージェントが cwd を報告しなかった。claw-hooks の cwd を継承する。
    Inherit,
    /// エージェントが報告したディレクトリ。
    Reported(PathBuf),
    /// 報告された値がディレクトリでない。判定器は起動せず、起動失敗として扱う。
    Unavailable,
}

/// 判定器の stdin に渡す JSON。
///
/// serde は構造体のフィールドを宣言順に出力するので、並びを仕様（docs/cli-reference.md）と
/// そろえておく。`None` は省略せず `null` として出す（判定器がキーの有無で分岐せずに済むように）。
#[derive(Serialize)]
struct JudgeRequest<'a> {
    version: u32,
    agent: &'a str,
    event: &'a str,
    tool_name: &'a str,
    session_id: Option<&'a str>,
    cwd: Option<&'a str>,
    analysis: &'a str,
    context_delivery: bool,
    argv: Vec<JudgeWord<'a>>,
    stdin: Option<JudgeStdin<'a>>,
}

/// 判定器に渡す `argv` の 1 語。
#[derive(Serialize)]
struct JudgeWord<'a> {
    /// 静的に確定した実行時の値。確定しなければ `null`。
    value: Option<&'a str>,
    /// `value` が確定しているか。
    #[serde(rename = "static")]
    is_static: bool,
    /// 実行時に何個の引数になるか（`"one"` / `"zero_or_more"`）。
    cardinality: &'a str,
}

/// 判定器に渡す呼び出しの標準入力。リダイレクトもパイプも無ければ `stdin` 自体を `null` にする。
#[derive(Serialize)]
struct JudgeStdin<'a> {
    /// ヒアドキュメント / here-string の本文が静的に確定していればその値、それ以外は `null`。
    value: Option<&'a str>,
    /// `value` が確定しているか。
    #[serde(rename = "static")]
    is_static: bool,
}

/// 1 回のフックイベントで、どの呼び出しにも共通する判定器への入力と起動条件。
struct EventContext<'a> {
    agent: &'static str,
    event: &'static str,
    tool_name: &'a str,
    session_id: Option<&'a str>,
    /// JSON の `cwd`（エージェントの報告値 → claw-hooks の cwd → `null` の順）。
    cwd: Option<String>,
    context_delivery: bool,
    /// ツールの性質から決まる確度の下限。PowerShell ツールのコマンドはシェル文法で
    /// 読んだ候補でしかないため `Uncertain` にする。
    analysis_floor: Analysis,
    /// 判定器を起動するディレクトリ。
    dir: JudgeDir,
}

impl<'a> EventContext<'a> {
    fn new(input: &'a HookInput, agent: AgentProfile) -> Self {
        let reported = match &input.tool_input {
            ToolInput::Bash(bash) => bash.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()),
            _ => None,
        };
        let dir = match reported {
            None => JudgeDir::Inherit,
            Some(cwd) if Path::new(cwd).is_dir() => JudgeDir::Reported(PathBuf::from(cwd)),
            Some(_) => JudgeDir::Unavailable,
        };
        let cwd = reported.map(str::to_string).or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned())
        });
        let analysis_floor = if input.tool_name == "PowerShell" {
            Analysis::Uncertain
        } else {
            Analysis::Complete
        };

        Self {
            agent: agent.id,
            event: event_name(input.event),
            tool_name: &input.tool_name,
            session_id: input.session_id.as_deref(),
            cwd,
            context_delivery: agent.pre_command_context && input.event == HookEvent::BeforeCommand,
            analysis_floor,
            dir,
        }
    }

    /// 呼び出し 1 つ分の判定器への入力 JSON（改行を含まない 1 行）を組み立てる。
    fn request_json(&self, invocation: &Invocation) -> Result<String, serde_json::Error> {
        let analysis = invocation.analysis.max(self.analysis_floor);
        let request = JudgeRequest {
            version: REQUEST_VERSION,
            agent: self.agent,
            event: self.event,
            tool_name: self.tool_name,
            session_id: self.session_id,
            cwd: self.cwd.as_deref(),
            analysis: analysis.as_str(),
            context_delivery: self.context_delivery,
            argv: invocation
                .words
                .iter()
                .map(|word| JudgeWord {
                    value: word.value.as_deref(),
                    is_static: word.is_static(),
                    cardinality: word.cardinality.as_str(),
                })
                .collect(),
            stdin: match &invocation.stdin {
                StdinSource::Inherited => None,
                StdinSource::Literal { value } => Some(JudgeStdin {
                    value: value.as_deref(),
                    is_static: value.is_some(),
                }),
                StdinSource::Other => Some(JudgeStdin {
                    value: None,
                    is_static: false,
                }),
            },
        };
        serde_json::to_string(&request)
    }
}

/// 判定器へ渡すイベント名（claw-hooks で正規化した名前）。
fn event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::PermissionRequest => "PermissionRequest",
        // applies_to を通るのは BeforeCommand と PermissionRequest だけ
        _ => "PreToolUse",
    }
}

/// 判定器の出力を、エージェントへ返せる形（ANSI 除去 + 前後の空白除去）にする。
fn clean_output(bytes: &[u8]) -> String {
    strip_ansi_codes(&String::from_utf8_lossy(bytes))
        .trim()
        .to_string()
}

/// 判定器の終了状態と出力を判定に分類する。
fn classify(output: &Output) -> Verdict {
    match output.status.code() {
        Some(0) => {
            let advice = clean_output(&output.stdout);
            if advice.is_empty() {
                Verdict::Pass
            } else {
                Verdict::Advice(advice)
            }
        }
        Some(2) => {
            // 理由は stderr を優先し、空なら stdout、それも空なら定型文にする
            let reason = [&output.stderr, &output.stdout]
                .into_iter()
                .map(|bytes| clean_output(bytes))
                .find(|text| !text.is_empty())
                .unwrap_or_else(|| DEFAULT_DENY_REASON.to_string());
            Verdict::Deny(reason)
        }
        Some(code) => Verdict::Failed(format!("exit code {code}")),
        None => Verdict::Failed("terminated by a signal".to_string()),
    }
}

impl CommandHookFilter {
    /// 設定から CommandHookFilter を作成する。`run` の argv 化はここで 1 回だけ行う。
    pub fn new(hooks: &[CommandHook], agent: AgentProfile) -> Self {
        let inside_judge = std::env::var_os(COMMAND_HOOK_ACTIVE_ENV).is_some();
        Self::build(hooks, agent, inside_judge)
    }

    /// 再帰防止の環境変数の有無を引数で受ける版。
    ///
    /// テストから環境変数を書き換えずに再帰防止を検証するために分けている
    /// （プロセスの環境変数の書き換えは、並列に走る他のテストへ波及する）。
    fn build(hooks: &[CommandHook], agent: AgentProfile, inside_judge: bool) -> Self {
        Self {
            hooks: hooks
                .iter()
                .enumerate()
                .filter_map(|(index, hook)| PreparedHook::new(index, hook))
                .collect(),
            agent,
            inside_judge,
        }
    }

    /// 解析済みのコマンドに対して判定器を順に実行し、判定を返す。
    ///
    /// パース（`execute`）と分けているのは、判定の分岐を手で組み立てた IR で検証するため。
    /// 呼び出しは出現順、同じ呼び出しに一致する hook は設定順に実行し、最初の拒否で打ち切る。
    /// 補足は拒否が無かったときだけ返す。
    fn judge(&self, input: &HookInput, analysis: &CommandLineAnalysis) -> Decision {
        if analysis.pathological {
            return self.judge_unanalyzable();
        }

        let event = EventContext::new(input, self.agent);
        let mut state = JudgeState::default();

        for invocation in &analysis.invocations {
            // 実在しない可能性がある過大近似は渡さない（存在しない呼び出しへの拒否・補足になる）
            if invocation.analysis == Analysis::Speculative {
                continue;
            }
            // プログラム名が静的に決まらない呼び出しは、どの hook の対象か判断できない
            let Some(key) = invocation.command_key() else {
                continue;
            };

            for (index, hook) in self.hooks.iter().enumerate() {
                if hook.key != key {
                    continue;
                }
                if let Some(verdict) = state.run_once(index, hook, &event, invocation)
                    && let Some(decision) = state.record(hook, verdict)
                {
                    return decision;
                }
            }
        }

        state.finish()
    }

    /// 解析を諦めたコマンド（長すぎる・深すぎる）の判定。
    ///
    /// 呼び出しを取りこぼしている可能性があるため、必須の検査として使われている
    /// （`on_error = "block"` の）hook があれば拒否する。助言用の hook しか無ければ通す。
    fn judge_unanalyzable(&self) -> Decision {
        match self
            .hooks
            .iter()
            .find(|hook| hook.on_error == CommandHookErrorPolicy::Block)
        {
            Some(hook) => {
                warn!(
                    "❌ Command hook [{}] could not analyze the command (on_error=block)",
                    hook.label
                );
                Decision::Block {
                    message: format!(
                        "[{}] command hook could not analyze the command",
                        hook.label
                    ),
                }
            }
            None => {
                debug!("🪝 Command hooks skipped: the command could not be analyzed");
                Decision::allow()
            }
        }
    }

    /// `on_error = "block"` の hook の失敗を拒否にする（`message` は `[{label}] ` に続く部分）。
    fn block_on_error(hook: &PreparedHook, message: &str) -> Decision {
        warn!(
            "❌ Command hook [{}] {} (on_error=block)",
            hook.label, message
        );
        Decision::Block {
            message: format!("[{}] {}", hook.label, message),
        }
    }

    /// 判定器を 1 回実行して結果を分類する。
    ///
    /// 時間制限はその hook の `timeout` だけ（グローバル設定にしか書けない値）。
    fn run_judge(hook: &PreparedHook, dir: &JudgeDir, request: String) -> Verdict {
        let cwd = match dir {
            JudgeDir::Inherit => None,
            JudgeDir::Reported(path) => Some(path.as_path()),
            JudgeDir::Unavailable => {
                // パスはログに残さない（ユーザーのディレクトリ構成を含むため）
                warn!(
                    "❌ Command hook [{}] not started: the reported working directory is not a directory",
                    hook.label
                );
                return Verdict::Failed(
                    "could not be started (working directory not found)".to_string(),
                );
            }
        };

        let mut stdin = request.into_bytes();
        stdin.push(b'\n');
        debug!(
            "🪝 Running command hook [{}]: arg_count={} input_bytes={} timeout={}s",
            hook.label,
            hook.args.len(),
            stdin.len(),
            hook.timeout_secs
        );

        let start = Instant::now();
        let child = match spawn_piped_with_input(
            &hook.program,
            &hook.args,
            &[(COMMAND_HOOK_ACTIVE_ENV, "1")],
            cwd,
            stdin,
        ) {
            Ok(child) => child,
            Err(e) => {
                warn!(
                    "❌ Command hook [{}] could not be started: {}",
                    hook.label, e
                );
                return Verdict::Failed("could not be started".to_string());
            }
        };
        // タイムアウト時の通知本文にも使われるので、プログラム名だけを渡す
        let result = match run_with_timeout_tracked(child, hook.timeout_secs, &hook.label) {
            Ok(result) => result,
            Err(e) => {
                warn!("❌ Command hook [{}] could not be run: {}", hook.label, e);
                return Verdict::Failed("could not be run".to_string());
            }
        };

        let status = match result.output.status.code() {
            _ if result.timed_out => "timed out".to_string(),
            Some(code) => format!("exit code {code}"),
            None => "signal".to_string(),
        };
        info!(
            "⏰️ Command hook [{}] finished in {:.2}s: {} stdout={} bytes stderr={} bytes",
            hook.label,
            start.elapsed().as_secs_f64(),
            status,
            result.output.stdout.len(),
            result.output.stderr.len()
        );

        if result.timed_out {
            return Verdict::Failed(format!("timed out after {}s", hook.timeout_secs));
        }
        classify(&result.output)
    }
}

impl Filter for CommandHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // コマンド実行前/承認前イベントのシェルツール（Bash / PowerShell）にのみ適用
        if !matches!(
            input.event,
            HookEvent::BeforeCommand | HookEvent::PermissionRequest
        ) || !input.is_shell_tool()
            || input.bash_command().is_none()
            || self.hooks.is_empty()
        {
            return false;
        }
        if self.inside_judge {
            debug!(
                "⛔ {} detected, skipping command hooks (recursion prevention)",
                COMMAND_HOOK_ACTIVE_ENV
            );
            return false;
        }
        true
    }

    fn execute(&self, input: &HookInput) -> Decision {
        let Some(command) = input.bash_command() else {
            return Decision::allow();
        };
        let analysis = ShellParser::new().extract_invocations(command);
        self.judge(input, &analysis)
    }

    fn priority(&self) -> u32 {
        super::priority::COMMAND_HOOK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::BashInput;
    use crate::domain::invocation::{Cardinality, ShellWord};
    // 判定器のプロセスを起動するテストは Unix だけで走らせる（sh のスクリプトを使うため）。
    // それらだけが使う補助は同じ条件でコンパイルし、Windows のテストビルドに未使用を残さない。
    #[cfg(unix)]
    use std::time::Duration;

    const CLAUDE: AgentProfile = AgentProfile {
        id: "claude-code",
        pre_command_context: true,
    };
    const CURSOR: AgentProfile = AgentProfile {
        id: "cursor",
        pre_command_context: false,
    };

    /// 値が静的に確定した語。
    fn word(value: &str) -> ShellWord {
        ShellWord {
            raw: value.to_string(),
            text: value.to_string(),
            value: Some(value.to_string()),
            cardinality: Cardinality::One,
        }
    }

    /// 実行時まで値が決まらない語（非引用の展開など）。
    fn dynamic_word(raw: &str) -> ShellWord {
        ShellWord {
            raw: raw.to_string(),
            text: raw.to_string(),
            value: None,
            cardinality: Cardinality::ZeroOrMore,
        }
    }

    fn invocation(words: &[&str]) -> Invocation {
        Invocation {
            words: words.iter().map(|w| word(w)).collect(),
            stdin: StdinSource::Inherited,
            analysis: Analysis::Complete,
        }
    }

    #[cfg(unix)]
    fn analysis_of(invocations: Vec<Invocation>) -> CommandLineAnalysis {
        CommandLineAnalysis {
            invocations,
            pathological: false,
        }
    }

    fn shell_input(event: HookEvent, tool_name: &str, cwd: Option<&str>) -> HookInput {
        HookInput {
            event,
            tool_name: tool_name.to_string(),
            tool_input: ToolInput::Bash(BashInput {
                command: "gws docs".to_string(),
                timeout: None,
                cwd: cwd.map(str::to_string),
            }),
            session_id: Some("abc".to_string()),
        }
    }

    fn bash_input() -> HookInput {
        shell_input(HookEvent::BeforeCommand, "Bash", None)
    }

    fn hook(command: &str, run: &str) -> CommandHook {
        CommandHook {
            command: command.to_string(),
            run: run.to_string(),
            timeout: None,
            on_error: CommandHookErrorPolicy::Allow,
        }
    }

    fn blocking(mut hook: CommandHook) -> CommandHook {
        hook.on_error = CommandHookErrorPolicy::Block;
        hook
    }

    #[cfg(unix)]
    fn with_timeout(mut hook: CommandHook, secs: u64) -> CommandHook {
        hook.timeout = Some(secs);
        hook
    }

    /// 環境変数に依らない（判定器の子孫ではない）フィルター。
    fn filter(hooks: &[CommandHook]) -> CommandHookFilter {
        CommandHookFilter::build(hooks, CLAUDE, false)
    }

    /// 判定器のスクリプトを書き、`run` に書く文字列（`sh '<path>'`）を返す。
    ///
    /// スクリプトに実行権限を付けて直接起動しないのは、書き込み直後の exec が並列の fork と
    /// 重なると Linux で ETXTBSY（Text file busy）になり、テストがまれに落ちるため。
    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        format!("sh '{}'", path.display())
    }

    /// `echo run >> <path>` を書いた行数（判定器が何回起動したか）。ファイルが無ければ 0。
    #[cfg(unix)]
    fn runs_recorded(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn expect_block(decision: Decision) -> String {
        match decision {
            Decision::Block { message } => message,
            other => panic!("Block を期待したが {other:?}"),
        }
    }

    fn expect_allow(decision: Decision) -> Option<String> {
        match decision {
            Decision::Allow { additional_context } => additional_context,
            other => panic!("Allow を期待したが {other:?}"),
        }
    }

    // === 構築と applies_to ===

    #[test]
    fn test_prepared_hook_splits_run_once_and_labels_by_basename() {
        let filter = filter(&[hook("GWS", "'/opt/judge dir/noslop' hook command")]);
        let prepared = &filter.hooks[0];
        assert_eq!(prepared.key, "gws", "command は command_key で正規化する");
        assert_eq!(prepared.program, "/opt/judge dir/noslop");
        assert_eq!(prepared.args, vec!["hook", "command"]);
        assert_eq!(prepared.label, "noslop", "ラベルにディレクトリを含めない");
        assert_eq!(prepared.timeout_secs, CommandHook::DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn test_hook_without_program_is_ignored() {
        let filter = filter(&[hook("gws", "   "), hook("", "judge")]);
        assert!(filter.hooks.is_empty());
        assert!(
            !filter.applies_to(&bash_input()),
            "起動できる hook が無ければ適用しない"
        );
    }

    #[test]
    fn test_applies_to_shell_tools_before_execution_only() {
        let filter = filter(&[hook("gws", "judge")]);

        assert!(filter.applies_to(&shell_input(HookEvent::BeforeCommand, "Bash", None)));
        assert!(filter.applies_to(&shell_input(HookEvent::BeforeCommand, "PowerShell", None)));
        assert!(filter.applies_to(&shell_input(HookEvent::PermissionRequest, "Bash", None)));
        assert!(!filter.applies_to(&shell_input(HookEvent::AfterFileEdit, "Bash", None)));
        assert!(!filter.applies_to(&shell_input(HookEvent::Stop, "Bash", None)));
        assert!(!filter.applies_to(&shell_input(HookEvent::BeforeCommand, "Write", None)));

        let non_bash_input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Other(serde_json::Value::Null),
            session_id: None,
        };
        assert!(
            !filter.applies_to(&non_bash_input),
            "コマンドが無ければ適用しない"
        );
    }

    #[test]
    fn test_applies_to_is_skipped_inside_a_judge() {
        // claw-hooks 自身が判定器の子孫なら（環境変数あり）、command hooks を丸ごと飛ばす
        let nested = CommandHookFilter::build(&[hook("gws", "judge")], CLAUDE, true);
        assert!(!nested.applies_to(&bash_input()));
    }

    #[test]
    fn test_priority_runs_after_builtin_and_custom_filters() {
        use super::super::priority;
        let filter = filter(&[hook("gws", "judge")]);
        assert_eq!(filter.priority(), priority::COMMAND_HOOK);
        for earlier in [priority::KILL, priority::DD, priority::RM, priority::CUSTOM] {
            assert!(earlier < filter.priority());
        }
    }

    // === 判定器への入力 JSON ===

    #[test]
    fn test_request_json_has_every_field_in_spec_order() {
        let input = shell_input(HookEvent::BeforeCommand, "Bash", Some("/work/project"));
        let context = EventContext::new(&input, CLAUDE);
        let invocation = Invocation {
            words: vec![word("gws"), word("docs"), dynamic_word("$ARGS")],
            stdin: StdinSource::Inherited,
            analysis: Analysis::Complete,
        };

        assert_eq!(
            context.request_json(&invocation).unwrap(),
            concat!(
                r#"{"version":1,"agent":"claude-code","event":"PreToolUse","tool_name":"Bash","#,
                r#""session_id":"abc","cwd":"/work/project","analysis":"complete","#,
                r#""context_delivery":true,"argv":["#,
                r#"{"value":"gws","static":true,"cardinality":"one"},"#,
                r#"{"value":"docs","static":true,"cardinality":"one"},"#,
                r#"{"value":null,"static":false,"cardinality":"zero_or_more"}],"#,
                r#""stdin":null}"#
            )
        );
    }

    #[test]
    fn test_request_json_stdin_variants() {
        let input = bash_input();
        let context = EventContext::new(&input, CLAUDE);
        let stdin_of = |stdin: StdinSource| {
            let invocation = Invocation {
                stdin,
                ..invocation(&["gws"])
            };
            let json: serde_json::Value =
                serde_json::from_str(&context.request_json(&invocation).unwrap()).unwrap();
            json["stdin"].clone()
        };

        assert_eq!(stdin_of(StdinSource::Inherited), serde_json::Value::Null);
        assert_eq!(
            stdin_of(StdinSource::Literal {
                value: Some("body\n".to_string())
            }),
            serde_json::json!({"value": "body\n", "static": true})
        );
        assert_eq!(
            stdin_of(StdinSource::Literal { value: None }),
            serde_json::json!({"value": null, "static": false})
        );
        assert_eq!(
            stdin_of(StdinSource::Other),
            serde_json::json!({"value": null, "static": false})
        );
    }

    fn request_value(
        input: &HookInput,
        agent: AgentProfile,
        analysis: Analysis,
    ) -> serde_json::Value {
        let context = EventContext::new(input, agent);
        let invocation = Invocation {
            analysis,
            ..invocation(&["gws", "docs"])
        };
        serde_json::from_str(&context.request_json(&invocation).unwrap()).unwrap()
    }

    #[test]
    fn test_request_json_permission_request_does_not_deliver_context() {
        let input = shell_input(HookEvent::PermissionRequest, "Bash", None);
        let json = request_value(&input, CLAUDE, Analysis::Complete);
        assert_eq!(json["event"], "PermissionRequest");
        assert_eq!(
            json["context_delivery"], false,
            "PermissionRequest の Allow に付けた補足はエージェントへ届かない"
        );
    }

    #[test]
    fn test_request_json_context_delivery_follows_agent_profile() {
        let json = request_value(&bash_input(), CURSOR, Analysis::Complete);
        assert_eq!(json["agent"], "cursor");
        assert_eq!(json["context_delivery"], false);
    }

    #[test]
    fn test_request_json_powershell_is_at_least_uncertain() {
        let powershell = shell_input(HookEvent::BeforeCommand, "PowerShell", None);
        let json = request_value(&powershell, CLAUDE, Analysis::Complete);
        assert_eq!(
            json["analysis"], "uncertain",
            "PowerShell のコマンドをシェル文法で読んだ結果は候補でしかない"
        );
        assert_eq!(json["tool_name"], "PowerShell");

        let bash = request_value(&bash_input(), CLAUDE, Analysis::Uncertain);
        assert_eq!(bash["analysis"], "uncertain");
    }

    #[test]
    fn test_request_json_falls_back_to_process_cwd_and_null_session() {
        let mut input = bash_input();
        input.session_id = None;
        let json = request_value(&input, CLAUDE, Analysis::Complete);
        assert_eq!(json["session_id"], serde_json::Value::Null);
        let process_cwd = std::env::current_dir().unwrap();
        assert_eq!(json["cwd"], process_cwd.to_string_lossy().as_ref());
    }

    // === 病的入力・対象外の呼び出し（判定器を起動しない） ===

    #[test]
    fn test_pathological_command_blocks_only_with_a_blocking_hook() {
        let pathological = CommandLineAnalysis {
            invocations: vec![invocation(&["gws", "docs"])],
            pathological: true,
        };
        // 起動されれば失敗する判定器にしておく（起動しないことの確認）
        let advisory = filter(&[hook("gws", "claw-hooks-no-such-judge")]);
        assert_eq!(
            expect_allow(advisory.judge(&bash_input(), &pathological)),
            None
        );

        let strict = filter(&[
            hook("gws", "claw-hooks-no-such-judge"),
            blocking(hook("other", "/opt/strict-judge --check")),
        ]);
        assert_eq!(
            expect_block(strict.judge(&bash_input(), &pathological)),
            "[strict-judge] command hook could not analyze the command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_unmatched_speculative_and_dynamic_invocations_do_not_start_judges() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("echo run >> '{}'\n", count.display()),
        );
        let filter = filter(&[blocking(hook("gws", &run))]);
        let speculative = Invocation {
            analysis: Analysis::Speculative,
            ..invocation(&["gws", "docs"])
        };
        let dynamic_program = Invocation {
            words: vec![dynamic_word("$CMD"), word("docs")],
            stdin: StdinSource::Inherited,
            analysis: Analysis::Complete,
        };
        let analysis = analysis_of(vec![
            invocation(&["ls", "-la"]),
            invocation(&["gwsx", "docs"]),
            speculative,
            dynamic_program,
        ]);

        assert_eq!(expect_allow(filter.judge(&bash_input(), &analysis)), None);
        assert_eq!(runs_recorded(&count), 0, "判定器を起動してはいけない");
    }

    // === 判定器の返し方 ===

    #[cfg(unix)]
    #[test]
    fn test_judge_receives_request_json_as_one_line_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let received = dir.path().join("received.json");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("cat > '{}'\n", received.display()),
        );
        let filter = filter(&[hook("gws", &run)]);
        let input = shell_input(
            HookEvent::BeforeCommand,
            "Bash",
            Some(dir.path().to_str().unwrap()),
        );
        let target = invocation(&["gws", "docs", "create"]);

        let decision = filter.judge(&input, &analysis_of(vec![target.clone()]));

        assert_eq!(
            expect_allow(decision),
            None,
            "exit 0 で出力なしは何もしない"
        );
        let expected = EventContext::new(&input, CLAUDE)
            .request_json(&target)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&received).unwrap(),
            format!("{expected}\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_zero_with_whitespace_only_output_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(dir.path(), "judge.sh", "printf '  \\n\\t\\n'\n");
        let filter = filter(&[hook("gws", &run)]);
        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));
        assert_eq!(expect_allow(decision), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_zero_output_becomes_labelled_advice_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let first = script(
            dir.path(),
            "first.sh",
            "printf '\\033[33mcheck the draft\\033[0m\\n'\n",
        );
        let second = script(dir.path(), "second.sh", "echo '  second note  '\n");
        let filter = filter(&[hook("gws", &first), hook("gws", &second)]);

        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));

        assert_eq!(
            expect_allow(decision).as_deref(),
            Some("[sh] check the draft\n[sh] second note"),
            "ANSI を除去して trim し、設定順に改行でつなぐ"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_two_denies_with_stderr_reason() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(
            dir.path(),
            "judge.sh",
            "echo 'from stdout'\necho 'use --dry-run first' >&2\nexit 2\n",
        );
        let filter = filter(&[hook("gws", &run)]);
        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));
        assert_eq!(expect_block(decision), "[sh] use --dry-run first");
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_two_falls_back_to_stdout_then_fixed_reason() {
        let dir = tempfile::tempdir().unwrap();
        let stdout_only = script(dir.path(), "stdout.sh", "echo 'from stdout'\nexit 2\n");
        let silent = script(dir.path(), "silent.sh", "exit 2\n");

        let decision = filter(&[hook("gws", &stdout_only)])
            .judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));
        assert_eq!(expect_block(decision), "[sh] from stdout");

        let decision = filter(&[hook("gws", &silent)])
            .judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));
        assert_eq!(expect_block(decision), "[sh] blocked by command hook");
    }

    #[cfg(unix)]
    #[test]
    fn test_other_exit_codes_follow_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(
            dir.path(),
            "judge.sh",
            "echo 'tip that must not leak'\nexit 1\n",
        );
        let analysis = analysis_of(vec![invocation(&["gws"])]);

        let decision = filter(&[hook("gws", &run)]).judge(&bash_input(), &analysis);
        assert_eq!(
            expect_allow(decision),
            None,
            "on_error=allow は通し、失敗した判定器の出力を補足にしない"
        );

        let decision = filter(&[blocking(hook("gws", &run))]).judge(&bash_input(), &analysis);
        assert_eq!(
            expect_block(decision),
            "[sh] command hook failed: exit code 1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_signal_termination_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(dir.path(), "judge.sh", "kill -KILL $$\n");
        let filter = filter(&[blocking(hook("gws", &run))]);
        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));
        assert_eq!(
            expect_block(decision),
            "[sh] command hook failed: terminated by a signal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_judge_timeout_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(dir.path(), "judge.sh", "sleep 5\n");
        let filter = filter(&[blocking(with_timeout(hook("gws", &run), 1))]);

        let start = Instant::now();
        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));

        assert_eq!(
            expect_block(decision),
            "[sh] command hook failed: timed out after 1s"
        );
        assert!(start.elapsed() < Duration::from_secs(4));
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_program_is_a_start_failure_without_its_directory() {
        let analysis = analysis_of(vec![invocation(&["gws"])]);

        let decision = filter(&[blocking(hook("gws", "claw-hooks-no-such-judge --check"))])
            .judge(&bash_input(), &analysis);
        assert_eq!(
            expect_block(decision),
            "[claw-hooks-no-such-judge] command hook failed: could not be started"
        );

        let decision = filter(&[blocking(hook("gws", "/claw-hooks-private/bin/judge"))])
            .judge(&bash_input(), &analysis);
        let message = expect_block(decision);
        assert_eq!(message, "[judge] command hook failed: could not be started");

        let decision =
            filter(&[hook("gws", "claw-hooks-no-such-judge")]).judge(&bash_input(), &analysis);
        assert_eq!(expect_allow(decision), None, "on_error=allow なら通す");
    }

    #[cfg(unix)]
    #[test]
    fn test_judge_runs_in_the_reported_directory() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(dir.path(), "judge.sh", "pwd -P\n");
        let filter = filter(&[hook("gws", &run)]);
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let input = shell_input(
            HookEvent::BeforeCommand,
            "Bash",
            Some(work.to_str().unwrap()),
        );

        let decision = filter.judge(&input, &analysis_of(vec![invocation(&["gws"])]));

        let expected = std::fs::canonicalize(&work).unwrap();
        assert_eq!(
            expect_allow(decision),
            Some(format!("[sh] {}", expected.display()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_judge_inherits_process_cwd_without_a_reported_directory() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(dir.path(), "judge.sh", "pwd -P\n");
        let filter = filter(&[hook("gws", &run)]);

        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));

        let expected = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(
            expect_allow(decision),
            Some(format!("[sh] {}", expected.display()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_reported_directory_is_a_start_failure() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("echo run >> '{}'\n", count.display()),
        );
        let missing = dir.path().join("missing");
        let input = shell_input(
            HookEvent::BeforeCommand,
            "Bash",
            Some(missing.to_str().unwrap()),
        );
        let analysis = analysis_of(vec![invocation(&["gws"])]);

        let decision = filter(&[hook("gws", &run)]).judge(&input, &analysis);
        assert_eq!(expect_allow(decision), None);

        let decision = filter(&[blocking(hook("gws", &run))]).judge(&input, &analysis);
        let message = expect_block(decision);
        assert_eq!(
            message,
            "[sh] command hook failed: could not be started (working directory not found)"
        );
        assert!(!message.contains(missing.to_str().unwrap()));
        assert_eq!(runs_recorded(&count), 0, "判定器を起動してはいけない");
    }

    // === 実行順序と上限 ===

    #[cfg(unix)]
    #[test]
    fn test_first_deny_stops_remaining_judges_and_drops_advice() {
        let dir = tempfile::tempdir().unwrap();
        let denied = dir.path().join("denied");
        let later = dir.path().join("later");
        let advisor = script(dir.path(), "advisor.sh", "echo 'a tip'\n");
        let denier = script(
            dir.path(),
            "denier.sh",
            &format!(
                "echo run >> '{}'\necho 'not allowed' >&2\nexit 2\n",
                denied.display()
            ),
        );
        let marker = script(
            dir.path(),
            "marker.sh",
            &format!("echo run >> '{}'\n", later.display()),
        );
        let filter = filter(&[
            hook("gws", &advisor),
            hook("gws", &denier),
            hook("gws", &marker),
        ]);
        let analysis = analysis_of(vec![invocation(&["gws", "a"]), invocation(&["gws", "b"])]);

        let decision = filter.judge(&bash_input(), &analysis);

        assert_eq!(
            expect_block(decision),
            "[sh] not allowed",
            "補足は拒否が無かったときだけ返す"
        );
        assert_eq!(runs_recorded(&denied), 1, "最初の拒否で打ち切る");
        assert_eq!(runs_recorded(&later), 0, "後続の判定器を起動してはいけない");
    }

    #[cfg(unix)]
    #[test]
    fn test_identical_invocations_are_judged_once() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("echo run >> '{}'\n", count.display()),
        );
        let filter = filter(&[hook("gws", &run)]);
        let piped = Invocation {
            stdin: StdinSource::Other,
            ..invocation(&["gws", "docs"])
        };
        let analysis = analysis_of(vec![
            invocation(&["gws", "docs"]),
            invocation(&["gws", "docs"]),
            invocation(&["/usr/bin/GWS", "docs"]),
            piped,
        ]);

        filter.judge(&bash_input(), &analysis);

        // 1 回目と 2 回目は同一。パスで呼んだ 3 回目は argv が違い、4 回目は stdin が違う
        assert_eq!(runs_recorded(&count), 3);
    }

    #[cfg(unix)]
    #[test]
    fn test_judge_runs_are_capped_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("echo run >> '{}'\n", count.display()),
        );
        let items: Vec<String> = (0..MAX_JUDGE_RUNS + 8)
            .map(|i| format!("item-{i}"))
            .collect();
        let analysis = analysis_of(
            items
                .iter()
                .map(|item| invocation(&["gws", item.as_str()]))
                .collect(),
        );

        let decision = filter(&[hook("gws", &run)]).judge(&bash_input(), &analysis);
        assert_eq!(
            expect_allow(decision),
            None,
            "on_error=allow は超過分を飛ばして通す"
        );
        assert_eq!(runs_recorded(&count), MAX_JUDGE_RUNS);

        std::fs::remove_file(&count).unwrap();
        let decision = filter(&[blocking(hook("gws", &run))]).judge(&bash_input(), &analysis);
        assert_eq!(
            expect_block(decision),
            "[sh] command hook skipped: too many invocations to check"
        );
        assert_eq!(runs_recorded(&count), MAX_JUDGE_RUNS);
    }

    #[cfg(unix)]
    #[test]
    fn test_deduplication_preserves_each_hook_and_invocation_order() {
        let dir = tempfile::tempdir().unwrap();
        let order = dir.path().join("order");
        let first = script(
            dir.path(),
            "first.sh",
            &format!("echo first >> '{}'\necho first\n", order.display()),
        );
        let second = script(
            dir.path(),
            "second.sh",
            &format!("echo second >> '{}'\necho second\n", order.display()),
        );
        let filter = filter(&[hook("gws", &first), hook("gws", &second)]);
        let analysis = analysis_of(vec![
            invocation(&["gws", "a"]),
            invocation(&["gws", "a"]),
            invocation(&["gws", "b"]),
        ]);

        let decision = filter.judge(&bash_input(), &analysis);

        assert_eq!(
            std::fs::read_to_string(&order).unwrap(),
            "first\nsecond\nfirst\nsecond\n"
        );
        assert_eq!(
            expect_allow(decision).as_deref(),
            Some("[sh] first\n[sh] second\n[sh] first\n[sh] second")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_run_limit_is_shared_across_hooks_and_ignores_duplicates_at_limit() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("echo run >> '{}'\n", count.display()),
        );
        let filter = filter(&[hook("gws", &run), blocking(hook("gws", &run))]);
        let mut invocations: Vec<_> = (0..MAX_JUDGE_RUNS / 2)
            .map(|i| invocation(&["gws", &format!("item-{i}")]))
            .collect();
        // 2 hook × 16 入力で上限。既に検査した入力はここでも拒否の理由にしない。
        invocations.push(invocations[0].clone());
        let mut analysis = analysis_of(invocations);
        assert_eq!(expect_allow(filter.judge(&bash_input(), &analysis)), None);
        assert_eq!(runs_recorded(&count), MAX_JUDGE_RUNS);

        // 新しいイベントでは枠がリセットされ、未検査の超過入力だけが on_error に従う。
        std::fs::remove_file(&count).unwrap();
        analysis.invocations.push(invocation(&["gws", "extra"]));
        assert_eq!(
            expect_block(filter.judge(&bash_input(), &analysis)),
            "[sh] command hook skipped: too many invocations to check"
        );
        assert_eq!(runs_recorded(&count), MAX_JUDGE_RUNS);
    }

    #[cfg(unix)]
    #[test]
    fn test_judge_environment_marks_recursion() {
        let dir = tempfile::tempdir().unwrap();
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("printf %s \"${COMMAND_HOOK_ACTIVE_ENV}\"\n"),
        );
        let filter = filter(&[hook("gws", &run)]);
        let decision = filter.judge(&bash_input(), &analysis_of(vec![invocation(&["gws"])]));
        assert_eq!(expect_allow(decision).as_deref(), Some("[sh] 1"));
    }

    #[cfg(unix)]
    #[test]
    fn test_execute_does_not_start_judges_for_unmatched_commands() {
        // execute はパーサの結果を judge に渡す。一致する呼び出しが無ければ判定器は起動しない
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("echo run >> '{}'\n", count.display()),
        );
        let filter = filter(&[blocking(hook("gws", &run))]);
        let input = bash_command_input("ls -la | grep gws");

        assert_eq!(expect_allow(filter.execute(&input)), None);
        assert_eq!(runs_recorded(&count), 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_execute_passes_only_the_matched_invocation_to_the_judge() {
        // パーサと組み合わせた経路。元のコマンド文字列ではなく、一致した呼び出しの argv だけを渡す
        let dir = tempfile::tempdir().unwrap();
        let received = dir.path().join("received");
        let run = script(
            dir.path(),
            "judge.sh",
            &format!("cat >> '{}'\n", received.display()),
        );
        let filter = filter(&[hook("gws", &run)]);
        let input = bash_command_input("echo secret-token && gws docs 'a b'");

        assert_eq!(expect_allow(filter.execute(&input)), None);

        let received = std::fs::read_to_string(&received).unwrap();
        assert_eq!(
            received.lines().count(),
            1,
            "判定器は一致した呼び出しの数だけ起動する"
        );
        assert!(
            !received.contains("secret-token"),
            "一致しない呼び出しを渡さない"
        );
        let json: serde_json::Value = serde_json::from_str(received.trim()).unwrap();
        assert_eq!(
            json["argv"],
            serde_json::json!([
                {"value": "gws", "static": true, "cardinality": "one"},
                {"value": "docs", "static": true, "cardinality": "one"},
                {"value": "a b", "static": true, "cardinality": "one"},
            ])
        );
        // analysis の値はパーサの経路（AST / フォールバック）で変わるのでここでは見ない
        assert_eq!(json["stdin"], serde_json::Value::Null);
    }

    #[cfg(unix)]
    fn bash_command_input(command: &str) -> HookInput {
        HookInput {
            tool_input: ToolInput::Bash(BashInput {
                command: command.to_string(),
                timeout: None,
                cwd: None,
            }),
            ..bash_input()
        }
    }
}
