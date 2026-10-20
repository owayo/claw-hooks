//! Claude Code のチームメイト（split-pane で起動された別プロセスの claude）の判定。
//!
//! split-pane（tmux / iTerm2）のチームメイトは、ターンを終えると自分のプロセスで通常の
//! `Stop` を出す。公式仕様の `agent_id` は「サブエージェント呼び出しの内側で発火したときだけ
//! 入る」項目なので、チームメイトのメインスレッドが出す Stop には入らない。`agent_type` も
//! `--agent` で起動したメインセッションと同じ形でしか入らず、入力だけではメインセッションと
//! 見分けられない。
//!
//! そこで、Claude Code がチームメイトを起動するときに渡す非公開の CLI オプション
//! （`--agent-id <名前>@<チーム> --agent-name <名前> --team-name <チーム>`）を、フックの祖先
//! プロセスの argv から探す。Claude Code 自身が 3 つがそろったときだけチームの文脈を作るので、
//! 3 つが同じ 1 つの argv にそろうことを求める。見つかれば「チームメイト本人、またはその配下」
//! で動いているとみなす（チームメイトが起動した入れ子の `claude -p` も含む）。
//!
//! 探索は上限付きで、見つからなかったとき・読めなかったときはメインセッション扱いに倒す
//! （入力だけで判定していた従来と同じ向き）。環境変数 `CLAW_HOOKS_TEAMMATE_PROBE=off` で
//! 走査そのものを止められる（統合テストは子プロセスにこれを渡し、実行環境の祖先に
//! 左右されないようにする）。読んだ argv・名前・パスはログに残さない。

use std::collections::HashSet;
use std::ffi::OsStr;

use tracing::{debug, warn};

use crate::domain::process_tree::{MAX_ARGS_BYTES, ProcessTable, ReadError, SystemProcessTable};

/// 祖先プロセスの走査を止める環境変数。
pub const PROBE_ENV: &str = "CLAW_HOOKS_TEAMMATE_PROBE";

/// 祖先をたどる上限（プロセス数）。
const MAX_DEPTH: usize = 32;

/// 走査全体で OS から読む argv の領域の合計の上限（バイト）。1 プロセスの読み取りには
/// 残りの量（と 1 プロセスの上限 [`MAX_ARGS_BYTES`] の小さい方）を予算として渡すので、
/// 読む量そのものがこの値を超えない。
const MAX_TOTAL_READ_BYTES: usize = 8 * 1024 * 1024;

/// Claude Code がチームメイトの識別に使う CLI オプション（3 つがそろって 1 組）。
const TEAMMATE_OPTIONS: [&[u8]; 3] = [b"--agent-id", b"--agent-name", b"--team-name"];

/// 祖先の走査で得た証拠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamEvidence {
    /// 祖先のどれかの argv に、チームメイトの 3 つのオプションがそろっていた。
    Found,
    /// 最後までたどったが見つからなかった（init・ほかのユーザーのプロセスに着いた）。
    NotFound,
    /// 途中で読めなくなった・上限に達した・形の崩れた argv があった。
    Unavailable,
}

/// Stop を出したセッションがチームメイトかを調べる手段。テストでは固定の結果を返す偽物を渡す。
pub trait TeammateProbe {
    fn evidence(&self) -> TeamEvidence;
}

/// `CLAW_HOOKS_TEAMMATE_PROBE` の値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSetting {
    /// 祖先を走査する（未設定・`auto`）。
    Auto,
    /// 走査しない（`off`）。Stop の入力による判定は変わらない。
    Off,
}

impl ProbeSetting {
    /// 環境変数の値を解釈する。`auto` と `off` だけを小文字の完全一致で受け付け、それ以外
    /// （空文字・`0`・`false`・大文字を含む）は `None` を返す。
    pub fn parse(value: Option<&OsStr>) -> Option<Self> {
        match value {
            None => Some(Self::Auto),
            Some(value) if value == "auto" => Some(Self::Auto),
            Some(value) if value == "off" => Some(Self::Off),
            Some(_) => None,
        }
    }
}

/// 環境変数に従って、Claude Code の Stop の判定に使う判定器を作る（`HookService` が使う）。
///
/// `off` なら `None`（祖先を読まない）。解釈できない値は警告を残して `auto` として扱う。
/// ユニットテストでは実際の祖先を読まないよう常に `None` を返す（テストを Claude Code の
/// チームメイトの中で回しても、判定が変わらないようにする）。
pub fn from_env() -> Option<Box<dyn TeammateProbe>> {
    if cfg!(test) {
        return None;
    }
    let value = std::env::var_os(PROBE_ENV);
    let setting = ProbeSetting::parse(value.as_deref()).unwrap_or_else(|| {
        warn!(
            "⚠️ {} has an unsupported value (bytes={}); expected \"auto\" or \"off\", using auto",
            PROBE_ENV,
            value.as_ref().map_or(0, |v| v.len())
        );
        ProbeSetting::Auto
    });
    match setting {
        ProbeSetting::Auto => Some(Box::new(AncestorProbe::system())),
        ProbeSetting::Off => {
            debug!("{} is off; not reading ancestor processes", PROBE_ENV);
            None
        }
    }
}

/// argv が「チームメイトの識別子の組」を持つかを調べたときの、形の崩れ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedTeamArgs;

/// argv（argv[0] を含む）に、チームメイトの 3 つのオプションがそろっているかを返す。
///
/// Claude CLI 全体の構文解析ではなく、3 つのオプションの検出に限る:
/// - オプション名は完全一致で比べる（部分一致・連結した文字列・引用符の除去はしない）
/// - `--flag value` と `--flag=value` を受け付ける
/// - argv[0] と `--` 以降は調べない
/// - 値の欠落・空や空白だけの値・`--` で始まる値・同じオプションの重複は形の崩れとして
///   `Err` を返す
pub fn has_teammate_options(args: &[Vec<u8>]) -> Result<bool, MalformedTeamArgs> {
    let mut seen = [false; TEAMMATE_OPTIONS.len()];
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_slice();
        if arg == b"--" {
            break;
        }
        let (name, inline_value) = match arg.iter().position(|&b| b == b'=') {
            Some(at) => (&arg[..at], Some(&arg[at + 1..])),
            None => (arg, None),
        };
        if let Some(slot) = TEAMMATE_OPTIONS.iter().position(|option| *option == name) {
            if seen[slot] {
                return Err(MalformedTeamArgs);
            }
            let value = match inline_value {
                Some(value) => value,
                None => {
                    i += 1;
                    args.get(i).ok_or(MalformedTeamArgs)?.as_slice()
                }
            };
            if value.iter().all(u8::is_ascii_whitespace) || value.starts_with(b"--") {
                return Err(MalformedTeamArgs);
            }
            seen[slot] = true;
        }
        i += 1;
    }
    Ok(seen.iter().all(|&present| present))
}

/// 走査の終わり方（ログ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkEnd {
    /// チームメイトの argv を見つけた。
    Found,
    /// PID 1 以下（init / launchd・親なし）に着いた。
    ReachedRoot,
    /// ほかのユーザーのプロセスに着いた。
    OtherUser,
    /// 同じ PID に戻った。
    Cycle,
    /// 深さか読み取り量の上限に達した。
    Limit,
    /// プロセスの情報を読めなかった。
    Read(ReadError),
}

/// フックの祖先プロセスの argv をたどる判定器。
pub struct AncestorProbe<T: ProcessTable> {
    table: T,
    /// 最初に調べるプロセス（claw-hooks の親）。取れない環境では `None`。
    start: Option<u32>,
}

impl<T: ProcessTable> AncestorProbe<T> {
    pub fn new(table: T, start: Option<u32>) -> Self {
        Self { table, start }
    }

    /// 祖先をたどり、証拠・調べたプロセス数・終わり方を返す。
    fn walk(&self) -> (TeamEvidence, usize, WalkEnd) {
        let Some(mut pid) = self.start else {
            return (
                TeamEvidence::Unavailable,
                0,
                WalkEnd::Read(ReadError::Unsupported),
            );
        };
        let mut visited = HashSet::new();
        let mut read_bytes = 0usize;
        // 形の崩れた argv を見た。上で見つからなければ「見つからない」とは言い切らない。
        let mut uncertain = false;
        let settled = |uncertain: bool| {
            if uncertain {
                TeamEvidence::Unavailable
            } else {
                TeamEvidence::NotFound
            }
        };

        for examined in 0..MAX_DEPTH {
            if pid <= 1 {
                return (settled(uncertain), examined, WalkEnd::ReachedRoot);
            }
            if !visited.insert(pid) {
                return (TeamEvidence::Unavailable, examined, WalkEnd::Cycle);
            }
            let budget = MAX_TOTAL_READ_BYTES
                .saturating_sub(read_bytes)
                .min(MAX_ARGS_BYTES);
            let record = match self.table.read(pid, budget) {
                Ok(record) => record,
                Err(ReadError::OtherUser) => {
                    return (settled(uncertain), examined, WalkEnd::OtherUser);
                }
                Err(ReadError::TooLarge) => {
                    return (TeamEvidence::Unavailable, examined, WalkEnd::Limit);
                }
                Err(error) => {
                    return (TeamEvidence::Unavailable, examined, WalkEnd::Read(error));
                }
            };
            read_bytes = read_bytes.saturating_add(record.read_bytes);
            match has_teammate_options(&record.args) {
                Ok(true) => return (TeamEvidence::Found, examined + 1, WalkEnd::Found),
                Ok(false) => {}
                Err(MalformedTeamArgs) => uncertain = true,
            }
            pid = record.ppid;
        }
        (TeamEvidence::Unavailable, MAX_DEPTH, WalkEnd::Limit)
    }
}

impl AncestorProbe<SystemProcessTable> {
    /// claw-hooks 自身の親から OS のプロセス情報をたどる判定器。
    pub fn system() -> Self {
        #[cfg(unix)]
        let start = Some(std::os::unix::process::parent_id());
        #[cfg(not(unix))]
        let start = None;
        Self::new(SystemProcessTable, start)
    }
}

impl<T: ProcessTable> TeammateProbe for AncestorProbe<T> {
    fn evidence(&self) -> TeamEvidence {
        let (evidence, examined, end) = self.walk();
        // argv・名前・パスは残さず、結果と終わり方だけを記録する（Debug Log Safety）。
        debug!(
            ?evidence,
            examined,
            end = ?end,
            "Claude Code teammate probe"
        );
        evidence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::process_tree::ProcessRecord;
    use std::collections::HashMap;

    fn argv(args: &[&str]) -> Vec<Vec<u8>> {
        args.iter().map(|arg| arg.as_bytes().to_vec()).collect()
    }

    // === has_teammate_options ===

    #[test]
    fn test_separate_values_are_detected() {
        let args = argv(&[
            "/Users/me/.local/bin/claude",
            "--agent-id",
            "docs@session-1",
            "--agent-name",
            "docs",
            "--team-name",
            "session-1",
            "--agent-color",
            "blue",
        ]);
        assert_eq!(has_teammate_options(&args), Ok(true));
    }

    #[test]
    fn test_inline_and_mixed_values_are_detected() {
        let args = argv(&[
            "claude",
            "--agent-id=docs@session-1",
            "--agent-name",
            "docs",
            "--team-name=session-1",
        ]);
        assert_eq!(has_teammate_options(&args), Ok(true));
    }

    #[test]
    fn test_missing_option_is_not_detected() {
        let args = argv(&["claude", "--agent-id", "docs@t", "--team-name", "t"]);
        assert_eq!(has_teammate_options(&args), Ok(false));
        // `--agent-type` だけ（`--agent` で起動したメインセッションに相当）は対象外
        let args = argv(&["claude", "--agent-type", "general-purpose"]);
        assert_eq!(has_teammate_options(&args), Ok(false));
    }

    #[test]
    fn test_argv0_and_arguments_after_terminator_are_ignored() {
        let args = argv(&[
            "--agent-id",
            "--agent-name",
            "x",
            "--team-name",
            "t",
            "--",
            "--agent-id",
            "x@t",
        ]);
        // argv[0] は実行名なので数えない。`--agent-name` と `--team-name` はそろうが、
        // `--agent-id` は `--` の後にしか無い。
        assert_eq!(has_teammate_options(&args), Ok(false));
    }

    #[test]
    fn test_options_inside_one_argument_are_not_detected() {
        // シェルの `-c` の本文のように 1 つの引数の中にあるだけなら数えない
        let args = argv(&[
            "sh",
            "-c",
            "claude --agent-id x@t --agent-name x --team-name t",
        ]);
        assert_eq!(has_teammate_options(&args), Ok(false));
    }

    #[test]
    fn test_prefix_match_is_not_detected() {
        let args = argv(&[
            "claude",
            "--agent-identity",
            "x@t",
            "--agent-name",
            "x",
            "--team-name",
            "t",
        ]);
        assert_eq!(has_teammate_options(&args), Ok(false));
    }

    #[test]
    fn test_malformed_values_are_errors() {
        for args in [
            // 値の欠落
            argv(&[
                "claude",
                "--agent-name",
                "x",
                "--team-name",
                "t",
                "--agent-id",
            ]),
            // 空・空白だけの値
            argv(&[
                "claude",
                "--agent-id=",
                "--agent-name",
                "x",
                "--team-name",
                "t",
            ]),
            argv(&[
                "claude",
                "--agent-id",
                " ",
                "--agent-name",
                "x",
                "--team-name",
                "t",
            ]),
            // 値が次のオプション
            argv(&[
                "claude",
                "--agent-id",
                "--agent-name",
                "x",
                "--team-name",
                "t",
            ]),
            // 重複
            argv(&[
                "claude",
                "--agent-id",
                "x@t",
                "--agent-id",
                "y@t",
                "--agent-name",
                "x",
                "--team-name",
                "t",
            ]),
        ] {
            assert_eq!(
                has_teammate_options(&args),
                Err(MalformedTeamArgs),
                "{args:?}"
            );
        }
    }

    // === AncestorProbe ===

    /// PID ごとの読み取り結果を固定で返す偽のプロセス表。読んだ量は argv の合計
    /// （区切りの NUL を含む）とし、予算を超えるものは OS 実装と同じく `TooLarge` にする。
    struct FakeTable(HashMap<u32, Result<ProcessRecord, ReadError>>);

    impl FakeTable {
        fn new() -> Self {
            Self(HashMap::new())
        }

        fn process(mut self, pid: u32, ppid: u32, args: &[&str]) -> Self {
            let args = argv(args);
            let read_bytes = args.iter().map(|arg| arg.len() + 1).sum();
            self.0.insert(
                pid,
                Ok(ProcessRecord {
                    ppid,
                    args,
                    read_bytes,
                }),
            );
            self
        }

        fn error(mut self, pid: u32, error: ReadError) -> Self {
            self.0.insert(pid, Err(error));
            self
        }
    }

    impl ProcessTable for FakeTable {
        fn read(&self, pid: u32, budget: usize) -> Result<ProcessRecord, ReadError> {
            let record = self.0.get(&pid).cloned().unwrap_or(Err(ReadError::Gone))?;
            if record.read_bytes > budget {
                return Err(ReadError::TooLarge);
            }
            Ok(record)
        }
    }

    const TEAMMATE: &[&str] = &[
        "claude",
        "--agent-id",
        "docs@session-1",
        "--agent-name",
        "docs",
        "--team-name",
        "session-1",
    ];

    fn walk(table: FakeTable, start: u32) -> (TeamEvidence, usize, WalkEnd) {
        AncestorProbe::new(table, Some(start)).walk()
    }

    #[test]
    fn test_teammate_parent_is_found() {
        // claude（チームメイト）が直接フックを起動した形
        let table = FakeTable::new().process(100, 50, TEAMMATE);
        assert_eq!(walk(table, 100), (TeamEvidence::Found, 1, WalkEnd::Found));
    }

    #[test]
    fn test_teammate_beyond_shell_and_nested_session_is_found() {
        // チームメイト → zsh（Bash ツール）→ 入れ子の claude -p → sh → claw-hooks
        let table = FakeTable::new()
            .process(400, 300, &["sh", "-c", "claw-hooks hook"])
            .process(300, 200, &["claude", "-p", "review"])
            .process(200, 100, &["/bin/zsh", "-c", "claude -p review"])
            .process(100, 50, TEAMMATE)
            .process(50, 1, &["tmux"]);
        assert_eq!(walk(table, 400), (TeamEvidence::Found, 4, WalkEnd::Found));
    }

    #[test]
    fn test_leader_reaches_root_without_evidence() {
        let table = FakeTable::new()
            .process(300, 200, &["claude", "--model", "opus"])
            .process(200, 100, &["-zsh"])
            .process(100, 1, &["tmux", "new-session"]);
        assert_eq!(
            walk(table, 300),
            (TeamEvidence::NotFound, 3, WalkEnd::ReachedRoot)
        );
    }

    #[test]
    fn test_other_user_ends_walk_without_evidence() {
        let table = FakeTable::new()
            .process(300, 200, &["claude"])
            .process(200, 100, &["-zsh"])
            .error(100, ReadError::OtherUser);
        assert_eq!(
            walk(table, 300),
            (TeamEvidence::NotFound, 2, WalkEnd::OtherUser)
        );
    }

    #[test]
    fn test_options_split_across_processes_are_not_combined() {
        let table = FakeTable::new()
            .process(300, 200, &["claude", "--agent-id", "docs@t"])
            .process(200, 100, &["claude", "--agent-name", "docs"])
            .process(100, 1, &["claude", "--team-name", "t"]);
        assert_eq!(
            walk(table, 300),
            (TeamEvidence::NotFound, 3, WalkEnd::ReachedRoot)
        );
    }

    #[test]
    fn test_read_failure_is_unavailable() {
        for error in [
            ReadError::Gone,
            ReadError::Changed,
            ReadError::Unreadable,
            ReadError::Unsupported,
        ] {
            let table = FakeTable::new()
                .process(300, 200, &["sh"])
                .error(200, error);
            assert_eq!(
                walk(table, 300),
                (TeamEvidence::Unavailable, 1, WalkEnd::Read(error))
            );
        }
    }

    #[test]
    fn test_cycle_is_unavailable() {
        let table = FakeTable::new()
            .process(300, 200, &["sh"])
            .process(200, 300, &["sh"]);
        assert_eq!(
            walk(table, 300),
            (TeamEvidence::Unavailable, 2, WalkEnd::Cycle)
        );
    }

    #[test]
    fn test_depth_limit_is_unavailable() {
        let mut table = FakeTable::new();
        for pid in 2..=(MAX_DEPTH as u32 + 2) {
            table = table.process(pid, pid + 1, &["sh"]);
        }
        assert_eq!(
            walk(table, 2),
            (TeamEvidence::Unavailable, MAX_DEPTH, WalkEnd::Limit)
        );
        // 上限ちょうどの深さにあるチームメイトは見つかる
        let mut table = FakeTable::new();
        for pid in 2..(MAX_DEPTH as u32 + 1) {
            table = table.process(pid, pid + 1, &["sh"]);
        }
        table = table.process(MAX_DEPTH as u32 + 1, 1, TEAMMATE);
        assert_eq!(
            walk(table, 2),
            (TeamEvidence::Found, MAX_DEPTH, WalkEnd::Found)
        );
    }

    #[test]
    fn test_process_over_per_process_budget_is_unavailable() {
        let big = "x".repeat(MAX_ARGS_BYTES);
        let table = FakeTable::new()
            .process(300, 100, &["sh", &big])
            .process(100, 1, TEAMMATE);
        assert_eq!(
            walk(table, 300),
            (TeamEvidence::Unavailable, 0, WalkEnd::Limit)
        );
    }

    #[test]
    fn test_total_read_budget_is_unavailable() {
        // 1 プロセスでちょうど 1 プロセスの上限を読む（"sh" と区切りの NUL 2 つで 4 バイト）。
        // 合計の予算を使い切った後は、チームメイトの小さな argv も読まない。
        let chunk = "x".repeat(MAX_ARGS_BYTES - 4);
        let fits = MAX_TOTAL_READ_BYTES / MAX_ARGS_BYTES;
        let mut table = FakeTable::new();
        for pid in 2..2 + fits as u32 {
            table = table.process(pid, pid + 1, &["sh", &chunk]);
        }
        table = table.process(2 + fits as u32, 1, TEAMMATE);
        assert_eq!(
            walk(table, 2),
            (TeamEvidence::Unavailable, fits, WalkEnd::Limit)
        );
    }

    #[test]
    fn test_malformed_ancestor_does_not_hide_teammate_above() {
        let table = FakeTable::new()
            .process(300, 200, &["claude", "--agent-id"])
            .process(200, 100, TEAMMATE);
        assert_eq!(walk(table, 300), (TeamEvidence::Found, 2, WalkEnd::Found));
    }

    #[test]
    fn test_malformed_ancestor_without_teammate_is_unavailable() {
        let table = FakeTable::new()
            .process(300, 200, &["claude", "--agent-id"])
            .process(200, 1, &["-zsh"]);
        assert_eq!(
            walk(table, 300),
            (TeamEvidence::Unavailable, 2, WalkEnd::ReachedRoot)
        );
    }

    #[test]
    fn test_missing_start_is_unavailable() {
        let probe = AncestorProbe::new(FakeTable::new(), None);
        assert_eq!(probe.evidence(), TeamEvidence::Unavailable);
    }

    #[test]
    fn test_start_at_root_is_not_found() {
        assert_eq!(
            walk(FakeTable::new(), 1),
            (TeamEvidence::NotFound, 0, WalkEnd::ReachedRoot)
        );
        assert_eq!(
            walk(FakeTable::new(), 0),
            (TeamEvidence::NotFound, 0, WalkEnd::ReachedRoot)
        );
    }

    // === ProbeSetting ===

    #[test]
    fn test_probe_setting_accepts_only_auto_and_off() {
        assert_eq!(ProbeSetting::parse(None), Some(ProbeSetting::Auto));
        assert_eq!(
            ProbeSetting::parse(Some(OsStr::new("auto"))),
            Some(ProbeSetting::Auto)
        );
        assert_eq!(
            ProbeSetting::parse(Some(OsStr::new("off"))),
            Some(ProbeSetting::Off)
        );
        for value in ["", "OFF", "Off", "0", "false", "no", " off"] {
            assert_eq!(
                ProbeSetting::parse(Some(OsStr::new(value))),
                None,
                "{value:?}"
            );
        }
    }

    #[test]
    fn test_from_env_never_reads_processes_in_unit_tests() {
        assert!(from_env().is_none());
    }
}
