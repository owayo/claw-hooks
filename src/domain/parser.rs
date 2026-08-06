//! シェルコマンドパーサー。
//!
//! シェルコマンド文字列からコマンドを抽出する機能を提供する。
//! `ast-parser` フィーチャーが有効な場合、tree-sitter-bash による正確な AST ベースの解析を使用する。

#[cfg(feature = "ast-parser")]
use tree_sitter::{Node, Parser};

use std::cell::Cell;

/// 再パースを伴う再帰解析（xargs / eval / find -exec / shell -c / env -S の内側
/// コマンド再評価）の深さ上限。深いネスト入力は再帰下降解析でスタックを溢れさせ
/// SIGABRT で異常終了（フェイルオープン）し得るため、上限超過時は解析を諦め安全側
/// （危険コマンド候補）に倒して fail-closed を保つ。正当なコマンドでこの深さに
/// 達することはまずない。
const MAX_RECURSION_DEPTH: usize = 64;

/// AST ノードの走査深さ上限。tree-sitter のパース自体は反復処理でスタックを
/// 溢れさせないが、AST を再帰下降で走査する `extract_commands_from_node` /
/// `extract_command_strings_from_node` は木の深さに比例してスタックを消費する。
/// 深いコマンドグループ `{ ...;}` 等は `is_pathological_command` をすり抜けて
/// 深い AST を生み得るため、走査再帰でスタックオーバーフロー（SIGABRT＝fail-open）
/// を起こし得る。正当なコマンドがこの深さに達することはまず無いため、超過時は
/// 解析を諦め安全側（危険コマンド候補）に倒して fail-closed を保つ。
#[cfg(feature = "ast-parser")]
const MAX_NODE_DEPTH: usize = 500;

/// 括弧 `()`・ブレース `{}` のネスト深さ上限（病的入力ガード）。
/// `is_pathological_command` が合算ネスト深さをこの値と比較し、超過する入力は
/// 解析を諦め安全側（危険コマンド候補）に倒して fail-closed を保つ。
/// 正当なコマンドで超えることはまずない。
const MAX_NESTING_DEPTH: usize = 96;

thread_local! {
    /// 再帰解析の現在の深さ（スレッドローカル）。同一スレッドの再帰チェーン内で
    /// 共有され、`RecursionGuard` により increment / decrement される。
    static RECURSION_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// 再帰深さを管理する RAII ガード。
///
/// `enter()` で深さを +1 し、スコープを抜けると `Drop` で -1 する。
/// early return や panic でも確実にデクリメントされるためカウンタがリークしない。
struct RecursionGuard;

impl RecursionGuard {
    /// 深さを +1 する。すでに上限に達している場合は `None` を返す
    /// （これ以上再帰せず解析を諦めるべき合図）。
    fn enter() -> Option<Self> {
        RECURSION_DEPTH.with(|d| {
            let depth = d.get();
            if depth >= MAX_RECURSION_DEPTH {
                None
            } else {
                d.set(depth + 1);
                Some(RecursionGuard)
            }
        })
    }
}

impl Drop for RecursionGuard {
    fn drop(&mut self) {
        RECURSION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// 実コマンドを実行するラッパーコマンド。
///
/// `pkexec`（Polkit）と `gosu`（コンテナ向けの軽量 setuid 代替）は `sudo` / `doas` と同様に
/// 後続の引数を昇格権限で実行するため、ここで認識しないと `pkexec rm -rf /` や
/// `gosu root rm -rf /` のように危険コマンド検出を root 権限ごとバイパスされる。
///
/// `arch` は macOS 標準の `/usr/bin/arch` で、`arch -arm64 rm -rf /` のように
/// アーキテクチャを指定して後続コマンドを実行する（`env` / `nice` と同じ実行委譲）。
/// Linux の coreutils `arch` は引数を無視して機種名を表示するだけなので、そこでは
/// 過剰ブロックになり得るが、フェイルクローズ方針に従い検出漏れより過剰ブロックを選ぶ。
///
/// `systemd-run` は一時ユニットとしてコマンドを実行し、`--uid` / `--machine` 等で
/// 特権委譲も伴うため、`pkexec` / `gosu` と同じ理由で認識する必要がある。
///
/// `script` は擬似端末を割り当てて後続コマンドを実行する（BSD/macOS の
/// `script -q /dev/null rm -rf /` 形式）。Linux 版は `-c` でコマンド文字列を取るため
/// `SHELL_COMMANDS` にも登録して両形式を捕捉する。
const COMMAND_WRAPPERS: &[&str] = &[
    "sudo",
    "env",
    "nohup",
    "nice",
    "ionice",
    "time",
    "timeout",
    "strace",
    "ltrace",
    "doas",
    "command",
    "exec",
    "setsid",
    "stdbuf",
    "unshare",
    "nsenter",
    "setpriv",
    "chroot",
    "flock",
    "taskset",
    "watch",
    "busybox",
    "toybox",
    "runuser",
    "pkexec",
    "gosu",
    "su",
    "arch",
    "systemd-run",
    "script",
];

/// -c フラグでコマンド文字列を実行できるシェル / シェル相当（`su -c` 等）。
/// `su` / `runuser` は `-c "cmd"` を sh 経由で実行し、`flock` は `<file> -c cmd` 形式を
/// 取るため、いずれも shell -c と同様に内側コマンド文字列を再評価する必要がある。
const SHELL_COMMANDS: &[&str] = &[
    "bash", "sh", "zsh", "ksh", "csh", "tcsh", "fish", "dash", "cmd", "su", "runuser", "flock",
];

/// find で後続引数をコマンドとして実行する述語
const FIND_EXEC_PREDICATES: &[&str] = &["-exec", "-execdir"];

/// ラッパーごとの「値を次トークンから取る」フラグ仕様（`WRAPPER_FLAG_SPECS` の要素）。
struct WrapperFlagSpec {
    /// 対象ラッパーの正規化済みコマンドキー（`command_key` の出力）。
    /// strace/ltrace のように同じフラグ仕様を共有するツールは 1 エントリに複数キーを持つ。
    keys: &'static [&'static str],
    /// 値を次トークンから取る短縮フラグ。
    short: &'static [&'static str],
    /// 値を次トークンから取る長形フラグ。
    long: &'static [&'static str],
}

/// ラッパーごとの「値を次トークンから取る」フラグ表。
///
/// 値取得フラグの過不足は「実コマンドの取りこぼし（フェイルオープン）」を生むだけで
/// 過剰ブロックは生じないため、各ツールの実フラグ仕様に合わせて短縮形・長形ともに
/// 網羅する。任意引数フラグ（例: watch -d）は `-d <cmd>` 形が一般的なので boolean 扱い
/// （値を取らない）にして次トークンのコマンドを取りこぼさない。
///
/// 短縮形と長形を 1 エントリに併記して単一の表に集約することで、ラッパー追加時の
/// 同期漏れ（短縮形だけ追加して長形を忘れる等 = fail-open）を構造的に防ぐ。
/// ここに無いラッパー（setsid/nohup/command/pkexec/gosu/busybox 等）は
/// 値取得フラグを持たない扱い。
const WRAPPER_FLAG_SPECS: &[WrapperFlagSpec] = &[
    WrapperFlagSpec {
        keys: &["sudo"],
        short: &[
            "-u", "-g", "-C", "-D", "-R", "-T", "-h", "-p", "-r", "-t", "-U",
        ],
        long: &[
            "--user",
            "--group",
            "--host",
            "--chdir",
            "--prompt",
            "--other-user",
        ],
    },
    WrapperFlagSpec {
        keys: &["env"],
        short: &["-u", "-C", "-S"],
        long: &[
            "--unset",
            "--chdir",
            "--argv0",
            "--split-string",
            "--block-signal",
            "--default-signal",
            "--ignore-signal",
        ],
    },
    WrapperFlagSpec {
        keys: &["timeout"],
        short: &["-k", "-s"],
        long: &["--signal", "--kill-after"],
    },
    WrapperFlagSpec {
        keys: &["nice"],
        short: &["-n"],
        long: &["--adjustment"],
    },
    WrapperFlagSpec {
        keys: &["ionice"],
        short: &["-c", "-n"],
        long: &["--class", "--classdata"],
    },
    WrapperFlagSpec {
        keys: &["doas"],
        short: &["-u"],
        long: &[],
    },
    WrapperFlagSpec {
        keys: &["exec"],
        short: &["-a"],
        long: &[],
    },
    WrapperFlagSpec {
        keys: &["stdbuf"],
        short: &["-i", "-o", "-e"],
        long: &["--input", "--output", "--error"],
    },
    WrapperFlagSpec {
        keys: &["taskset"],
        short: &["-c", "-p"],
        long: &["--cpu-list", "--pid"],
    },
    WrapperFlagSpec {
        keys: &["watch"],
        short: &["-n"],
        long: &["--interval"],
    },
    WrapperFlagSpec {
        keys: &["nsenter"],
        short: &["-t", "-S", "-G"],
        long: &["--target", "--setuid", "--setgid", "--root", "--wd"],
    },
    WrapperFlagSpec {
        keys: &["unshare"],
        short: &["-R", "-S", "-G"],
        long: &[
            "--map-user",
            "--map-group",
            "--setgroups",
            "--root",
            "--setuid",
            "--setgid",
            "--wd",
        ],
    },
    WrapperFlagSpec {
        keys: &["runuser"],
        short: &["-u", "-g", "-c", "-G", "-s", "-w"],
        long: &[
            "--user",
            "--group",
            "--command",
            "--supp-group",
            "--shell",
            "--session-command",
            "--whitelist-environment",
        ],
    },
    // `su` のオプション（GNU coreutils と util-linux 系で共通の主要なもの）。
    // 値取得フラグを定義しないと、`su -s /bin/bash user rm` で `/bin/bash` を leading
    // positional として消費し `user` をコマンドと誤認するため、検出漏れになる。
    WrapperFlagSpec {
        keys: &["su"],
        short: &["-c", "-g", "-G", "-s"],
        long: &[
            "--command",
            "--group",
            "--supp-group",
            "--shell",
            "--session-command",
        ],
    },
    WrapperFlagSpec {
        keys: &["chroot"],
        short: &[],
        long: &["--userspec", "--groups"],
    },
    WrapperFlagSpec {
        keys: &["setpriv"],
        short: &[],
        long: &[
            "--reuid",
            "--regid",
            "--groups",
            "--inh-caps",
            "--ambient-caps",
            "--bounding-set",
            "--securebits",
            "--pdeathsig",
            "--selinux-label",
            "--apparmor-profile",
        ],
    },
    WrapperFlagSpec {
        keys: &["flock"],
        short: &["-w", "-E"],
        long: &["--timeout", "--conflict-exit-code"],
    },
    WrapperFlagSpec {
        keys: &["time"],
        short: &["-o", "-f"],
        long: &["--output", "--format"],
    },
    // strace / ltrace の値取得フラグ（出力先・式・PID・サイズ等）。`-o` を取りこぼすと
    // `strace -o file rm` で rm を見落とすため網羅する。両ツールで共用する。
    WrapperFlagSpec {
        keys: &["strace", "ltrace"],
        short: &[
            "-o", "-e", "-p", "-s", "-E", "-P", "-a", "-S", "-u", "-b", "-l", "-X",
        ],
        long: &["--output", "--expression", "--attach"],
    },
];

/// 正規化済みキーに対応するラッパーのフラグ仕様を返す。
fn wrapper_flag_spec(wrapper_key: &str) -> Option<&'static WrapperFlagSpec> {
    WRAPPER_FLAG_SPECS
        .iter()
        .find(|spec| spec.keys.contains(&wrapper_key))
}

/// 再評価系コマンドがシェルとして再評価する内側コマンド文字列
/// （`ShellParser::reevaluated_inner_command_strings` の要素）。
struct ReevaluatedInner {
    /// 再評価される内側コマンド文字列。
    text: String,
    /// 内側文字列そのものを 1 つの完全コマンド文字列として扱うべきか。
    /// xargs のみ true: `xargs rm -rf` の `rm -rf` は再帰解析とは別に、それ自体が
    /// アンカー付きカスタムフィルタ（`^rm`）の照合対象となる完全コマンド文字列になる。
    /// コマンド名抽出経路では再帰解析が名前を拾うため、このフラグは参照しない。
    raw_is_command_string: bool,
}

/// コマンド名を判定用のキーに正規化する。
///
/// 以下を行う:
/// - basename 抽出（`/bin/rm`、`./rm`、`C:\Windows\rm.exe` → `rm`）
/// - 実行可能ファイル拡張子の除去（`.exe`、`.cmd`、`.bat`、`.com`、大文字小文字を問わない）
/// - 小文字化（`DEL`、`Rm` → `del`、`rm`）
///
/// これにより `/bin/rm`・`cmd.exe`・`DEL` などのパス・拡張子・大文字経由のバイパスを防ぐ。
pub(crate) fn command_key(command: &str) -> String {
    // コマンド名位置のブレース展開を最初の選択肢へ畳んでから正規化する。
    // bash は `{rm,-rf,/p}` を実行前に展開し先頭 `rm` を起動するため、展開しないと
    // 末尾断片（`p}`）が leaf になり危険コマンド判定をすり抜ける。
    let expanded = expand_braces_first_choice(command);
    let leaf = expanded
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(&expanded);
    let lower = leaf.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stripped) = lower.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    lower
}

/// コマンド名位置のブレース展開を「最初の選択肢」で1つの語に畳む。
///
/// bash はコマンド位置の `{rm,-rf,/p}` を実行前に `rm -rf /p` へ展開し、先頭トークン
/// `rm` を起動コマンドとする。`/bin/{rm,ls}` は `/bin/rm /bin/ls`、`{r,}m` は `rm m`
/// に展開され、いずれも最初の選択肢が起動コマンドになる。セキュリティ判定では最初の
/// 選択肢のみが実行コマンドになり得るため、それを取り出す。ネスト `{r{m,d},ls}` や
/// 接頭辞付き `pre{a,b}` も再帰的に畳む。引用された `'{rm,ls}'` は本来展開されないが、
/// ここでは安全側（過剰に展開）に倒して fail-closed を保つ。
fn expand_braces_first_choice(word: &str) -> String {
    if !word.contains('{') {
        return word.to_string();
    }
    let mut out = String::with_capacity(word.len());
    expand_braces_into(word, &mut out, 0);
    out
}

/// ブレース展開の再帰深さ上限。病的入力ガード（`MAX_NESTING_DEPTH`）と同値でなければ
/// ならない: runtime 経路ではブレースネストがこれを超える入力は `is_pathological_command`
/// が事前に block するため、この値が実際に到達し得る再帰深さの上限になる。
/// 超過時は残りをそのまま追記してスタックオーバーフローを避ける。
const MAX_BRACE_EXPAND_DEPTH: usize = MAX_NESTING_DEPTH;

/// `s` を1パスで走査し、コマンド名位置のブレース展開を「最初の非空選択肢」で畳んだ結果を
/// `out` へ追記する。選んだ選択肢は再帰的に展開する。各バイトは1回だけ走査・コピーされる
/// ため全体は線形時間（反復ごとに文字列全体を作り直さない）。
fn expand_braces_into(s: &str, out: &mut String, depth: usize) {
    if depth > MAX_BRACE_EXPAND_DEPTH {
        out.push_str(s);
        return;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut lit_start = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && (i == 0 || bytes[i - 1] != b'$') {
            if let Some((close, alt_start, alt_end)) = brace_group_at(bytes, i) {
                out.push_str(&s[lit_start..i]); // 群より前の literal
                expand_braces_into(&s[alt_start..alt_end], out, depth + 1); // 選択肢を再帰展開
                i = close + 1;
                lit_start = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[lit_start..]); // 末尾の literal
}

/// `bytes[open] == b'{'` を前提に、その位置から始まる「展開可能な」ブレース群について
/// (閉じ `}` の位置, 採用する選択肢の開始, 採用する選択肢の終了) のバイト位置を返す。
/// bash はブレース展開後の最初の「非空」ワードを起動コマンドにするため（`{,rm}` は `rm` を
/// 起動する）、トップレベルのカンマ区切りのうち最初の非空な選択肢を採用する。全選択肢が空
/// （`{,}`）なら空へ畳む。カンマを持たない群（`{ cmd; }`）や閉じない群は展開対象外として
/// None を返す。返す位置はすべて ASCII バイト境界なので、後続のスライスは UTF-8 境界上で安全。
fn brace_group_at(bytes: &[u8], open: usize) -> Option<(usize, usize, usize)> {
    let mut depth = 1usize;
    let mut alt_start = open + 1;
    let mut first_nonempty: Option<(usize, usize)> = None;
    let mut has_comma = false;
    let mut j = open + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if !has_comma {
                        return None; // トップレベルにカンマが無い → ブレース展開ではない
                    }
                    // 末尾の選択肢 alt_start..j も候補に含める。
                    if first_nonempty.is_none() && j > alt_start {
                        first_nonempty = Some((alt_start, j));
                    }
                    // 非空の選択肢が無ければ（`{,}` 等）空へ畳む。
                    let (s0, e0) = first_nonempty.unwrap_or((open + 1, open + 1));
                    return Some((j, s0, e0));
                }
            }
            b',' if depth == 1 => {
                has_comma = true;
                if first_nonempty.is_none() && j > alt_start {
                    first_nonempty = Some((alt_start, j));
                }
                alt_start = j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// コマンド全体のブレース展開を最初の選択肢へ畳んだ文字列を返す（元と異なる場合のみ）。
///
/// tree-sitter-bash は先頭 `{` のブレース展開（`{rm,-rf,/p}`）をコマンドではなく
/// コマンドグループ等として解釈し、`rm` を command_name として取りこぼす。畳んだ
/// 文字列（`rm`）を再解析することで、AST 経路でもこのバイパスを fail-closed に塞ぐ。
/// 畳み込みは選択肢を1つに減らすだけなので長さは増えず、再帰解析は必ず停止する。
#[cfg(feature = "ast-parser")]
fn brace_expanded_command(command: &str) -> Option<String> {
    if !command.contains('{') {
        return None;
    }
    let expanded = expand_braces_first_choice(command);
    (expanded != command).then_some(expanded)
}

/// SHELL_COMMANDS と一致するかを正規化キーで判定する（テスト専用）。
/// 本番経路は `reevaluated_inner_command_strings` が正規化済みキーで直接判定する。
#[cfg(test)]
fn is_shell_command(name: &str) -> bool {
    SHELL_COMMANDS.contains(&command_key(name).as_str())
}

/// COMMAND_WRAPPERS と一致するかを正規化キーで判定する。
fn is_command_wrapper(name: &str) -> bool {
    let key = command_key(name);
    COMMAND_WRAPPERS.iter().any(|s| *s == key)
}

/// tree-sitter-bash を使用した AST ベースのシェルコマンドパーサー。
pub struct ShellParser {
    #[cfg(feature = "ast-parser")]
    parser: Parser,
}

impl ShellParser {
    /// 新しい ShellParser を作成する。
    pub fn new() -> Self {
        #[cfg(feature = "ast-parser")]
        {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_bash::LANGUAGE.into())
                .expect("Failed to load tree-sitter-bash grammar");
            Self { parser }
        }
        #[cfg(not(feature = "ast-parser"))]
        {
            Self {}
        }
    }

    /// シェルコマンド文字列からコマンドを抽出する。
    ///
    /// 対応する構文:
    /// - パイプライン (|)
    /// - 論理演算子 (&&, ||)
    /// - セミコロン (;)
    /// - ラッパーコマンド (sudo, env, nohup 等)
    /// - サブシェル (bash -c, sh -c 等)
    /// - xargs 付きコマンド
    #[cfg(feature = "ast-parser")]
    pub fn extract_commands(&mut self, command: &str) -> Vec<String> {
        // スタックオーバーフロー対策: 再帰が深すぎる、または病的に深い/長い入力は
        // 再帰解析を諦め、安全側（危険コマンド候補）に倒して fail-closed を保つ。
        let Some(_guard) = RecursionGuard::enter() else {
            return Self::pathological_block_commands();
        };
        if Self::is_pathological_command(command) {
            return Self::pathological_block_commands();
        }
        let tree = match self.parser.parse(command, None) {
            Some(tree) => tree,
            None => return self.extract_commands_fallback(command),
        };

        let root = tree.root_node();
        let mut commands = Vec::new();
        // 文字列検索の代わりに AST ベースの引数抽出を使用して
        // ラッパーとサブシェルを extract_commands_from_node 内で直接処理する
        self.extract_commands_from_node(root, command, &mut commands, 0);

        // tree-sitter は先頭 `{` のブレース展開（`{rm,...}`）をコマンドとして解釈せず
        // 取りこぼすため、ブレースを最初の選択肢へ畳んだ文字列も解析して補完する。
        if let Some(expanded) = brace_expanded_command(command) {
            for nested in self.extract_commands(&expanded) {
                Self::push_unique_command(&mut commands, &nested);
            }
        }

        commands
    }

    #[cfg(not(feature = "ast-parser"))]
    pub fn extract_commands(&mut self, command: &str) -> Vec<String> {
        let Some(_guard) = RecursionGuard::enter() else {
            return Self::pathological_block_commands();
        };
        if Self::is_pathological_command(command) {
            return Self::pathological_block_commands();
        }
        self.extract_commands_fallback(command)
    }

    /// 解析対象コマンドが病的に深い/長いか（スタックオーバーフロー対策）。
    /// 多段にネストしたコマンド置換（`$( $( ... ) )` の連鎖）や極端に長い入力は
    /// 再帰下降解析でスタックを溢れさせ、SIGABRT で異常終了（フェイルオープン）し得る。
    /// 上限超過時は解析を諦め、安全側（危険コマンド候補）に倒すための判定。
    fn is_pathological_command(command: &str) -> bool {
        /// コマンド文字列長の上限（バイト）。
        const MAX_COMMAND_LEN: usize = 65_536;

        if command.len() > MAX_COMMAND_LEN {
            return true;
        }
        // 括弧 `()` に加えてブレース `{}`（コマンドグループ／ブレース展開）も計数する。
        // 深いコマンドグループ `{ ...;}` は括弧を使わずにネストでき、括弧のみの計数を
        // すり抜けて深い AST を生み、走査再帰でスタックを溢れさせ得るため。両者を合算した
        // 保守的な深さで判定する（多少過大評価しても安全側に倒れるだけ）。
        let mut depth: usize = 0;
        let mut max_depth: usize = 0;
        for b in command.bytes() {
            match b {
                b'(' | b'{' => {
                    depth += 1;
                    if depth > max_depth {
                        max_depth = depth;
                    }
                }
                b')' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        max_depth > MAX_NESTING_DEPTH
    }

    /// 病的入力に対して返す、安全側（危険コマンド候補）のコマンド一覧。
    /// builtin フィルタがこれらを検出して block するため fail-closed になる。
    fn pathological_block_commands() -> Vec<String> {
        vec!["rm".to_string(), "kill".to_string(), "dd".to_string()]
    }

    /// シェルコマンド文字列から完全なコマンド文字列（コマンド名 + 引数）を抽出する。
    /// "npm install" のようなパターンをマッチするためにカスタムフィルターで使用される。
    #[cfg(feature = "ast-parser")]
    pub fn extract_command_strings(&mut self, command: &str) -> Vec<String> {
        // extract_commands と同様にスタックオーバーフロー対策を行う。
        // 従来この経路には再帰上限も病的入力チェックも無く、深いネストで
        // スタックを溢れさせ fail-open になり得た。
        let Some(_guard) = RecursionGuard::enter() else {
            return Self::pathological_block_commands();
        };
        if Self::is_pathological_command(command) {
            return Self::pathological_block_commands();
        }
        let tree = match self.parser.parse(command, None) {
            Some(tree) => tree,
            None => return self.extract_command_strings_fallback(command),
        };

        let root = tree.root_node();
        let mut command_strings = Vec::new();
        self.extract_command_strings_from_node(root, command, &mut command_strings, 0);

        // 先頭ブレース展開の取りこぼしを補うため、畳んだ文字列も解析する。
        if let Some(expanded) = brace_expanded_command(command) {
            for nested in self.extract_command_strings(&expanded) {
                if !command_strings.contains(&nested) {
                    command_strings.push(nested);
                }
            }
        }

        command_strings
    }

    #[cfg(not(feature = "ast-parser"))]
    pub fn extract_command_strings(&mut self, command: &str) -> Vec<String> {
        let Some(_guard) = RecursionGuard::enter() else {
            return Self::pathological_block_commands();
        };
        if Self::is_pathological_command(command) {
            return Self::pathological_block_commands();
        }
        self.extract_command_strings_fallback(command)
    }

    /// AST ノードから完全なコマンド文字列を再帰的に抽出する。
    /// 正確なパターンマッチングのためにクォートを保持した生の引数を使用する。
    #[cfg(feature = "ast-parser")]
    fn extract_command_strings_from_node(
        &mut self,
        node: Node,
        source: &str,
        command_strings: &mut Vec<String>,
        depth: usize,
    ) {
        // AST 走査の再帰がスタックを溢れさせないよう深さ上限を課す。超過時は
        // 解析を諦め安全側（危険コマンド候補）に倒して fail-closed を保つ。
        if depth > MAX_NODE_DEPTH {
            command_strings.extend(Self::pathological_block_commands());
            return;
        }
        match node.kind() {
            "command" | "simple_command" => {
                if let Some(cmd_name) = self.get_command_name(node, source) {
                    if !cmd_name.is_empty() {
                        // 完全なコマンド文字列を構築: コマンド + 引数（クォート保持）
                        let args_raw = self.get_command_arguments_raw(node, source);
                        let full_cmd = if args_raw.is_empty() {
                            cmd_name.clone()
                        } else {
                            format!("{} {}", cmd_name, args_raw.join(" "))
                        };
                        command_strings.push(full_cmd);

                        // 内側コマンドの抽出にはクォート除去済み引数を使用（一度だけ取得して共有）
                        let args = self.get_command_arguments(node, source);

                        // 実行委譲ラッパー（sudo/env/setsid/flock/...）の内側コマンド文字列も
                        // 抽出する。これを行わないと `^npm install` のようなアンカー付きカスタム
                        // フィルタが `sudo npm install` で素通りする。
                        if is_command_wrapper(&cmd_name) {
                            if let Some(idx) = Self::find_wrapped_command_index(&cmd_name, &args) {
                                let inner = args[idx..].join(" ");
                                if !inner.is_empty() {
                                    command_strings.extend(self.extract_command_strings(&inner));
                                }
                            }
                        }

                        // shell -c / env -S / xargs / eval / find -exec など、後続をコマンド
                        // として再評価する形の内側コマンド文字列も抽出する。ディスパッチは
                        // コマンド名抽出経路（extract_reevaluated_inner_commands）と同一実装
                        // （reevaluated_inner_command_strings）を共有し、経路間の実装乖離に
                        // よる検出漏れ（fail-open）を防ぐ。
                        for inner in Self::reevaluated_inner_command_strings(&cmd_name, &args) {
                            if inner.raw_is_command_string {
                                command_strings.push(inner.text.clone());
                            }
                            command_strings.extend(self.extract_command_strings(&inner.text));
                        }
                    }
                }
                // コマンド置換のために子ノードに再帰
                for child in node.children(&mut node.walk()) {
                    self.extract_command_strings_from_node(
                        child,
                        source,
                        command_strings,
                        depth + 1,
                    );
                }
            }
            "subshell" | "command_substitution" => {
                for child in node.children(&mut node.walk()) {
                    self.extract_command_strings_from_node(
                        child,
                        source,
                        command_strings,
                        depth + 1,
                    );
                }
            }
            _ => {
                for child in node.children(&mut node.walk()) {
                    self.extract_command_strings_from_node(
                        child,
                        source,
                        command_strings,
                        depth + 1,
                    );
                }
            }
        }
    }

    /// extract_command_strings のフォールバックパーサー
    fn extract_command_strings_fallback(&self, command: &str) -> Vec<String> {
        // フォールバック経路も内側コマンドを再帰解析するため、再帰深さ上限を適用する。
        let Some(_guard) = RecursionGuard::enter() else {
            return Self::pathological_block_commands();
        };
        let mut command_strings = Vec::new();

        // セミコロンに加え、改行と単独 `&`（バックグラウンド実行）もトップレベル区切りとして扱う。
        // `;` のみで分割していた従来実装では、`echo ok\nrm -rf /tmp` や
        // `echo ok & rm -rf /tmp` を 1 セグメントにまとめてしまい、後続コマンドが
        // アンカー付きカスタム正規表現フィルタ（例 `^rm `）を素通りしていた。
        // extract_commands_fallback と同じ split_top_level_terminators を使って対称にする。
        for segment in Self::split_top_level_terminators(command) {
            for part in Self::split_by_logical_ops(segment) {
                for pipe_part in Self::split_respecting_quotes(part, '|') {
                    if !pipe_part.is_empty() {
                        command_strings
                            .extend(self.extract_command_strings_from_segment_fallback(pipe_part));
                    }
                }
            }
        }

        command_strings
    }

    /// 単一セグメントから完全なコマンド文字列を抽出する（フォールバック）。
    fn extract_command_strings_from_segment_fallback(&self, segment: &str) -> Vec<String> {
        let mut command_strings = Vec::new();
        let trimmed = segment.trim();

        if trimmed.is_empty() {
            return command_strings;
        }

        if let Some(inner) = Self::unwrap_subshell(trimmed) {
            return self.extract_command_strings_fallback(inner);
        }

        let tokens = parse_shell_tokens(trimmed);
        let Some((cmd_name, args)) = Self::parse_effective_command(&tokens) else {
            // ヘッダだけで本体コマンドを持たないセグメント（`for f in $(rm …)` の
            // ように parse_effective_command が None を返すケース）でも、ヘッダ内の
            // コマンド置換の中身は取りこぼさず解析する（fail-open 防止）。
            for nested in Self::extract_nested_command_fragments(trimmed) {
                command_strings.extend(self.extract_command_strings_fallback(&nested));
            }
            return command_strings;
        };

        let full_cmd = if args.is_empty() {
            cmd_name.clone()
        } else {
            format!("{} {}", cmd_name, args.join(" "))
        };
        command_strings.push(full_cmd);

        // 実行委譲ラッパー（sudo/env/setsid/flock/...）の内側コマンド文字列も抽出する。
        // アンカー付きカスタムフィルタが `sudo npm install` で素通りするのを防ぐ。
        if is_command_wrapper(&cmd_name) {
            if let Some(idx) = Self::find_wrapped_command_index(&cmd_name, &args) {
                let inner = args[idx..].join(" ");
                if !inner.is_empty() {
                    command_strings.extend(self.extract_command_strings_fallback(&inner));
                }
            }
        }

        // shell -c / env -S / xargs / eval / find -exec など、後続をコマンドとして
        // 再評価する形の内側コマンド文字列も抽出する。ディスパッチはコマンド名抽出経路
        // と同一実装（reevaluated_inner_command_strings）を共有し、経路間の実装乖離に
        // よる検出漏れ（fail-open）を防ぐ。
        for inner in Self::reevaluated_inner_command_strings(&cmd_name, &args) {
            if inner.raw_is_command_string {
                command_strings.push(inner.text.clone());
            }
            command_strings.extend(self.extract_command_strings_fallback(&inner.text));
        }

        // 引数内のコマンド置換を処理。
        for nested in Self::extract_nested_command_fragments(trimmed) {
            command_strings.extend(self.extract_command_strings_fallback(&nested));
        }

        command_strings
    }

    /// ASTノードを再帰的に走査してコマンドを抽出する
    #[cfg(feature = "ast-parser")]
    fn extract_commands_from_node(
        &mut self,
        node: Node,
        source: &str,
        commands: &mut Vec<String>,
        depth: usize,
    ) {
        // AST 走査の再帰がスタックを溢れさせないよう深さ上限を課す。超過時は
        // 解析を諦め安全側（危険コマンド候補）に倒して fail-closed を保つ。
        if depth > MAX_NODE_DEPTH {
            commands.extend(Self::pathological_block_commands());
            return;
        }
        match node.kind() {
            "command" | "simple_command" => {
                // command_name の子ノードを取得
                if let Some(cmd_name) = self.get_command_name(node, source) {
                    if !cmd_name.is_empty() {
                        commands.push(cmd_name.clone());
                    }

                    // 後続解析のため引数を取得
                    let args = self.get_command_arguments(node, source);

                    // ASTレベルでラッパーコマンドを展開（sudo, env, command など）
                    if is_command_wrapper(&cmd_name) {
                        self.process_wrapper_args(&cmd_name, &args, commands);
                    }

                    // shell -c / env -S / xargs / eval / find -exec など、後続をコマンドと
                    // して再評価する形の内側コマンドを抽出する。ラッパー配下
                    // （process_wrapper_args）と同じヘルパを共有し、両者の実装が乖離して
                    // 検出漏れ（fail-open）が生じるのを防ぐ。
                    self.extract_reevaluated_inner_commands(&cmd_name, &args, commands);
                }
                // 引数内のコマンド置換を拾うために子ノードも再帰的に探索する。
                // 例: echo $(yarn --version) から yarn を抽出する。
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands, depth + 1);
                }
            }
            "subshell" | "command_substitution" => {
                // サブシェル/コマンド置換の中身を再帰解析する。
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands, depth + 1);
                }
            }
            _ => {
                // 子ノードを再帰走査する。
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands, depth + 1);
                }
            }
        }
    }

    /// 再評価系コマンド（shell -c / env -S / xargs / eval / find -exec）が後続の引数を
    /// コマンドとして再評価する場合、その内側コマンド文字列を列挙する純粋ヘルパ。
    ///
    /// AST/fallback × コマンド名抽出/コマンド文字列抽出の全経路がこの単一ディスパッチを
    /// 共有し、再帰先（AST か fallback か、名前抽出か文字列抽出か）は呼び出し側が選ぶ。
    /// かつては経路ごとに同じ分岐が個別実装されており、片方にだけ再評価形式が欠けて
    /// 検出漏れ（fail-open）が生じた（例: ラッパー配下の env/xargs/eval/find 再評価漏れ）。
    /// 新しい再評価形式への対応はこの関数にのみ追加すればよい。
    /// 返却順に意味はない（全消費者は集合として扱う）。
    fn reevaluated_inner_command_strings(cmd_name: &str, args: &[String]) -> Vec<ReevaluatedInner> {
        let mut inners = Vec::new();
        // 判定キーは一度だけ計算して全分岐で共有する（command_key は String 割当を伴う）。
        let key = command_key(cmd_name);
        // shell -c "command"（bash/sh/... のほか su -c / runuser -c / flock -c も含む）
        if SHELL_COMMANDS.contains(&key.as_str()) {
            if let Some(shell_cmd) = Self::extract_shell_c_from_args(args) {
                inners.push(ReevaluatedInner {
                    text: shell_cmd,
                    raw_is_command_string: false,
                });
            }
        }
        match key.as_str() {
            // env -S / --split-string（分割文字列をコマンドとして再評価）
            "env" => {
                if let Some(env_cmd) = Self::extract_env_split_string_from_args(args) {
                    inners.push(ReevaluatedInner {
                        text: env_cmd,
                        raw_is_command_string: false,
                    });
                }
            }
            // xargs（標準入力の各要素に対して実行するコマンド）
            "xargs" => {
                if let Some(xargs_cmd) = Self::extract_xargs_command_string_from_args(args) {
                    inners.push(ReevaluatedInner {
                        text: xargs_cmd,
                        raw_is_command_string: true,
                    });
                }
            }
            // eval（引数をシェルとして再評価）
            "eval" => {
                if let Some(eval_cmd) = Self::join_eval_args(args) {
                    inners.push(ReevaluatedInner {
                        text: eval_cmd,
                        raw_is_command_string: false,
                    });
                }
            }
            // find -exec/-execdir（後続の引数をコマンドとして実行）
            "find" => {
                for exec_cmd in Self::extract_find_exec_commands(args) {
                    inners.push(ReevaluatedInner {
                        text: exec_cmd,
                        raw_is_command_string: false,
                    });
                }
            }
            _ => {}
        }
        inners
    }

    /// 再評価系コマンド（shell -c / env -S / xargs / eval / find -exec）の内側コマンドを
    /// 抽出する。これらは後続の引数をコマンドとして再評価するため、内側の実コマンドを
    /// 取りこぼすと危険コマンド検出が漏れる（fail-open）。
    ///
    /// トップレベル（extract_commands_from_node）とラッパー配下
    /// （process_wrapper_args）の双方から呼ぶ。かつては両者が同じロジックを個別に
    /// 実装しており、ラッパー側にだけ env/xargs/eval/find の再評価処理が欠けていたため、
    /// `sudo eval "rm -rf /"` などが素通り（fail-open）していた。ディスパッチ本体は
    /// `reevaluated_inner_command_strings` に単一実装され、コマンド文字列抽出経路とも
    /// 共有される。
    #[cfg(feature = "ast-parser")]
    fn extract_reevaluated_inner_commands(
        &mut self,
        cmd_name: &str,
        args: &[String],
        commands: &mut Vec<String>,
    ) {
        for inner in Self::reevaluated_inner_command_strings(cmd_name, args) {
            for nested in self.extract_commands(&inner.text) {
                Self::push_unique_command(commands, &nested);
            }
        }
    }

    /// AST ノードから引数を取得する（コマンド名自体は除外）。
    /// 内部処理用にクォートを除去する。
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments(&self, node: Node, source: &str) -> Vec<String> {
        self.get_command_arguments_impl(node, source, true)
    }

    /// パターンマッチ用途でクォートを保持したまま引数を取得する。
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments_raw(&self, node: Node, source: &str) -> Vec<String> {
        self.get_command_arguments_impl(node, source, false)
    }

    /// 実装本体: クォート除去有無を切り替えて引数を取得する。
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments_impl(
        &self,
        node: Node,
        source: &str,
        strip_quotes: bool,
    ) -> Vec<String> {
        let mut args = Vec::new();
        let mut found_command_name = false;

        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "command_name" => {
                    found_command_name = true;
                }
                // `number`（裸の整数）も引数に含める。これを欠落させると
                // `xargs -n 1 rm` / `sudo -u 1000 rm` / `nice -n 10 rm` のような
                // 「値を取るフラグ + 数値」で数値が脱落し、フラグが後続のコマンド
                // (rm 等) を値として消費して危険コマンド検出が漏れる。
                "word" | "string" | "raw_string" | "simple_expansion" | "expansion"
                | "concatenation" | "number"
                    if found_command_name =>
                {
                    let raw = &source[child.byte_range()];
                    let text = if strip_quotes {
                        Self::normalize_shell_word(raw)
                    } else {
                        raw.to_string()
                    };
                    args.push(text);
                }
                _ => {}
            }
        }

        args
    }

    /// shell -c 形式の引数から実行文字列を取り出す。
    fn extract_shell_c_from_args(args: &[String]) -> Option<String> {
        for (i, arg) in args.iter().enumerate() {
            let lower = arg.to_ascii_lowercase();
            if matches!(lower.as_str(), "/c" | "/k") && i + 1 < args.len() {
                return Some(args[i + 1..].join(" "));
            }

            // su / runuser / flock の long-form コマンド指定（--command / --session-command）。
            // `--command=script`（結合）と `--command script`（分離）の両形に対応する。
            // 短縮 -c しか見ないと `su --command "rm -rf /"` を取りこぼしフェイルオープンになる。
            if let Some(value) = arg
                .strip_prefix("--command=")
                .or_else(|| arg.strip_prefix("--session-command="))
            {
                return (!value.is_empty()).then(|| value.to_string());
            }
            if matches!(arg.as_str(), "--command" | "--session-command") {
                return args.get(i + 1).cloned();
            }

            let has_shell_c = arg == "-c"
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.chars().skip(1).any(|c| c == 'c'));
            if has_shell_c {
                // `bash -c -- 'script'` のように -c とスクリプトの間に `--`
                // （オプション終端）が挟まる場合は読み飛ばす。読み飛ばさないと
                // `--` をスクリプト本体とみなし、内側の rm 等を取りこぼす。
                let mut script_idx = i + 1;
                if args.get(script_idx).map(String::as_str) == Some("--") {
                    script_idx += 1;
                }
                if script_idx < args.len() {
                    return Some(args[script_idx].clone());
                }
            }
        }
        None
    }

    /// env の -S / --split-string の値（コマンド文字列）を取り出す。
    /// GNU/macOS の env は -S/--split-string で与えた文字列を分割して
    /// コマンドとして実行するため、内側のコマンドを再帰解析する必要がある。
    /// 対応形式: `-Scmd`(結合) / `-S cmd`(分離) / `--split-string=cmd` / `--split-string cmd`
    /// 注意: env 固有処理であり、他コマンドの -S（例: sort -S=メモリ指定）と
    /// 混同しないよう、呼び出し側で env のときのみ使用すること。
    fn extract_env_split_string_from_args(args: &[String]) -> Option<String> {
        for (i, arg) in args.iter().enumerate() {
            if let Some(value) = arg.strip_prefix("--split-string=") {
                return (!value.is_empty()).then(|| value.to_string());
            }
            if arg == "--split-string" {
                return args.get(i + 1).cloned();
            }
            if let Some(value) = arg.strip_prefix("-S") {
                if value.is_empty() {
                    // `-S cmd`（分離形式）: 次トークンが値
                    return args.get(i + 1).cloned();
                }
                // `-Scmd`（結合形式）: -S 直後が値
                return Some(value.to_string());
            }
        }
        None
    }

    /// xargs の引数から実行コマンド文字列を取り出す。
    fn extract_xargs_command_string_from_args(args: &[String]) -> Option<String> {
        Self::find_xargs_command_index(args).map(|index| args[index..].join(" "))
    }

    /// xargs のオプションを読み飛ばし、実行対象コマンドの位置を返す。
    fn find_xargs_command_index(args: &[String]) -> Option<usize> {
        let mut i = 0usize;
        while i < args.len() {
            let arg = &args[i];

            if arg == "--" {
                return (i + 1 < args.len()).then_some(i + 1);
            }

            if arg.starts_with('-') {
                if Self::xargs_flag_takes_arg(arg) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            return Some(i);
        }

        None
    }

    /// xargs で次トークンを値として消費するオプションか判定する。
    fn xargs_flag_takes_arg(flag: &str) -> bool {
        let (base_flag, has_inline_value) = match flag.split_once('=') {
            Some((base, _)) => (base, true),
            None => (flag, false),
        };

        if has_inline_value {
            return false;
        }

        if base_flag.starts_with("--") {
            return matches!(
                base_flag,
                "--arg-file"
                    | "--delimiter"
                    | "--eof"
                    | "--max-args"
                    | "--max-chars"
                    | "--max-lines"
                    | "--max-procs"
                    | "--process-slot-var"
                    | "--replace"
            );
        }

        matches!(
            base_flag,
            "-a" | "-d" | "-E" | "-I" | "-L" | "-n" | "-P" | "-s"
        )
    }

    /// eval の引数を再解析用のコマンド文字列へ戻す。
    fn join_eval_args(args: &[String]) -> Option<String> {
        (!args.is_empty()).then(|| args.join(" "))
    }

    /// find の -exec/-execdir 述語から実行コマンド文字列を取り出す。
    fn extract_find_exec_commands(args: &[String]) -> Vec<String> {
        let mut commands = Vec::new();
        let mut i = 0;

        while i < args.len() {
            if FIND_EXEC_PREDICATES.contains(&args[i].as_str()) {
                let start = i + 1;
                let mut end = start;
                while end < args.len() && !Self::is_find_exec_terminator(&args[end]) {
                    end += 1;
                }
                if start < end {
                    commands.push(args[start..end].join(" "));
                }
                i = end;
            }
            i += 1;
        }

        commands
    }

    /// find -exec の終端記号かどうかを判定する。
    fn is_find_exec_terminator(arg: &str) -> bool {
        matches!(arg, ";" | r"\;" | "+")
    }

    /// command ノードからコマンド名を取得する。
    #[cfg(feature = "ast-parser")]
    fn get_command_name(&self, node: Node, source: &str) -> Option<String> {
        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "command_name" => {
                    // command_name 全体を正規化して、`r\m` や `r''m` のような
                    // シェルの quote removal 後に実行されるコマンド名で判定する。
                    return Some(Self::normalize_shell_word(&source[child.byte_range()]));
                }
                "word" => {
                    // simple_command の先頭 word がコマンド名である場合に拾う。
                    let text = Self::normalize_shell_word(&source[child.byte_range()]);
                    if !text.starts_with('-') && !text.contains('=') {
                        return Some(text);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// シェルの quote removal に近い形で1トークンを正規化する。
    ///
    /// tree-sitter-bash は `r\m` や `r''m` の raw 表記を保持するため、
    /// 危険コマンド判定ではシェルが実際に実行するコマンド名へ寄せる必要がある。
    fn normalize_shell_word(word: &str) -> String {
        let mut result = String::with_capacity(word.len());
        let mut chars = word.chars().peekable();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let preserve_windows_separators = word.len() >= 3
            && word.as_bytes()[0].is_ascii_alphabetic()
            && word.as_bytes()[1] == b':'
            && word.as_bytes()[2] == b'\\';

        while let Some(c) = chars.next() {
            match c {
                '$' if !in_single_quote && !in_double_quote => {
                    if chars.peek() == Some(&'\'') {
                        // Bash の ANSI-C quoting: $'r\x6d' は quote removal 後に `rm`。
                        let _ = chars.next();
                        result.push_str(&Self::read_ansi_c_quoted(&mut chars));
                    } else if chars.peek() == Some(&'"') {
                        // Bash の $"..." はロケール翻訳文字列。未翻訳時は通常の二重引用と同じ。
                        // `$` 自体は quote removal 後のコマンド名に残らないため捨てる。
                    } else {
                        result.push(c);
                    }
                }
                '\\' if !in_single_quote => {
                    if let Some(next) = chars.next() {
                        if preserve_windows_separators && !in_double_quote {
                            result.push('\\');
                            result.push(next);
                            continue;
                        }
                        if in_double_quote && !matches!(next, '$' | '`' | '"' | '\\' | '\n' | '\r')
                        {
                            result.push('\\');
                        }
                        if next != '\n' && next != '\r' {
                            result.push(next);
                        }
                    } else {
                        result.push(c);
                    }
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                _ => result.push(c),
            }
        }

        result
    }

    /// Bash の ANSI-C quoted string (`$'...'`) をコマンド名比較用に展開する。
    fn read_ansi_c_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
        let mut result = String::new();

        while let Some(c) = chars.next() {
            match c {
                '\'' => break,
                '\\' => {
                    if let Some(decoded) = Self::read_ansi_c_escape(chars) {
                        result.push(decoded);
                    }
                }
                _ => result.push(c),
            }
        }

        result
    }

    /// ANSI-C quoted string 内のバックスラッシュエスケープを1文字読む。
    fn read_ansi_c_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
        let escaped = chars.next()?;
        match escaped {
            'a' => Some('\x07'),
            'b' => Some('\x08'),
            'e' | 'E' => Some('\x1b'),
            'f' => Some('\x0c'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\x0b'),
            '\\' => Some('\\'),
            '\'' => Some('\''),
            '"' => Some('"'),
            '\n' | '\r' => None,
            'x' => Self::read_radix_escape(chars, 2, 16),
            'u' => Self::read_radix_escape(chars, 4, 16),
            'U' => Self::read_radix_escape(chars, 8, 16),
            '0'..='7' => {
                let mut digits = String::from(escaped);
                digits.push_str(&Self::take_while_limited(chars, 2, |c| {
                    matches!(c, '0'..='7')
                }));
                u32::from_str_radix(&digits, 8)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ => Some(escaped),
        }
    }

    /// 最大桁数付きの基数エスケープを読む。
    fn read_radix_escape(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        max_digits: usize,
        radix: u32,
    ) -> Option<char> {
        let digits = Self::take_while_limited(chars, max_digits, |c| c.is_digit(radix));
        if digits.is_empty() {
            return None;
        }
        u32::from_str_radix(&digits, radix)
            .ok()
            .and_then(char::from_u32)
    }

    /// 条件に合う文字を最大 `limit` 文字まで取り出す。
    fn take_while_limited<F>(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        limit: usize,
        mut predicate: F,
    ) -> String
    where
        F: FnMut(char) -> bool,
    {
        let mut result = String::new();
        while result.len() < limit {
            let Some(&next) = chars.peek() else {
                break;
            };
            if !predicate(next) {
                break;
            }
            result.push(next);
            let _ = chars.next();
        }
        result
    }

    /// ラッパー引数を処理して実行対象コマンドを見つける
    /// 入れ子ラッパーも再帰的に処理する（例: sudo bash -c 'rm'）
    #[cfg(feature = "ast-parser")]
    fn process_wrapper_args(&mut self, wrapper: &str, args: &[String], commands: &mut Vec<String>) {
        // ラッパーが連鎖する場合（`sudo sudo ... rm`）でも、再帰ではなく反復で辿る。
        // 再帰にすると連鎖の深さに比例してスタックを消費し、長さ上限（65536）未満でも
        // `sudo` を数千個並べるだけでスタックオーバーフロー（SIGABRT＝fail-open）し得る。
        // 連鎖は線形なので反復で等価に処理でき、スタックは一定に保たれる。
        // 異常に深い連鎖（MAX_RECURSION_DEPTH 超過）は解析を諦め、安全側
        // （危険コマンド候補）に倒して fail-closed を保つ。
        let mut wrapper_name = wrapper.to_string();
        let mut rest: &[String] = args;
        for _ in 0..MAX_RECURSION_DEPTH {
            let Some(command_index) = Self::find_wrapped_command_index(&wrapper_name, rest) else {
                return;
            };
            let command_name = rest[command_index].clone();

            if !commands.contains(&command_name) {
                commands.push(command_name.clone());
            }

            // このコマンド以降の残り引数
            let remaining = &rest[command_index + 1..];

            // wrap されたコマンドが shell -c / env -S / xargs / eval / find -exec の
            // ように後続を再評価する形なら、内側の実コマンドも抽出する。
            // トップレベルと同じヘルパを共有し、`sudo eval "rm -rf /"` /
            // `sudo xargs rm` / `sudo find . -exec rm {} \;` などの検出漏れ
            // （fail-open）を防ぐ。
            self.extract_reevaluated_inner_commands(&command_name, remaining, commands);

            // 次のコマンドがラッパーでなければ終了。ラッパーなら反復で辿る。
            if !is_command_wrapper(&command_name) {
                return;
            }
            wrapper_name = command_name;
            rest = remaining;
        }
        // 異常に深いラッパー連鎖は解析を諦め fail-closed に倒す。
        commands.extend(Self::pathological_block_commands());
    }

    /// 指定ラッパーで「値を次トークンから取る」フラグかを判定する（テスト専用）。
    /// ラッパー名はパス・拡張子・大文字混在（`/usr/bin/sudo` 等）のままでよい。
    /// 本番経路は正規化済みキーを持つ呼び出し元が `wrapper_flag_takes_arg_key` を直接使う。
    #[cfg(test)]
    fn wrapper_flag_takes_arg(wrapper: &str, flag: &str) -> bool {
        Self::wrapper_flag_takes_arg_key(&command_key(wrapper), flag)
    }

    /// `wrapper_flag_takes_arg` の正規化済みキー版。
    /// `find_wrapped_command_index` のトークンループのような高頻度呼び出し元が、
    /// トークンごとの `command_key` 再計算（String 割当）を避けるために使う。
    fn wrapper_flag_takes_arg_key(wrapper_key: &str, flag: &str) -> bool {
        if !flag.starts_with('-') || flag == "-" || flag == "--" {
            return false;
        }

        let Some(spec) = wrapper_flag_spec(wrapper_key) else {
            return false;
        };

        let (base_flag, has_inline_value) = match flag.split_once('=') {
            Some((base, _)) => (base, true),
            None => (flag, false),
        };

        if base_flag.starts_with("--") {
            if has_inline_value {
                return false;
            }
            return spec.long.contains(&base_flag);
        }

        // 短縮フラグの cluster（例: `-nu`、`-uroot`）を解釈する。
        // - `-uroot`（先頭が値取得フラグで以降が値）→ 追加トークン不要
        // - `-nu`（cluster の末尾だけが値取得フラグ）→ 追加トークンが必要
        // - `-nv`（値取得フラグを含まない）→ 追加トークン不要
        if base_flag.len() > 2 {
            let cluster = &base_flag[1..];
            for (idx, ch) in cluster.char_indices() {
                let opt = format!("-{}", ch);
                if spec.short.contains(&opt.as_str()) {
                    // cluster 末尾が値取得フラグなら追加トークン必要、
                    // それ以前の位置にあれば残りが inline 値とみなされる。
                    let has_inline_value = idx + ch.len_utf8() < cluster.len();
                    return !has_inline_value;
                }
            }
            return false;
        }

        spec.short.contains(&base_flag)
    }

    /// GNU timeout の制限時間トークンかを判定する（例: 10, 0.5, 30s, 5m）。
    fn is_timeout_duration_token(token: &str) -> bool {
        if token.is_empty() {
            return false;
        }

        let trimmed = token.trim();
        let value = match trimmed.chars().last() {
            Some('s' | 'm' | 'h' | 'd') => &trimmed[..trimmed.len() - 1],
            _ => trimmed,
        };

        if value.is_empty() {
            return false;
        }

        value
            .parse::<f64>()
            .is_ok_and(|n| n.is_finite() && n >= 0.0)
    }

    /// ラッパー引数列から実際に実行されるコマンド位置を返す。
    fn find_wrapped_command_index(wrapper: &str, args: &[String]) -> Option<usize> {
        let mut i = 0usize;
        let mut timeout_duration_consumed = false;
        let mut taskset_mask_consumed = false;
        let wrapper_key = command_key(wrapper);
        let mut leading_positionals = Self::wrapper_leading_positionals(&wrapper_key);

        while i < args.len() {
            let arg = &args[i];

            if arg == "--" {
                // `--` はオプション解釈の打ち切りを意味するが、ラッパーが取る
                // leading positional（`su`/`gosu` のユーザ指定など）は `--` の
                // 後ろにも残る。これを消費してから実コマンド位置を返さないと、
                // ユーザ名を実行コマンドと誤認して後続の rm/kill/dd を見落とす
                // （例: `su -- root rm -rf /` で root をコマンド扱いしてしまう）。
                // chroot/flock は位置引数が `--` より前に来るため、ここに到達する
                // 時点で leading_positionals は通常 0 になっている。
                i += 1;
                while leading_positionals > 0 && i < args.len() {
                    leading_positionals -= 1;
                    i += 1;
                }
                return (i < args.len()).then_some(i);
            }

            if wrapper_key == "env" && Self::is_env_assignment_token(arg) {
                i += 1;
                continue;
            }

            // sudo は `sudo VAR=value command` 形式でコマンド直前の環境変数代入を受け付ける。
            // ここを実行対象コマンドとして扱うと、後続の rm/kill/dd を見落とす。
            if wrapper_key == "sudo" && Self::is_env_assignment_token(arg) {
                i += 1;
                continue;
            }

            if arg.starts_with('-') {
                // wrapper_key は正規化済みなので、トークンごとの再正規化を避ける key 版を使う。
                if Self::wrapper_flag_takes_arg_key(&wrapper_key, arg) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            if wrapper_key == "timeout"
                && !timeout_duration_consumed
                && Self::is_timeout_duration_token(arg)
            {
                timeout_duration_consumed = true;
                i += 1;
                continue;
            }

            // chroot <dir> cmd / flock <file> cmd: コマンドの前に位置引数（ディレクトリ／
            // ロックファイル）を取る。読み飛ばさないと dir/file を実行コマンドと誤認する。
            if leading_positionals > 0 {
                leading_positionals -= 1;
                i += 1;
                continue;
            }

            // taskset <mask> cmd: -c 不使用時は先頭に CPU マスク（16進/10進/カンマ列）を取る。
            if wrapper_key == "taskset"
                && !taskset_mask_consumed
                && Self::is_taskset_mask_token(arg)
            {
                taskset_mask_consumed = true;
                i += 1;
                continue;
            }

            return Some(i);
        }

        None
    }

    /// ラッパーがコマンド名の前に取る位置引数の数（オプションを除く）を返す。
    /// `chroot <dir> cmd` と `flock <file> cmd` は先頭に1つ位置引数を取る。
    /// `gosu <user[:group]> cmd` も先頭にユーザ指定（`root` や `app:app` 等）を取る。
    /// `su <user> cmd` 形式（Busybox/Alpine で広く使われる）も先頭にユーザ指定を取るため
    /// 読み飛ばさないと `su root rm -rf /` で `rm` を検出できず、特権昇格込みでバイパスされる。
    /// `runuser` は `-u USER` フラグでユーザを指定する形式が GNU 標準なので、leading
    /// positional には含めない（含めると `runuser -u user cmd` の `cmd` を消費してしまう）。
    fn wrapper_leading_positionals(wrapper_key: &str) -> usize {
        match wrapper_key {
            "chroot" | "flock" | "gosu" | "su" => 1,
            _ => 0,
        }
    }

    /// taskset の CPU マスクトークンらしさを判定する（16進 `0x1`・10進 `1`・カンマ列 `0,2`・
    /// 範囲 `0-3`）。コマンド名が数字始まりになることはまず無いため、先頭の1トークンのみ
    /// マスクとして読み飛ばす。判定を誤っても危険コマンドの検出漏れになるだけで過剰
    /// ブロックは生じない。
    fn is_taskset_mask_token(token: &str) -> bool {
        let body = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"));
        match body {
            Some(hex) => !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            None => {
                !token.is_empty()
                    && token.bytes().next().is_some_and(|b| b.is_ascii_digit())
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b',' || b == b'-')
            }
        }
    }

    /// シェル形式の環境変数代入（例: KEY=value）かを判定する。
    fn is_env_assignment_token(token: &str) -> bool {
        let Some((name, _value)) = token.split_once('=') else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first == '_' || first.is_ascii_alphabetic()) {
            return false;
        }
        chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    }

    /// コマンド位置で読み飛ばすべきシェル制御構文の前置キーワードか判定する。
    ///
    /// フォールバックパーサーは AST を持たないため、`then rm -rf /` や
    /// `do rm -rf /` のように制御構文キーワードに続く実コマンドを取りこぼすと
    /// fail-open になる。これらはコマンド名ではないので、セグメント先頭
    /// （コマンド位置）に現れた場合は読み飛ばして内側の実コマンドへ到達する。
    /// `echo then` のように引数位置に現れる同じ語は parse_effective_command が
    /// 先頭位置でしか判定しないため誤って読み飛ばさない。
    fn is_control_prefix_keyword(token: &str) -> bool {
        matches!(
            token,
            "if" | "then" | "elif" | "else" | "do" | "while" | "until" | "!"
        )
    }

    /// 制御構文の閉じ語か判定する。セグメントがこれらの語だけで構成される場合、
    /// 実行されるコマンドは存在しない（`fi` / `done` / `esac`）。
    fn is_control_closer_keyword(token: &str) -> bool {
        matches!(token, "fi" | "done" | "esac")
    }

    /// `for` / `select` ループのヘッダ `for VAR in LIST` を読み飛ばし、本体の
    /// 開始位置（`do` の直後）を返す。ループ変数 VAR とリスト LIST はコマンドでは
    /// ないため、同一セグメント内に `do` が無ければ None を返す。これにより
    /// `for rm in a` のループ変数 `rm` を誤ってコマンドとして検出しない
    /// （本体は `;` / 改行で分割された別セグメントの `do …` で捕捉される）。
    fn loop_body_start(tokens: &[String], index: usize) -> Option<usize> {
        let do_offset = tokens[index..].iter().position(|t| t == "do")?;
        Some(index + do_offset + 1)
    }

    /// `case WORD in` ヘッダを読み飛ばし、最初のパターンの位置（`in` の直後）を
    /// 返す。WORD はコマンドではないため読み飛ばす。`in` が無ければ None。
    /// パターン `pat)` 自体は parse_effective_command の `)` 終端スキップで処理する。
    fn case_header_end(tokens: &[String], index: usize) -> Option<usize> {
        let in_offset = tokens[index..].iter().position(|t| t == "in")?;
        Some(index + in_offset + 1)
    }

    /// トークン列から実際に実行されるコマンドを取り出す。
    /// 先頭の環境変数代入・ブレースグループ開き `{`・シェル制御構文を読み飛ばす。
    ///
    /// `{ rm -rf /; }` のコマンドグループの `{`、`if … then rm …` の `then`、
    /// `for … do rm …` の `do`、`case W in P) rm …` のヘッダ・パターンなどは
    /// コマンド名ではない。これらを読み飛ばさないと制御構文キーワードをコマンド
    /// 名と誤認し、内側の実コマンド（`rm` 等）を取りこぼして fail-open になる
    /// （AST パーサーは検出するが、フォールバックでも取りこぼさない）。
    /// グループ閉じ `}` / 制御構文の閉じ語（`fi`/`done`/`esac`）のみのトークンは
    /// コマンドを含まないため None を返す。
    fn parse_effective_command(tokens: &[String]) -> Option<(String, Vec<String>)> {
        let mut index = 0usize;
        loop {
            // 環境変数代入とブレースグループ開き `{` を読み飛ばす。
            while index < tokens.len()
                && (Self::is_env_assignment_token(&tokens[index]) || tokens[index] == "{")
            {
                index += 1;
            }
            let token = tokens.get(index)?.as_str();

            // グループ閉じ `}` / 制御構文の閉じ語はコマンドを含まない。
            if token == "}" || Self::is_control_closer_keyword(token) {
                return None;
            }
            // 前置キーワード（then/do/if/...）は読み飛ばして内側コマンドへ。
            if Self::is_control_prefix_keyword(token) {
                index += 1;
                continue;
            }
            // case 節パターン `pat)` / POSIX 関数定義 `name()` は `)` で終わる
            // トークンとして現れる。コマンド名ではないため読み飛ばす。
            if token.ends_with(')') {
                index += 1;
                continue;
            }
            match token {
                // for/select VAR in LIST: 変数・リストはコマンドではない。本体は
                // `do` の後（同一セグメントに無ければ別セグメントの `do …` で捕捉）。
                "for" | "select" => index = Self::loop_body_start(tokens, index)?,
                // case WORD in: WORD はコマンドではないため読み飛ばす。
                "case" => index = Self::case_header_end(tokens, index)?,
                _ => return Some((tokens[index].clone(), tokens[index + 1..].to_vec())),
            }
        }
    }

    /// セグメント全体が最上位の `( ... )` で包まれていれば中身を返す。
    fn unwrap_subshell(segment: &str) -> Option<&str> {
        let trimmed = segment.trim();
        if !(trimmed.starts_with('(') && trimmed.ends_with(')')) {
            return None;
        }

        let last_index = trimmed.char_indices().last()?.0;
        let mut depth = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;

        for (idx, ch) in trimmed.char_indices() {
            if escape {
                escape = false;
                continue;
            }

            if ch == '\\' && !in_single {
                escape = true;
                continue;
            }
            if ch == '\'' && !in_double {
                in_single = !in_single;
                continue;
            }
            if ch == '"' && !in_single {
                in_double = !in_double;
                continue;
            }
            if in_single || in_double {
                continue;
            }

            match ch {
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                    // 最外周の括弧は末尾で閉じている必要がある。
                    if depth == 0 && idx != last_index {
                        return None;
                    }
                }
                _ => {}
            }
        }

        if depth == 0 {
            Some(&trimmed[1..trimmed.len() - 1])
        } else {
            None
        }
    }

    /// コマンド置換（`$(...)`, `` `...` ``）およびプロセス置換（`<(...)`, `>(...)`）
    /// から内部断片を抽出する。
    ///
    /// プロセス置換 `diff <(rm -rf /) <(ls)` や `tee >(rm -rf /)` は内側のコマンドを
    /// 実際に実行するため、フォールバック経路でも取りこぼすと fail-open になる。
    /// `<(` / `>(` は `$(` と同じく括弧の対応を数えて中身を取り出す。なお直後が `(`
    /// のときのみ対象とするため、リダイレクト `> file` / `< file` とは区別される。
    fn extract_nested_command_fragments(segment: &str) -> Vec<String> {
        let chars: Vec<char> = segment.chars().collect();
        let len = chars.len();
        let mut fragments = Vec::new();
        let mut i = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;

        while i < len {
            let ch = chars[i];

            if escape {
                escape = false;
                i += 1;
                continue;
            }

            if ch == '\\' && !in_single {
                escape = true;
                i += 1;
                continue;
            }
            if ch == '\'' && !in_double {
                in_single = !in_single;
                i += 1;
                continue;
            }
            if ch == '"' && !in_single {
                in_double = !in_double;
                i += 1;
                continue;
            }

            // `$(...)` コマンド置換、`<(...)` / `>(...)` プロセス置換
            // （いずれも `(` の対応を数えて中身を取り出す。直後が `(` のときのみ
            // 対象とするため、リダイレクト `< file` / `> file` とは区別される）
            if !in_single
                && (ch == '$' || ch == '<' || ch == '>')
                && i + 1 < len
                && chars[i + 1] == '('
            {
                if let Some((end, inner)) = Self::extract_parenthesized_fragment(&chars, i + 1) {
                    if !inner.trim().is_empty() {
                        fragments.push(inner);
                    }
                    i = end + 1;
                    continue;
                }
            }

            // `` `...` `` 形式
            if !in_single && ch == '`' {
                if let Some((end, inner)) = Self::extract_backtick_fragment(&chars, i) {
                    if !inner.trim().is_empty() {
                        fragments.push(inner);
                    }
                    i = end + 1;
                    continue;
                }
            }

            i += 1;
        }

        fragments
    }

    /// 開き括弧に対応する閉じ括弧と、その内側のコマンド断片を返す。
    ///
    /// 引用符内とバックスラッシュでエスケープされた括弧は構造として数えない。
    /// 閉じ括弧が無い場合は `None` を返し、呼び出し側が残りの入力走査を継続する。
    fn extract_parenthesized_fragment(
        chars: &[char],
        open_index: usize,
    ) -> Option<(usize, String)> {
        debug_assert_eq!(chars.get(open_index), Some(&'('));

        let start = open_index + 1;
        let mut depth = 1usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;

        for index in start..chars.len() {
            let ch = chars[index];

            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' && !in_single {
                escape = true;
                continue;
            }
            if ch == '\'' && !in_double {
                in_single = !in_single;
                continue;
            }
            if ch == '"' && !in_single {
                in_double = !in_double;
                continue;
            }
            if in_single || in_double {
                continue;
            }

            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((index, chars[start..index].iter().collect()));
                    }
                }
                _ => {}
            }
        }

        None
    }

    /// バッククォートに対応する終端と、その内側のコマンド断片を返す。
    /// エスケープされたバッククォートは終端として扱わない。
    fn extract_backtick_fragment(chars: &[char], open_index: usize) -> Option<(usize, String)> {
        debug_assert_eq!(chars.get(open_index), Some(&'`'));

        let start = open_index + 1;
        let mut escape = false;
        for index in start..chars.len() {
            let ch = chars[index];
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if ch == '`' {
                return Some((index, chars[start..index].iter().collect()));
            }
        }

        None
    }

    /// 文字列処理ベースのフォールバックパーサー。
    fn extract_commands_fallback(&self, command: &str) -> Vec<String> {
        // フォールバック経路も内側コマンドを再帰解析するため、AST 経路と同じ
        // 再帰深さ上限を適用してスタックオーバーフロー（fail-open）を防ぐ。
        let Some(_guard) = RecursionGuard::enter() else {
            return Self::pathological_block_commands();
        };
        let mut commands = Vec::new();

        for segment in Self::split_top_level_terminators(command) {
            for part in Self::split_by_logical_ops(segment) {
                for pipe_part in Self::split_respecting_quotes(part, '|') {
                    if !pipe_part.is_empty() {
                        commands.extend(self.extract_commands_from_segment_fallback(pipe_part));
                    }
                }
            }
        }

        commands
    }

    /// 単一セグメントからコマンドを抽出する（フォールバック）。
    fn extract_commands_from_segment_fallback(&self, segment: &str) -> Vec<String> {
        let mut commands = Vec::new();
        let trimmed = segment.trim();

        if trimmed.is_empty() {
            return commands;
        }

        if let Some(inner) = Self::unwrap_subshell(trimmed) {
            return self.extract_commands_fallback(inner);
        }

        let tokens = parse_shell_tokens(trimmed);
        let Some((cmd, args)) = Self::parse_effective_command(&tokens) else {
            // ヘッダだけで本体コマンドを持たないセグメント（`for f in $(rm …)` の
            // ように parse_effective_command が None を返すケース）でも、ヘッダ内の
            // コマンド置換の中身は取りこぼさず解析する（fail-open 防止）。
            for nested in Self::extract_nested_command_fragments(trimmed) {
                commands.extend(self.extract_commands_fallback(&nested));
            }
            return commands;
        };

        commands.push(cmd.clone());

        // ラッパーコマンドを展開
        if is_command_wrapper(&cmd) {
            self.expand_wrapper_commands_fallback(&cmd, &args, &mut commands);
        }

        // shell -c / env -S / xargs / eval / find -exec など、後続をコマンドとして
        // 再評価する形の内側コマンドを抽出する。ラッパー展開
        // （expand_wrapper_commands_fallback）と同じヘルパを共有し、両者の実装が乖離して
        // 検出漏れ（fail-open）が生じるのを防ぐ。
        self.extract_reevaluated_inner_commands_fallback(&cmd, &args, &mut commands);

        // 引数中のコマンド置換を処理（例: echo $(rm -rf /tmp)）。
        for nested in Self::extract_nested_command_fragments(trimmed) {
            commands.extend(self.extract_commands_fallback(&nested));
        }

        commands
    }

    /// 再評価系コマンド（shell -c / env -S / xargs / eval / find -exec）の内側コマンドを
    /// 抽出する（フォールバック経路）。AST 経路の extract_reevaluated_inner_commands と
    /// 対になり、トップレベル（extract_commands_from_segment_fallback）とラッパー展開
    /// （expand_wrapper_commands_fallback）の双方から呼ぶ。ディスパッチ本体は
    /// `reevaluated_inner_command_strings` に単一実装され、AST 経路・コマンド文字列
    /// 抽出経路とも共有される。
    fn extract_reevaluated_inner_commands_fallback(
        &self,
        cmd_name: &str,
        args: &[String],
        commands: &mut Vec<String>,
    ) {
        for inner in Self::reevaluated_inner_command_strings(cmd_name, args) {
            commands.extend(self.extract_commands_fallback(&inner.text));
        }
    }

    /// フォールバックパーサーでラッパーコマンドの実行対象を展開する。
    /// `sudo -u root bash -c 'rm -rf /'` のように wrapper → shell -c が
    /// 連続する場合でも、シェルに渡される内側のコマンドを落とさない。
    fn expand_wrapper_commands_fallback(
        &self,
        wrapper: &str,
        args: &[String],
        commands: &mut Vec<String>,
    ) {
        // process_wrapper_args と同様、ラッパー連鎖は再帰ではなく反復で辿り、
        // 深いラッパー連鎖（`sudo sudo ... rm`）でのスタックオーバーフロー
        // （fail-open）を防ぐ。MAX_RECURSION_DEPTH 超過時は安全側に倒す。
        let mut wrapper_name = wrapper.to_string();
        let mut rest: &[String] = args;
        for _ in 0..MAX_RECURSION_DEPTH {
            let Some(command_index) = Self::find_wrapped_command_index(&wrapper_name, rest) else {
                return;
            };

            let command_name = rest[command_index].clone();
            Self::push_unique_command(commands, &command_name);

            let remaining = &rest[command_index + 1..];

            // wrap されたコマンドが shell -c / env -S / xargs / eval / find -exec の
            // ように後続を再評価する形なら、内側の実コマンドも抽出する。トップレベルと
            // 同じヘルパを共有する。tail を join して丸ごと再解析する方式は、
            // `sudo echo "; rm -rf /"` のようにクォート内の区切り文字を誤って
            // 再トークン化し誤検出（false positive）するため採用しない。
            self.extract_reevaluated_inner_commands_fallback(&command_name, remaining, commands);

            if !is_command_wrapper(&command_name) {
                return;
            }
            wrapper_name = command_name;
            rest = remaining;
        }
        commands.extend(Self::pathological_block_commands());
    }

    /// 同じコマンド名を重複して追加しない。
    fn push_unique_command(commands: &mut Vec<String>, command: &str) {
        if !commands.iter().any(|existing| existing == command) {
            commands.push(command.to_string());
        }
    }

    /// トップレベルのコマンド終端子で分割する。
    /// `;`、改行 (`\n`)、単独の `&` (バックグラウンド実行) を区切りとして扱う。
    /// 引用符内、サブシェル `(...)` の内側、エスケープされた文字は分割しない。
    /// `&&` や `||` はここでは分割せず、後続の split_by_logical_ops に委ねる。
    fn split_top_level_terminators(s: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current_start = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;
        let mut paren_depth = 0usize;
        let mut chars = s.char_indices().peekable();

        while let Some((idx, c)) = chars.next() {
            if escape_next {
                escape_next = false;
                continue;
            }

            match c {
                '\\' if !in_single_quote => {
                    escape_next = true;
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                '(' if !in_single_quote && !in_double_quote => {
                    paren_depth += 1;
                }
                ')' if !in_single_quote && !in_double_quote => {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                ';' | '\n' if !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    let part = &s[current_start..idx];
                    if !part.trim().is_empty() {
                        result.push(part.trim());
                    }
                    current_start = idx + c.len_utf8();
                }
                '&' if !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    let next_is_amp = chars.peek().is_some_and(|(_, nc)| *nc == '&');
                    if next_is_amp {
                        // `&&` (論理 AND) は後続の split_by_logical_ops に委ねる。
                        // ここで2文字目の `&` を消費しておかないと、次のループ反復で
                        // 単独 `&` として誤って分割されてしまう。
                        let _ = chars.next();
                    } else {
                        // 単独 `&` (バックグラウンド実行) はここで分割する。
                        let part = &s[current_start..idx];
                        if !part.trim().is_empty() {
                            result.push(part.trim());
                        }
                        current_start = idx + c.len_utf8();
                    }
                }
                _ => {}
            }
        }

        let remaining = &s[current_start..];
        if !remaining.trim().is_empty() {
            result.push(remaining.trim());
        }

        result
    }

    /// クォート（`'` `"`）と括弧 `()` を考慮して、単一文字 `sep` で分割する。
    /// クォート/括弧の内側の `sep` は区切りとして扱わない。
    /// `|` をシェルの構造を保ったまま分割するために使う。
    fn split_respecting_quotes(s: &str, sep: char) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current_start = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;
        let mut paren_depth = 0usize;

        for (idx, c) in s.char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }
            match c {
                '\\' if !in_single_quote => {
                    escape_next = true;
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                '(' if !in_single_quote && !in_double_quote => {
                    paren_depth += 1;
                }
                ')' if !in_single_quote && !in_double_quote => {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                ch if ch == sep && !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    let part = &s[current_start..idx];
                    if !part.trim().is_empty() {
                        result.push(part.trim());
                    }
                    current_start = idx + ch.len_utf8();
                }
                _ => {}
            }
        }

        let remaining = &s[current_start..];
        if !remaining.trim().is_empty() {
            result.push(remaining.trim());
        }

        result
    }

    /// `&&` と `||` で分割する。
    fn split_by_logical_ops(s: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current_start = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;
        let mut paren_depth = 0usize;
        let mut chars = s.char_indices().peekable();

        while let Some((idx, c)) = chars.next() {
            if escape_next {
                escape_next = false;
                continue;
            }

            match c {
                '\\' if !in_single_quote => {
                    escape_next = true;
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                '(' if !in_single_quote && !in_double_quote => {
                    paren_depth += 1;
                }
                ')' if !in_single_quote && !in_double_quote => {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                '&' | '|' if !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    if let Some(&(next_idx, next_c)) = chars.peek() {
                        if next_c == c {
                            let part = &s[current_start..idx];
                            if !part.trim().is_empty() {
                                result.push(part.trim());
                            }
                            // 2文字目の演算子も消費し、次の開始位置を更新する。
                            let _ = chars.next();
                            current_start = next_idx + next_c.len_utf8();
                        }
                    }
                }
                _ => {}
            }
        }

        let remaining = &s[current_start..];
        if !remaining.trim().is_empty() {
            result.push(remaining.trim());
        }

        result
    }

    /// コマンドと引数を抽出する（テスト専用）。
    /// 本番経路では使われないため、テストビルドでのみコンパイルする。
    #[cfg(test)]
    fn extract_command_with_args(&self, command: &str) -> (String, Vec<String>) {
        let mut parts = parse_shell_tokens(command);
        if parts.is_empty() {
            return (String::new(), Vec::new());
        }

        let cmd = parts.remove(0);
        (cmd, parts)
    }
}

impl Default for ShellParser {
    fn default() -> Self {
        Self::new()
    }
}

/// シェルのクォート規則を考慮してコマンド文字列をトークン化する。
/// `ShellParser` を生成せずに使える独立関数。
///
/// # 使用例
/// ```
/// let tokens = parse_shell_tokens("echo 'hello world'");
/// assert_eq!(tokens, vec!["echo", "hello world"]);
/// ```
pub fn parse_shell_tokens(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for c in command.trim().chars() {
        if escape_next {
            // 分割判定ではエスケープを考慮しつつ、quote removal は後段に任せる。
            current.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if !in_single_quote => {
                current.push(c);
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                current.push(c);
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                current.push(c);
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' | '\n' | '\r' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    parts.push(ShellParser::normalize_shell_word(&current));
                    current.clear();
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        parts.push(ShellParser::normalize_shell_word(&current));
    }

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple_command() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("ls -la");
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_extract_piped_commands() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cat file.txt | grep error | wc -l");
        assert!(commands.contains(&"cat".to_string()));
        assert!(commands.contains(&"grep".to_string()));
        assert!(commands.contains(&"wc".to_string()));
    }

    #[test]
    fn test_extract_logical_ops() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("mkdir -p dir && cd dir && ls");
        assert!(commands.contains(&"mkdir".to_string()));
        assert!(commands.contains(&"cd".to_string()));
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_extract_semicolon() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo hello; echo world");
        assert!(commands.iter().filter(|c| *c == "echo").count() >= 2);
    }

    #[test]
    fn test_extract_background_amp_separator() {
        // 単独の `&` (バックグラウンド実行) でもコマンドが分割されること。
        // 後続コマンドが見落とされると危険コマンドブロックが回避される。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo ok & rm -rf /tmp/dummy");
        assert!(commands.contains(&"echo".to_string()));
        assert!(
            commands.contains(&"rm".to_string()),
            "single `&` should split commands; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_newline_separator() {
        // 改行で区切られた複数コマンドも個別に抽出できること。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo ok\nrm -rf /tmp/dummy");
        assert!(commands.contains(&"echo".to_string()));
        assert!(
            commands.contains(&"rm".to_string()),
            "newline should split commands; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_amp_inside_quotes_not_split() {
        // 引用符内の `&` はコマンド分離子として扱わない。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo 'a & b' && ls");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"ls".to_string()));
        assert!(!commands.contains(&"b".to_string()));
    }

    #[test]
    fn test_extract_backslash_escaped_command_name() {
        // シェルは `r\m` を quote removal 後に `rm` として実行する。
        // raw 表記のままだと危険コマンドブロックを回避できてしまう。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"r\m -rf /tmp/dummy");
        assert!(
            commands.contains(&"rm".to_string()),
            "escaped command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_quoted_concatenated_command_name() {
        // シェルでは `r''m` も quote removal 後に `rm` として実行される。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("r''m -rf /tmp/dummy");
        assert!(
            commands.contains(&"rm".to_string()),
            "quoted command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_ansi_c_quoted_command_name() {
        // Bash の $'...' 形式も quote removal 後のコマンド名で判定する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"$'r\x6d' -rf /tmp/dummy");
        assert!(
            commands.contains(&"rm".to_string()),
            "ANSI-C quoted command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_locale_quoted_command_name() {
        // Bash の $"..." は未翻訳なら通常の二重引用と同じコマンド名になる。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r#"$"rm" -rf /tmp/dummy"#);
        assert!(
            commands.contains(&"rm".to_string()),
            "locale quoted command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_split_top_level_terminators_basic() {
        // セミコロン、改行、単独 `&` で分割される。
        let result = ShellParser::split_top_level_terminators("a; b\nc & d");
        assert_eq!(result, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_split_top_level_terminators_double_amp_preserved() {
        // `&&` は分割しない（後続の split_by_logical_ops に委ねる）。
        let result = ShellParser::split_top_level_terminators("a && b");
        assert_eq!(result, vec!["a && b"]);
    }

    #[test]
    fn test_split_top_level_terminators_quoted_separators_kept() {
        // 引用符内・サブシェル内の分離子は無視する。
        let result = ShellParser::split_top_level_terminators("echo 'a;b'");
        assert_eq!(result, vec!["echo 'a;b'"]);
        let result = ShellParser::split_top_level_terminators("echo \"a;b\"");
        assert_eq!(result, vec!["echo \"a;b\""]);
        let result = ShellParser::split_top_level_terminators("(a; b); c");
        assert_eq!(result, vec!["(a; b)", "c"]);
    }

    #[test]
    fn test_extract_command_with_args() {
        let parser = ShellParser::new();
        let (cmd, args) = parser.extract_command_with_args("git commit -m \"Hello world\"");
        assert_eq!(cmd, "git");
        assert_eq!(args, vec!["commit", "-m", "Hello world"]);
    }

    #[test]
    fn test_extract_command_with_single_quotes() {
        let parser = ShellParser::new();
        let (cmd, args) = parser.extract_command_with_args("echo 'hello world'");
        assert_eq!(cmd, "echo");
        assert_eq!(args, vec!["hello world"]);
    }

    #[test]
    fn test_extract_commands_with_env_assignment_prefix() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("NODE_ENV=prod npm install");
        assert!(commands.contains(&"npm".to_string()));
    }

    #[test]
    fn test_command_key_basename() {
        // basename 抽出: /bin/rm → rm
        assert_eq!(command_key("/bin/rm"), "rm");
        assert_eq!(command_key("/usr/bin/rm"), "rm");
        assert_eq!(command_key("./rm"), "rm");
    }

    #[test]
    fn test_command_key_windows_path() {
        // Windows パス区切り `\` も basename として扱う
        assert_eq!(command_key("C:\\Windows\\rm.exe"), "rm");
    }

    #[test]
    fn test_command_key_extension_strip() {
        // 実行ファイル拡張子の除去
        assert_eq!(command_key("rm.exe"), "rm");
        assert_eq!(command_key("rm.CMD"), "rm");
        assert_eq!(command_key("rm.Bat"), "rm");
        assert_eq!(command_key("rm.com"), "rm");
    }

    #[test]
    fn test_command_key_lowercase() {
        // 大文字小文字を正規化
        assert_eq!(command_key("DEL"), "del");
        assert_eq!(command_key("Rm"), "rm");
        assert_eq!(command_key("TASKKILL.EXE"), "taskkill");
    }

    #[test]
    fn test_command_key_already_normalized() {
        // 既に正規化されている文字列はそのまま
        assert_eq!(command_key("rm"), "rm");
        assert_eq!(command_key("bash"), "bash");
    }

    #[test]
    fn test_command_key_brace_expansion() {
        // コマンド名位置のブレース展開は最初の選択肢を採用して正規化する。
        assert_eq!(command_key("{rm,-rf,/tmp/foo}"), "rm");
        assert_eq!(command_key("{kill,-9,1}"), "kill");
        assert_eq!(command_key("{dd,if=/dev/zero}"), "dd");
        assert_eq!(command_key("/bin/{rm,ls}"), "rm");
        assert_eq!(command_key("{r,}m"), "rm"); // `{r,}m` → 最初の選択肢 `r` + `m`
        assert_eq!(command_key("{r{m,d},ls}"), "rm"); // ネスト
        // ブレース展開でない `{` はそのまま（${VAR} やカンマ無し群）。
        assert_eq!(command_key("${VAR}"), "${var}");
        assert_eq!(command_key("git"), "git");
    }

    #[test]
    fn test_expand_braces_first_choice() {
        assert_eq!(expand_braces_first_choice("{rm,ls}"), "rm");
        assert_eq!(expand_braces_first_choice("pre{a,b}post"), "preapost");
        assert_eq!(expand_braces_first_choice("/bin/{rm,ls}"), "/bin/rm");
        assert_eq!(expand_braces_first_choice("{r{m,d},ls}"), "rm");
        // bash は最初の「非空」選択肢を起動コマンドにする（`{,rm}` → `rm`）。
        assert_eq!(expand_braces_first_choice("{,rm}"), "rm");
        assert_eq!(expand_braces_first_choice("{,,rm}"), "rm");
        assert_eq!(expand_braces_first_choice("{ls,rm}"), "ls");
        assert_eq!(expand_braces_first_choice("{,}"), "");
        // カンマの無い群・パラメータ展開は変更しない。
        assert_eq!(expand_braces_first_choice("${HOME}"), "${HOME}");
        assert_eq!(expand_braces_first_choice("{ cmd; }"), "{ cmd; }");
        assert_eq!(expand_braces_first_choice("plain"), "plain");
    }

    #[test]
    fn test_is_shell_command_recognizes_cmd_exe() {
        // cmd.exe を cmd と認識する
        assert!(is_shell_command("cmd.exe"));
        assert!(is_shell_command("cmd"));
        assert!(is_shell_command("CMD.EXE"));
    }

    #[test]
    fn test_is_shell_command_recognizes_path_prefixed_bash() {
        // /bin/bash も bash として認識する
        assert!(is_shell_command("/bin/bash"));
        assert!(is_shell_command("/usr/bin/sh"));
    }

    #[test]
    fn test_is_command_wrapper_recognizes_path_prefixed() {
        // /usr/bin/sudo も sudo として認識する
        assert!(is_command_wrapper("/usr/bin/sudo"));
        assert!(is_command_wrapper("sudo"));
    }

    #[test]
    fn test_extract_commands_absolute_path_rm() {
        // /bin/rm の絶対パス指定でもコマンドが抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("/bin/rm -rf /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_cmd_exe_del() {
        // cmd.exe /c del を経由しても del が抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cmd.exe /c del C:\\tmp\\file.txt");
        assert!(commands.iter().any(|c| command_key(c) == "del"));
    }

    #[test]
    fn test_extract_commands_brace_expansion_bypass() {
        // ブレース展開 `{rm,...}` 経由でも rm が抽出される（#1 バイパス対策）。
        let mut parser = ShellParser::new();
        for input in [
            "{rm,-rf,/tmp/x}",
            "/bin/{rm,ls} -rf /tmp/x",
            "{r,}m -rf /tmp/x",
            "{r{m,d},ls} -rf /tmp/x",
            "echo hi; {rm,-rf,/tmp/x}",
            "{,rm} -rf /tmp/x",
            "{,}rm -rf /tmp/x",
        ] {
            let commands = parser.extract_commands(input);
            assert!(
                commands.iter().any(|c| command_key(c) == "rm"),
                "rm should be detected in: {input} -> {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_brace_first_choice_not_overblocked() {
        // `{ls,rm}` は bash で先頭 `ls` が起動コマンドになり rm は引数。過剰ブロックしない。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("{ls,rm} -rf /tmp/x");
        assert!(!commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_pathological_deep_brace_nesting_fails_closed() {
        // 深いブレースグループのネストは病的入力として安全側（rm/kill/dd 候補）に倒す（#2）。
        // 括弧を使わないため、`{`/`}` を計数しない実装ではスタックオーバーフロー→fail-open
        // し得たケース。
        let payload = format!("{}rm -rf /{}", "{ ".repeat(2000), " ;}".repeat(2000));
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(&payload);
        assert!(
            commands
                .iter()
                .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd")),
            "deep nesting must fail closed"
        );
    }

    #[test]
    fn test_extract_commands_execution_wrapper_bypasses() {
        // 実行委譲ラッパー / シェル委譲経由でも内側の rm が抽出される（#3 バイパス対策）。
        let mut parser = ShellParser::new();
        for input in [
            "setsid rm -rf /tmp/x",
            "flock /tmp/lock rm -rf /tmp/x",
            "flock -e /tmp/lock rm -rf /tmp/x",
            "flock /tmp/lock -c \"rm -rf /tmp/x\"",
            "stdbuf -oL rm -rf /tmp/x",
            "taskset 1 rm -rf /tmp/x",
            "taskset 0x3 rm -rf /tmp/x",
            "taskset -c 0 rm -rf /tmp/x",
            "su -c \"rm -rf /tmp/x\"",
            "su root -c \"rm -rf /tmp/x\"",
            // Busybox/Alpine 形式の `su <user> cmd` でも内側の rm を検出する
            // （leading positional 1 でユーザを skip して cmd を抽出）。
            "su root rm -rf /tmp/x",
            "su -l root rm -rf /tmp/x",
            "su -s /bin/sh root rm -rf /tmp/x",
            "runuser -u user rm -rf /tmp/x",
            "runuser -c \"rm -rf /tmp/x\"",
            "watch -n 1 rm -rf /tmp/x",
            "busybox rm -rf /tmp/x",
            "chroot /jail rm -rf /tmp/x",
            "nsenter -t 123 -m rm -rf /tmp/x",
            "unshare rm -rf /tmp/x",
            // 値取得フラグ（long/short）の後ろに実コマンドが続くケース（codex 指摘の
            // フェイルオープン回帰対策）。フラグ値を実コマンドと誤認しないこと。
            "watch -d rm -rf /tmp/x",
            "stdbuf --output L rm -rf /tmp/x",
            "unshare -R /jail rm -rf /tmp/x",
            "unshare --root /jail rm -rf /tmp/x",
            "nsenter --wd /tmp rm -rf /tmp/x",
            "flock -w 1 /tmp/lock rm -rf /tmp/x",
            "time -o /tmp/t rm -rf /tmp/x",
            "strace -o /tmp/trace rm -rf /tmp/x",
            // su / runuser / flock の long-form コマンド指定。
            "su --command \"rm -rf /tmp/x\" root",
            "su --command=\"rm -rf /tmp/x\" root",
            "runuser --session-command \"rm -rf /tmp/x\" user",
            "flock --command \"rm -rf /tmp/x\" /tmp/lock",
        ] {
            let commands = parser.extract_commands(input);
            assert!(
                commands.iter().any(|c| command_key(c) == "rm"),
                "rm should be detected in: {input} -> {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_wrapper_reevaluation_bypasses() {
        // ラッパー（sudo/timeout/doas 等）配下に再評価系コマンド（eval/xargs/
        // find -exec/env -S）が来ても内側の危険コマンドを検出する。
        // これらは以前、ラッパー展開が shell -c しか再評価しなかったため
        // 素通り（fail-open）していた（#wrapper-reeval 対策）。
        let mut parser = ShellParser::new();
        for input in [
            "sudo eval 'rm -rf /tmp/x'",
            "sudo xargs rm",
            r"sudo find . -exec rm {} \;",
            "sudo env -S 'rm -rf /tmp/x'",
            "timeout 10 eval 'rm -rf /tmp/x'",
            "timeout 10 xargs rm",
            "nice -n 10 find . -exec rm {} +",
            "command eval 'rm -rf /tmp/x'",
            // 値取得フラグを挟んでも wrap されたコマンドを正しく特定して再評価する。
            "sudo -u root eval 'rm -rf /tmp/x'",
            // xargs の -I（置換）や find の -execdir など別の述語形式でも検出する。
            "sudo xargs -I {} rm {}",
            r"sudo find . -execdir rm {} \;",
            // env -S の中身がさらに eval 等でも再帰的に解析する。
            "env -S 'eval rm -rf /tmp/x'",
            // 連鎖ラッパーの終端が再評価系でも取りこぼさない。
            "sudo sudo eval 'rm -rf /tmp/x'",
            "busybox xargs rm",
            "doas find . -exec kill -9 {} +",
        ] {
            let commands = parser.extract_commands(input);
            assert!(
                commands
                    .iter()
                    .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd")),
                "dangerous command should be detected in: {input} -> {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_wrapper_reevaluation_no_false_positive() {
        // ラッパー配下の非再評価系コマンド（echo/printf 等）の引数に危険コマンド名を
        // 含む文字列があっても、それはコマンド位置ではないので検出しない。
        // tail を join して丸ごと再解析する方式ではクォート内の `;` を再トークン化して
        // `rm` を誤検出（false positive / 過剰ブロック）してしまうため、
        // 再評価系ヘルパを直接適用する方式で回避している。
        let mut parser = ShellParser::new();
        for input in [
            "sudo echo '; rm -rf /tmp/x'",
            "sudo echo 'rm -rf /tmp/x'",
            "timeout 10 echo '; rm -rf /'",
            "sudo printf 'rm %s' /tmp/x",
            // rm が find の -name の値や grep の検索語として現れてもコマンド位置ではない。
            "sudo find . -name rm",
            "sudo grep -r rm /etc",
        ] {
            let commands = parser.extract_commands(input);
            assert!(
                !commands.iter().any(|c| command_key(c) == "rm"),
                "rm must NOT be detected (it is a quoted argument) in: {input} -> {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_command_strings_expands_wrappers() {
        // カスタムフィルタ用の文字列抽出経路でもラッパーの内側コマンドを展開する
        // （`^npm install` のようなアンカー付きパターンが `sudo npm install` で
        // 素通りしないこと）。
        let mut parser = ShellParser::new();
        for (input, needle) in [
            ("sudo npm install", "npm install"),
            ("setsid npm run build", "npm run build"),
            ("flock /tmp/lock npm test", "npm test"),
        ] {
            let strings = parser.extract_command_strings(input);
            assert!(
                strings.iter().any(|s| s.contains(needle)),
                "{needle:?} should be extracted from {input:?} -> {strings:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_wrapper_positional_not_overblocked() {
        // 正常なラッパー使用（ディレクトリ／ファイル／マスクが先頭）は過剰ブロックしない。
        let mut parser = ShellParser::new();
        for input in [
            "flock /tmp/lock echo done",
            "chroot /jail ls",
            "taskset -c 0 ls",
            "setsid mycmd --flag",
        ] {
            let commands = parser.extract_commands(input);
            assert!(
                !commands
                    .iter()
                    .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd")),
                "must not over-block: {input} -> {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_windows_path_command() {
        // Windows ドライブパスの `\` はコマンド名ではパス区切りとして扱う
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("C:\\Windows\\rm.exe C:\\tmp\\file.txt");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_sudo_nu_cluster() {
        // sudo -nu root rm の cluster short option でも rm が抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -nu root rm -rf /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_path_prefixed_sudo_with_value_option() {
        // /usr/bin/sudo のようにラッパーがパス付きでも -u の値を読み飛ばし rm を抽出する
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("/usr/bin/sudo -u root rm -rf /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_path_prefixed_timeout_with_value_option() {
        // /usr/bin/timeout のようにラッパーがパス付きでも -s と duration を読み飛ばす
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("/usr/bin/timeout -s TERM 10 rm -f /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_timeout_vs_cluster() {
        // timeout -vs TERM 10 rm の cluster short option でも rm が抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("timeout -vs TERM 10 rm -f /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_command_strings_with_bash_c_subshell() {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings("bash -c 'npm install'");
        assert!(command_strings.iter().any(|s| s == "npm install"));
    }

    #[test]
    fn test_extract_command_strings_from_command_substitution() {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings("echo $(npm install)");
        assert!(command_strings.iter().any(|s| s == "npm install"));
    }

    #[test]
    fn test_extract_command_strings_from_eval() {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings("eval 'npm install'");
        assert!(
            command_strings.iter().any(|s| s == "npm install"),
            "eval の内側のコマンド文字列を抽出すべき: {:?}",
            command_strings
        );
    }

    #[test]
    fn test_extract_command_strings_from_find_exec() {
        let mut parser = ShellParser::new();
        let command_strings =
            parser.extract_command_strings(r"find . -name package.json -exec npm install \;");
        assert!(
            command_strings.iter().any(|s| s == "npm install"),
            "find -exec の実行コマンド文字列を抽出すべき: {:?}",
            command_strings
        );
    }

    // === ラッパー・サブシェル検出テスト ===

    #[test]
    fn test_extract_sudo_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo rm -rf /tmp/test");
        assert!(commands.contains(&"sudo".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sudo_with_flags() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -u root rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sudo_with_long_user_flag() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo --user root rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sudo_with_non_interactive_flag() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -n rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_timeout_with_long_signal_option() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("timeout --signal TERM 10 rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_command_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("command rm -rf /tmp/test");
        assert!(commands.contains(&"command".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_exec_wrapper() {
        // exec は現在のシェルを指定コマンドで置き換えて実行する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("exec rm -rf /tmp/test");
        assert!(commands.contains(&"exec".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_env_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("env PATH=/usr/bin rm file.txt");
        assert!(commands.contains(&"env".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_bash_c_subshell() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -c 'rm -rf /tmp/test'");
        assert!(commands.contains(&"bash".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_bash_combined_c_flag() {
        // bash -lc のように -c が他フラグと結合されても内側のコマンドを抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -lc 'rm -rf /tmp/test'");
        assert!(commands.contains(&"bash".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sh_c_subshell() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sh -c \"kill -9 1234\"");
        assert!(commands.contains(&"sh".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_cmd_c_shell() {
        // Windows の cmd /c 経由で del が実行される場合も検出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cmd /c del C:\\tmp\\file.txt");
        assert!(commands.contains(&"cmd".to_string()));
        assert!(commands.contains(&"del".to_string()));
    }

    #[test]
    fn test_extract_pkexec_wraps_rm() {
        // Polkit の pkexec は sudo と同じく後続コマンドを昇格権限で実行する。
        // ラッパー認識リストから漏れると `pkexec rm -rf /` が root バイパスになるため、
        // 必ず内側 rm を抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("pkexec rm -rf /tmp/foo");
        assert!(
            commands.contains(&"rm".to_string()),
            "pkexec ラッパーで rm が検出できていない: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_gosu_wraps_rm() {
        // gosu はコンテナ向けの sudo 代替で `gosu <user> <cmd>` 形式。
        // ラッパー認識リストから漏れると `gosu root rm -rf /` が root バイパスになる。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("gosu root rm -rf /tmp/foo");
        assert!(
            commands.contains(&"rm".to_string()),
            "gosu ラッパーで rm が検出できていない: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_xargs_command() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("find . -name '*.tmp' | xargs rm");
        assert!(commands.contains(&"find".to_string()));
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_xargs_with_flags() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("pgrep node | xargs -r kill -9");
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_xargs_with_replace_flag() {
        // -I は次トークンを置換文字列として消費するため、その後の rm が実行対象。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("find . -name '*.tmp' | xargs -I {} rm -rf {}");
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_xargs_shell_c_command() {
        // xargs が sh -c を実行する場合は、シェル内のコマンドまで再帰的に抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo file | xargs sh -c 'rm -f \"$@\"' sh");
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"sh".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_eval_inner_command() {
        // eval の引数はシェルとして再評価されるため、内側のコマンドも抽出する
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("eval 'rm -rf /tmp/test'");
        assert!(commands.contains(&"eval".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_find_exec_command() {
        // find -exec/-execdir は後続の引数をコマンドとして実行する
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"find . -name '*.tmp' -exec rm -rf {} \;");
        assert!(commands.contains(&"find".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_find_execdir_command_with_plus_terminator() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("find . -type f -execdir kill -9 {} +");
        assert!(commands.contains(&"find".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_nested_wrappers() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo bash -c 'rm -rf /'");
        assert!(commands.contains(&"sudo".to_string()));
        assert!(commands.contains(&"bash".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_nohup_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("nohup kill -9 1234 &");
        assert!(commands.contains(&"nohup".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_semicolon_with_yarn() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo \"install\"; yarn install");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"yarn".to_string()));
    }

    #[test]
    fn test_extract_semicolon_with_pnpm() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo \"not yarn install\"; pnpm install");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"pnpm".to_string()));
        // クォート内の文字列から yarn は抽出しない。
        assert!(!commands.contains(&"yarn".to_string()));
    }

    #[test]
    fn test_extract_commands_in_quotes_not_executed() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo 'rm -rf /'");
        assert!(commands.contains(&"echo".to_string()));
        // rm は引数内クォートなので抽出しない。
        assert!(!commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_command_substitution() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo $(yarn --version)");
        assert!(commands.contains(&"echo".to_string()));
        // $() 内の yarn は実行コマンドとして抽出する。
        assert!(
            commands.contains(&"yarn".to_string()),
            "yarn should be extracted from command substitution: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_substitution_backticks() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo `yarn --version`");
        assert!(commands.contains(&"echo".to_string()));
        // バッククォート内の yarn は実行コマンドとして抽出する。
        assert!(
            commands.contains(&"yarn".to_string()),
            "yarn should be extracted from backtick command substitution: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_subshell() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("(cd project && yarn install)");
        assert!(commands.contains(&"cd".to_string()));
        assert!(commands.contains(&"yarn".to_string()));
    }

    // === 境界条件テスト ===

    #[test]
    fn test_extract_commands_empty_input() {
        let mut parser = ShellParser::new();
        assert!(parser.extract_commands("").is_empty());
    }

    #[test]
    fn test_extract_commands_whitespace_only() {
        let mut parser = ShellParser::new();
        assert!(parser.extract_commands("   ").is_empty());
        assert!(parser.extract_commands("\t\n").is_empty());
    }

    #[test]
    fn test_extract_commands_trailing_operator() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("ls &&");
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_extract_commands_leading_operator() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("&& ls");
        // 先頭が演算子でも ls は抽出する。
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_parse_shell_tokens_empty() {
        let tokens = parse_shell_tokens("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_whitespace_only() {
        let tokens = parse_shell_tokens("   ");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_escaped_space() {
        let tokens = parse_shell_tokens("echo foo\\ bar");
        assert_eq!(tokens, vec!["echo", "foo bar"]);
    }

    #[test]
    fn test_parse_shell_tokens_escaped_quote() {
        let tokens = parse_shell_tokens("echo \"hello\\\"world\"");
        assert_eq!(tokens, vec!["echo", "hello\"world"]);
    }

    #[test]
    fn test_parse_shell_tokens_mixed_quotes() {
        let tokens = parse_shell_tokens("echo 'single' \"double\"");
        assert_eq!(tokens, vec!["echo", "single", "double"]);
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_commands_ignores_operators_in_quotes() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo \"a && b\" && rm -rf /tmp/test");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"rm".to_string()));
        // "b" はクォート内部なので抽出しない。
        assert!(!commands.contains(&"b".to_string()));
    }

    #[test]
    fn test_extract_commands_newline_separated() {
        let mut parser = ShellParser::new();
        // セミコロン区切りのコマンドを抽出する（改行はシェル側で解釈）。
        let commands = parser.extract_commands("ls; echo hello");
        assert!(commands.contains(&"ls".to_string()));
        assert!(commands.contains(&"echo".to_string()));
    }

    // === is_env_assignment_token のテスト ===

    #[test]
    fn test_is_env_assignment_valid() {
        assert!(ShellParser::is_env_assignment_token("FOO=bar"));
        assert!(ShellParser::is_env_assignment_token("A_B_C=value"));
        assert!(ShellParser::is_env_assignment_token("_VAR=1"));
        assert!(ShellParser::is_env_assignment_token("X="));
    }

    #[test]
    fn test_is_env_assignment_invalid() {
        assert!(!ShellParser::is_env_assignment_token("=nokey"));
        assert!(!ShellParser::is_env_assignment_token("123=bad"));
        assert!(!ShellParser::is_env_assignment_token(""));
        assert!(!ShellParser::is_env_assignment_token("noequals"));
        assert!(!ShellParser::is_env_assignment_token("a-b=val"));
    }

    // === is_timeout_duration_token のテスト ===

    #[test]
    fn test_is_timeout_duration_valid() {
        assert!(ShellParser::is_timeout_duration_token("10"));
        assert!(ShellParser::is_timeout_duration_token("3.5"));
        assert!(ShellParser::is_timeout_duration_token("30s"));
        assert!(ShellParser::is_timeout_duration_token("5m"));
        assert!(ShellParser::is_timeout_duration_token("1h"));
        assert!(ShellParser::is_timeout_duration_token("2d"));
        assert!(ShellParser::is_timeout_duration_token("0"));
        assert!(ShellParser::is_timeout_duration_token("0.5"));
    }

    #[test]
    fn test_is_timeout_duration_invalid() {
        assert!(!ShellParser::is_timeout_duration_token(""));
        assert!(!ShellParser::is_timeout_duration_token("abc"));
        assert!(!ShellParser::is_timeout_duration_token("10x"));
        assert!(!ShellParser::is_timeout_duration_token("s"));
        assert!(!ShellParser::is_timeout_duration_token("-1"));
        assert!(!ShellParser::is_timeout_duration_token("NaN"));
    }

    // === unwrap_subshell のテスト ===

    #[test]
    fn test_unwrap_subshell_simple() {
        assert_eq!(ShellParser::unwrap_subshell("(inner)"), Some("inner"));
    }

    #[test]
    fn test_unwrap_subshell_with_spaces() {
        assert_eq!(
            ShellParser::unwrap_subshell("  (echo hello)  "),
            Some("echo hello")
        );
    }

    #[test]
    fn test_unwrap_subshell_nested() {
        assert_eq!(
            ShellParser::unwrap_subshell("(echo $(pwd))"),
            Some("echo $(pwd)")
        );
    }

    #[test]
    fn test_unwrap_subshell_not_wrapped() {
        assert_eq!(ShellParser::unwrap_subshell("echo hello"), None);
    }

    #[test]
    fn test_unwrap_subshell_unbalanced() {
        assert_eq!(ShellParser::unwrap_subshell("(no close"), None);
    }

    #[test]
    fn test_unwrap_subshell_middle_close() {
        // 最外周の括弧が途中で閉じるケース → None
        assert_eq!(ShellParser::unwrap_subshell("(a)(b)"), None);
    }

    // === extract_nested_command_fragments のテスト ===

    #[test]
    fn test_extract_nested_dollar_paren() {
        let frags = ShellParser::extract_nested_command_fragments("echo $(rm -rf /tmp)");
        assert_eq!(frags, vec!["rm -rf /tmp"]);
    }

    #[test]
    fn test_extract_nested_backticks() {
        let frags = ShellParser::extract_nested_command_fragments("echo `ls -la`");
        assert_eq!(frags, vec!["ls -la"]);
    }

    #[test]
    fn test_extract_nested_mixed() {
        let frags = ShellParser::extract_nested_command_fragments("$(cmd1) and `cmd2`");
        assert_eq!(frags, vec!["cmd1", "cmd2"]);
    }

    #[test]
    fn test_extract_nested_none() {
        let frags = ShellParser::extract_nested_command_fragments("plain text");
        assert!(frags.is_empty());
    }

    #[test]
    fn test_extract_nested_in_single_quotes_ignored() {
        // シングルクォート内のコマンド置換は無視される
        let frags = ShellParser::extract_nested_command_fragments("echo '$(dangerous)'");
        assert!(frags.is_empty());
    }

    #[test]
    fn test_extract_nested_dollar_paren_deep() {
        let frags = ShellParser::extract_nested_command_fragments("$(echo $(inner))");
        // 最外周の $() のみ抽出
        assert_eq!(frags, vec!["echo $(inner)"]);
    }

    // === コマンド置換断片の境界値テスト ===

    #[test]
    fn test_extract_parenthesized_fragment_ignores_quoted_and_escaped_parentheses() {
        let chars: Vec<char> = r#"(printf ')' \) && rm)"#.chars().collect();

        let (end, inner) = ShellParser::extract_parenthesized_fragment(&chars, 0)
            .expect("対応する閉じ括弧を検出できること");

        assert_eq!(end, chars.len() - 1);
        assert_eq!(inner, r#"printf ')' \) && rm"#);
    }

    #[test]
    fn test_extract_parenthesized_fragment_unclosed_returns_none() {
        let chars: Vec<char> = "(echo $(rm)".chars().collect();

        assert!(ShellParser::extract_parenthesized_fragment(&chars, 0).is_none());
    }

    #[test]
    fn test_extract_backtick_fragment_ignores_escaped_terminator() {
        let chars: Vec<char> = r"`printf \`literal\`; rm`".chars().collect();

        let (end, inner) = ShellParser::extract_backtick_fragment(&chars, 0)
            .expect("エスケープされていない終端を検出できること");

        assert_eq!(end, chars.len() - 1);
        assert_eq!(inner, r"printf \`literal\`; rm");
    }

    #[test]
    fn test_extract_backtick_fragment_unclosed_returns_none() {
        let chars: Vec<char> = "`echo rm".chars().collect();

        assert!(ShellParser::extract_backtick_fragment(&chars, 0).is_none());
    }

    // === find_wrapped_command_index のテスト ===

    #[test]
    fn test_find_wrapped_sudo_user() {
        let args: Vec<String> = vec!["-u", "root", "rm", "-rf", "/tmp"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_timeout_duration() {
        let args: Vec<String> = vec!["10", "rm", "-rf", "/tmp"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_timeout_with_signal() {
        let args: Vec<String> = vec!["--signal", "TERM", "10", "dd"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(3)
        );
    }

    #[test]
    fn test_find_wrapped_env_with_assignments() {
        let args: Vec<String> = vec!["VAR=x", "FOO=y", "rm", "-rf"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("env", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_double_dash() {
        let args: Vec<String> = vec!["--", "rm", "-rf"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_empty_args() {
        let args: Vec<String> = vec![];
        assert_eq!(ShellParser::find_wrapped_command_index("sudo", &args), None);
    }

    #[test]
    fn test_find_wrapped_only_flags() {
        let args: Vec<String> = vec!["-n".to_string()];
        assert_eq!(ShellParser::find_wrapped_command_index("sudo", &args), None);
    }

    // === parse_effective_command のテスト ===

    #[test]
    fn test_parse_effective_command_with_env_prefix() {
        let tokens: Vec<String> = vec!["VAR=x", "FOO=y", "rm", "-rf", "/tmp"]
            .into_iter()
            .map(String::from)
            .collect();
        let (cmd, args) = ShellParser::parse_effective_command(&tokens).unwrap();
        assert_eq!(cmd, "rm");
        assert_eq!(args, vec!["-rf", "/tmp"]);
    }

    #[test]
    fn test_parse_effective_command_no_env() {
        let tokens: Vec<String> = vec!["ls", "-la"].into_iter().map(String::from).collect();
        let (cmd, args) = ShellParser::parse_effective_command(&tokens).unwrap();
        assert_eq!(cmd, "ls");
        assert_eq!(args, vec!["-la"]);
    }

    #[test]
    fn test_parse_effective_command_all_env() {
        let tokens: Vec<String> = vec!["A=1", "B=2"].into_iter().map(String::from).collect();
        assert!(ShellParser::parse_effective_command(&tokens).is_none());
    }

    #[test]
    fn test_parse_effective_command_empty() {
        let tokens: Vec<String> = vec![];
        assert!(ShellParser::parse_effective_command(&tokens).is_none());
    }

    #[test]
    fn test_parse_effective_command_skips_control_prefixes() {
        // 制御構文の前置キーワードやヘッダを読み飛ばし、内側の実コマンドへ到達する
        for (raw, expected) in [
            (vec!["then", "rm", "-rf", "/tmp"], "rm"),
            (vec!["do", "kill", "-9", "1"], "kill"),
            (vec!["elif", "dd", "if=/dev/zero"], "dd"),
            (vec!["else", "rm", "-rf", "/tmp"], "rm"),
            (vec!["while", "kill", "-9", "1"], "kill"),
            (vec!["until", "dd", "if=/dev/zero"], "dd"),
            (vec!["!", "rm", "-rf", "/tmp"], "rm"),
            (vec!["then", "VAR=x", "{", "rm", "-rf", "/tmp"], "rm"),
            (vec!["for", "f", "in", "a", "do", "rm", "-rf", "/tmp"], "rm"),
            (vec!["case", "x", "in", "x)", "dd", "if=/dev/zero"], "dd"),
        ] {
            let tokens = raw.into_iter().map(String::from).collect::<Vec<_>>();
            let (cmd, _) = ShellParser::parse_effective_command(&tokens).expect("command expected");
            assert_eq!(cmd, expected, "tokens did not resolve to expected command");
        }
    }

    #[test]
    fn test_parse_effective_command_headers_and_closers_have_no_command() {
        // ループ変数・case ワード・case パターン・閉じ語はコマンドではない（None）
        for raw in [
            vec!["for", "rm", "in", "a"],      // ループ変数 rm はコマンドではない
            vec!["select", "kill", "in", "a"], // ループ変数 kill はコマンドではない
            vec!["case", "x", "in", "dd)"],    // パターン dd) はコマンドではない
            vec!["fi"],
            vec!["done"],
            vec!["esac"],
            vec!["}"],
        ] {
            let tokens = raw.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(
                ShellParser::parse_effective_command(&tokens).is_none(),
                "header/closer must not be treated as a command"
            );
        }
    }

    #[test]
    fn test_fallback_control_structures_detect_dangerous_commands() {
        // フォールバックパーサー（非 ast-parser 経路）が制御構文内の危険コマンドを
        // 取りこぼさないことの回帰テスト。以前は if/then・for/do・while/do・case・
        // ヘッダ内コマンド置換でこれらが素通り（fail-open）していた。
        let parser = ShellParser::new();
        for (cmd, expected) in [
            ("if true; then rm -rf /; fi", "rm"),
            ("for f in a; do rm -rf /; done", "rm"),
            ("while true; do kill -9 1; done", "kill"),
            ("case x in x) dd if=/dev/zero of=/dev/sda;; esac", "dd"),
            ("if true; then for f in a; do rm -rf /; done; fi", "rm"),
            ("case w in a) echo ok;; b) kill -9 1;; esac", "kill"),
            ("case w in a|b) echo ok;; c|d) rm -rf /tmp/x;; esac", "rm"),
            ("! rm -rf /tmp/x", "rm"),
            ("until rm -rf /tmp/x; do true; done", "rm"),
            ("for f in $(rm -rf /tmp); do echo; done", "rm"),
            ("case $(rm -rf /tmp) in x) echo;; esac", "rm"),
        ] {
            let detected = parser
                .extract_commands_fallback(cmd)
                .iter()
                .any(|c| command_key(c) == expected);
            assert!(detected, "fallback must detect `{expected}` in: {cmd}");
        }
    }

    #[test]
    fn test_fallback_control_syntax_no_false_positive() {
        // ループ変数・case パターン・引数位置のキーワードを危険コマンドと誤検知しない
        let parser = ShellParser::new();
        for cmd in [
            "for rm in a; do echo safe; done",
            "select kill in a; do echo safe; done",
            "case x in rm) echo safe;; kill) echo safe;; dd) echo safe;; esac",
            "echo then",
            "grep do file",
            "VAR=if echo ok",
        ] {
            let dangerous = parser
                .extract_commands_fallback(cmd)
                .iter()
                .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd"));
            assert!(!dangerous, "fallback must NOT flag safe command: {cmd}");
        }
    }

    // === parse_shell_tokens の追加テスト ===

    #[test]
    fn test_parse_shell_tokens_empty_input() {
        // 空文字列は空のベクタを返す
        let tokens = parse_shell_tokens("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_whitespace_only_mixed() {
        // スペースとタブのみの入力は空のベクタを返す
        let tokens = parse_shell_tokens("  \t  ");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_single_quotes() {
        // シングルクォート内のスペースはトークンを分割しない
        let tokens = parse_shell_tokens("echo 'hello world'");
        assert_eq!(tokens, vec!["echo", "hello world"]);
    }

    #[test]
    fn test_parse_shell_tokens_double_quotes() {
        // ダブルクォート内のスペースはトークンを分割しない
        let tokens = parse_shell_tokens("echo \"hello world\"");
        assert_eq!(tokens, vec!["echo", "hello world"]);
    }

    #[test]
    fn test_parse_shell_tokens_escaped_space_in_token() {
        // バックスラッシュでエスケープされたスペースはトークンを分割しない
        let tokens = parse_shell_tokens("echo hello\\ world");
        assert_eq!(tokens, vec!["echo", "hello world"]);
    }

    #[test]
    fn test_parse_shell_tokens_mixed_quotes_separate() {
        // シングルクォートとダブルクォートが混在する場合それぞれ独立したトークンになる
        let tokens = parse_shell_tokens("echo 'hello' \"world\"");
        assert_eq!(tokens, vec!["echo", "hello", "world"]);
    }

    #[test]
    fn test_parse_shell_tokens_unmatched_single_quote() {
        // 閉じられていないシングルクォートでも残りの文字列をトークンとして返す
        let tokens = parse_shell_tokens("echo 'hello");
        assert_eq!(tokens, vec!["echo", "hello"]);
    }

    #[test]
    fn test_parse_shell_tokens_unmatched_double_quote() {
        // 閉じられていないダブルクォートでも残りの文字列をトークンとして返す
        let tokens = parse_shell_tokens("echo \"hello");
        assert_eq!(tokens, vec!["echo", "hello"]);
    }

    #[test]
    fn test_parse_shell_tokens_unicode() {
        // Unicode文字（日本語）を正しくトークン分割する
        let tokens = parse_shell_tokens("echo こんにちは 世界");
        assert_eq!(tokens, vec!["echo", "こんにちは", "世界"]);
    }

    #[test]
    fn test_parse_shell_tokens_consecutive_spaces() {
        // 連続するスペースは無視してトークンを正しく分割する
        let tokens = parse_shell_tokens("echo   hello    world");
        assert_eq!(tokens, vec!["echo", "hello", "world"]);
    }

    // ========================================================================
    // wrapper_flag_takes_arg のテスト
    // ========================================================================

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_u_takes_arg() {
        // sudo -u は値を取るフラグ
        assert!(ShellParser::wrapper_flag_takes_arg("sudo", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_n_no_arg() {
        // sudo -n は値を取らないフラグ
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-n"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_long_user() {
        // sudo --user は値を取るロングフラグ
        assert!(ShellParser::wrapper_flag_takes_arg("sudo", "--user"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_long_user_inline() {
        // --user=root はインライン値なので追加引数不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "--user=root"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_uroot_concatenated() {
        // -uroot は値が連結されているので追加引数不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-uroot"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_nu_cluster_takes_arg() {
        // -nu の cluster: 末尾 `u` が値取得フラグ、`n` は値取らない
        // → 次トークン (`root`) を値として消費すべき（true）
        assert!(ShellParser::wrapper_flag_takes_arg("sudo", "-nu"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_path_prefixed_sudo() {
        // パス付きラッパーでも sudo の値付きオプションとして扱う
        assert!(ShellParser::wrapper_flag_takes_arg("/usr/bin/sudo", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_timeout_vs_cluster_takes_arg() {
        // timeout -vs の cluster: 末尾 `s` が値取得フラグ
        // → 次トークン (`TERM`) を値として消費すべき（true）
        assert!(ShellParser::wrapper_flag_takes_arg("timeout", "-vs"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_nv_cluster_no_value_flag() {
        // -nv の cluster: 両方とも値取らないフラグ → 追加トークン不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-nv"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_unroot_inline_value() {
        // -unroot の cluster: 先頭 `u` が値取得フラグで以降 `nroot` が inline 値
        // → 追加トークン不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-unroot"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_timeout_signal() {
        // timeout --signal は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("timeout", "--signal"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_timeout_kill_after() {
        // timeout --kill-after は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg(
            "timeout",
            "--kill-after"
        ));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_env_u() {
        // env -u は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("env", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_doas_u() {
        // doas -u は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("doas", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_nice_n() {
        // nice -n は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("nice", "-n"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_unknown_wrapper() {
        // 未知のラッパーはフラグを返さない
        assert!(!ShellParser::wrapper_flag_takes_arg("unknown", "-u"));
        assert!(!ShellParser::wrapper_flag_takes_arg("unknown", "--flag"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_double_dash() {
        // -- はフラグとして扱わない
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "--"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_single_dash() {
        // - はフラグとして扱わない
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-"));
    }

    // ========================================================================
    // find_wrapped_command_index のテスト
    // ========================================================================

    #[test]
    fn test_find_wrapped_command_index_sudo_rm() {
        // sudo rm -rf / → コマンド位置は 0 ("rm")
        let args: Vec<String> = vec!["rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(0)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_sudo_u_root_rm() {
        // sudo -u root rm -rf / → コマンド位置は 2 ("rm")
        let args: Vec<String> = vec!["-u", "root", "rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_sudo_env_assignment_rm() {
        // sudo VAR=value rm -rf / → sudo が受け取る環境変数代入をスキップ
        let args: Vec<String> = vec!["VAR=value", "rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_sudo_double_dash() {
        // sudo -- rm → -- 後の位置
        let args: Vec<String> = vec!["--", "rm"].into_iter().map(String::from).collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_su_double_dash_user() {
        // su -- root rm → `--` の後に残るユーザ位置引数(root)を消費し rm を指す。
        // `--` 直後を無条件にコマンド扱いすると root を誤検出し rm を見落とす。
        let args: Vec<String> = vec!["--", "root", "rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("su", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_gosu_double_dash_user() {
        // gosu -- root rm → gosu もユーザ位置引数を取るため同様に root を消費する。
        let args: Vec<String> = vec!["--", "root", "rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("gosu", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_su_double_dash_no_command() {
        // su -- root（コマンドなし）→ ユーザのみで実行コマンドは無いので None。
        let args: Vec<String> = vec!["--", "root"].into_iter().map(String::from).collect();
        assert_eq!(ShellParser::find_wrapped_command_index("su", &args), None);
    }

    #[test]
    fn test_extract_commands_su_double_dash_detects_rm() {
        // エンドツーエンド: `su -- root rm -rf /` から rm が抽出されること。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("su -- root rm -rf /tmp/x");
        assert!(
            commands.iter().any(|c| c == "rm"),
            "rm should be detected behind `su --`: {commands:?}"
        );
    }

    #[test]
    fn test_find_wrapped_command_index_env_assignment() {
        // env VAR=value rm → 環境変数代入をスキップ
        let args: Vec<String> = vec!["VAR=value", "rm"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("env", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_timeout_numeric() {
        // timeout 10 rm → 制限時間トークンをスキップ
        let args: Vec<String> = vec!["10", "rm"].into_iter().map(String::from).collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_timeout_with_suffix() {
        // timeout 30s rm → サフィックス付き制限時間
        let args: Vec<String> = vec!["30s", "rm"].into_iter().map(String::from).collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_timeout_signal_and_duration() {
        // timeout --signal TERM 10 rm → フラグ+制限時間をスキップ
        let args: Vec<String> = vec!["--signal", "TERM", "10", "rm"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(3)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_empty_args() {
        // 引数がない場合 → None
        let args: Vec<String> = vec![];
        assert_eq!(ShellParser::find_wrapped_command_index("sudo", &args), None);
    }

    // ========================================================================
    // is_timeout_duration_token のテスト
    // ========================================================================

    #[test]
    fn test_is_timeout_duration_token_integer() {
        // 整数の制限時間
        assert!(ShellParser::is_timeout_duration_token("10"));
    }

    #[test]
    fn test_is_timeout_duration_token_float() {
        // 小数の制限時間
        assert!(ShellParser::is_timeout_duration_token("0.5"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_s() {
        // 秒サフィックス
        assert!(ShellParser::is_timeout_duration_token("30s"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_m() {
        // 分サフィックス
        assert!(ShellParser::is_timeout_duration_token("5m"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_h() {
        // 時間サフィックス
        assert!(ShellParser::is_timeout_duration_token("2h"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_d() {
        // 日サフィックス
        assert!(ShellParser::is_timeout_duration_token("1d"));
    }

    #[test]
    fn test_is_timeout_duration_token_empty() {
        // 空文字列は無効
        assert!(!ShellParser::is_timeout_duration_token(""));
    }

    #[test]
    fn test_is_timeout_duration_token_alphabetic() {
        // アルファベット文字列は無効
        assert!(!ShellParser::is_timeout_duration_token("abc"));
    }

    #[test]
    fn test_is_timeout_duration_token_negative() {
        // 負数は無効
        assert!(!ShellParser::is_timeout_duration_token("-1"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_only() {
        // サフィックスのみは無効
        assert!(!ShellParser::is_timeout_duration_token("s"));
    }

    // ========================================================================
    // is_env_assignment_token のテスト
    // ========================================================================

    #[test]
    fn test_is_env_assignment_token_valid() {
        // 正常な環境変数代入
        assert!(ShellParser::is_env_assignment_token("VAR=value"));
    }

    #[test]
    fn test_is_env_assignment_token_underscore_prefix() {
        // アンダースコアで始まる変数名
        assert!(ShellParser::is_env_assignment_token("_VAR=value"));
    }

    #[test]
    fn test_is_env_assignment_token_alphanumeric() {
        // 英数字の変数名
        assert!(ShellParser::is_env_assignment_token("A123=x"));
    }

    #[test]
    fn test_is_env_assignment_token_empty_name() {
        // 空の変数名は無効
        assert!(!ShellParser::is_env_assignment_token("=value"));
    }

    #[test]
    fn test_is_env_assignment_token_digit_start() {
        // 数字で始まる変数名は無効
        assert!(!ShellParser::is_env_assignment_token("123=value"));
    }

    #[test]
    fn test_is_env_assignment_token_no_equals() {
        // = がない場合は無効
        assert!(!ShellParser::is_env_assignment_token("rm"));
    }

    // ========================================================================
    // unwrap_subshell のテスト
    // ========================================================================

    #[test]
    fn test_unwrap_subshell_wrapped_command() {
        // 単純なサブシェル
        assert_eq!(ShellParser::unwrap_subshell("(ls -la)"), Some("ls -la"));
    }

    #[test]
    fn test_unwrap_subshell_plain_command() {
        // サブシェルでない場合
        assert_eq!(ShellParser::unwrap_subshell("ls -la"), None);
    }

    #[test]
    fn test_unwrap_subshell_multiple_groups() {
        // 複数の括弧グループがある場合
        assert_eq!(ShellParser::unwrap_subshell("(a) && (b)"), None);
    }

    #[test]
    fn test_unwrap_subshell_unclosed() {
        // 閉じられていない括弧
        assert_eq!(ShellParser::unwrap_subshell("(unclosed"), None);
    }

    #[test]
    fn test_unwrap_subshell_quoted_paren() {
        // クォート内の括弧は無視される
        let result = ShellParser::unwrap_subshell("(ls 'a)' -la)");
        assert!(result.is_some());
    }

    // ========================================================================
    // extract_nested_command_fragments のテスト
    // ========================================================================

    #[test]
    fn test_extract_nested_fragments_dollar_paren() {
        // $() 形式のコマンド置換
        let fragments = ShellParser::extract_nested_command_fragments("echo $(rm -rf /)");
        assert_eq!(fragments, vec!["rm -rf /"]);
    }

    #[test]
    fn test_extract_nested_fragments_backtick() {
        // バッククォート形式のコマンド置換
        let fragments = ShellParser::extract_nested_command_fragments("echo `rm -rf /`");
        assert_eq!(fragments, vec!["rm -rf /"]);
    }

    #[test]
    fn test_extract_nested_fragments_no_substitution() {
        // コマンド置換なし
        let fragments = ShellParser::extract_nested_command_fragments("echo hello");
        assert!(fragments.is_empty());
    }

    #[test]
    fn test_extract_nested_fragments_empty_substitution() {
        // 空のコマンド置換
        let fragments = ShellParser::extract_nested_command_fragments("echo $()");
        assert!(fragments.is_empty());
    }

    // ========================================================================
    // process_wrapper_args の統合テスト（extract_commands 経由）
    // ========================================================================

    #[test]
    fn test_process_wrapper_sudo_rm() {
        // sudo rm -rf / → "sudo" と "rm" の両方を含む
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo rm -rf /");
        assert!(commands.contains(&"sudo".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_process_wrapper_escaped_sudo_rm() {
        // `s\udo` は quote removal 後に `sudo` なので、内側の rm も抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"s\udo rm -rf /");
        assert!(
            commands.contains(&"rm".to_string()),
            "escaped sudo wrapper should expose wrapped rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_process_wrapper_sudo_u_root_bash_c() {
        // sudo -u root bash -c 'rm -rf /' → "rm" を含む（ネストラッパー + shell -c）
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -u root bash -c 'rm -rf /'");
        assert!(
            commands.contains(&"rm".to_string()),
            "commands should contain 'rm', got: {:?}",
            commands
        );
    }

    #[test]
    fn test_process_wrapper_timeout_signal_rm() {
        // timeout --signal TERM 10 rm -f /tmp/file → "rm" を含む
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("timeout --signal TERM 10 rm -f /tmp/file");
        assert!(
            commands.contains(&"rm".to_string()),
            "commands should contain 'rm', got: {:?}",
            commands
        );
    }

    #[test]
    fn test_process_wrapper_env_command_rm() {
        // env VAR=value command rm -f → "rm" を含む
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("env VAR=value command rm -f");
        assert!(
            commands.contains(&"rm".to_string()),
            "commands should contain 'rm', got: {:?}",
            commands
        );
    }

    // === エッジケーステスト ===

    #[test]
    fn test_extract_commands_empty_string() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("");
        assert!(
            commands.is_empty(),
            "空文字列からはコマンドが抽出されないべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_only_whitespace_and_tabs() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("   \t  ");
        assert!(
            commands.is_empty(),
            "空白のみからはコマンドが抽出されないべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_deeply_nested_subshells() {
        // 3層のネストされたサブシェル
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -c 'sh -c \"bash -c rm\"'");
        assert!(
            commands.contains(&"rm".to_string()),
            "深いネストでも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_null_byte_in_command() {
        // NULL バイトを含むコマンド（パーサーがクラッシュしないこと）
        let mut parser = ShellParser::new();
        let _commands = parser.extract_commands("echo hello\0world");
        // パニックしなければ成功
    }

    #[test]
    fn test_extract_commands_very_long_pipeline() {
        // 長いパイプライン
        let mut parser = ShellParser::new();
        let cmd = (0..50)
            .map(|i| format!("cmd{}", i))
            .collect::<Vec<_>>()
            .join(" | ");
        let commands = parser.extract_commands(&cmd);
        assert!(
            !commands.is_empty(),
            "長いパイプラインからもコマンドを抽出すべき"
        );
    }

    #[test]
    fn test_extract_commands_mixed_quote_styles() {
        // シングルクォートとダブルクォートの混在
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r#"echo "hello 'world'" && rm -f test"#);
        assert!(
            commands.contains(&"rm".to_string()),
            "混合クォートでも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_consecutive_semicolons() {
        // 連続するセミコロン
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo a ;; echo b ; rm file");
        assert!(
            commands.contains(&"rm".to_string()),
            "連続セミコロンでも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_heredoc() {
        // ヒアドキュメント内のコマンドは実行されない
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cat <<EOF\nrm -rf /\nEOF");
        // ヒアドキュメント内の rm は実際のコマンドではない
        // cat のみが抽出されるのが理想
        assert!(
            commands.contains(&"cat".to_string()),
            "ヒアドキュメントで cat を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_comment_ignored() {
        // コメント後のコマンドは無視されるべき
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo hello # rm -rf /");
        assert!(
            !commands.contains(&"rm".to_string()),
            "コメント内の rm は検出すべきでない: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_backgrounded_command() {
        // バックグラウンド実行コマンド
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("rm -f file &");
        assert!(
            commands.contains(&"rm".to_string()),
            "バックグラウンド実行でも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_strings_empty() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_command_strings("");
        assert!(
            commands.is_empty(),
            "空文字列からはコマンド文字列が抽出されないべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_strings_with_args() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_command_strings("npm install lodash");
        assert!(
            commands
                .iter()
                .any(|c| c.contains("npm") && c.contains("install")),
            "npm install を含むコマンド文字列が抽出されるべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_strings_splits_on_newline() {
        // 改行はトップレベル区切りとして扱う。改行で分割されないと、
        // アンカー付きカスタム正規表現フィルタ（`^rm `）が後続コマンドを取りこぼす。
        let parser = ShellParser::new();
        let commands = parser.extract_command_strings_fallback("echo ok\nrm -rf /tmp/foo");
        assert!(
            commands.iter().any(|c| c.starts_with("rm")),
            "改行区切り後の rm がコマンド文字列として独立抽出されるべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_strings_splits_on_background_amp() {
        // 単独 `&`（バックグラウンド実行）もトップレベル区切り。
        let parser = ShellParser::new();
        let commands = parser.extract_command_strings_fallback("echo ok & rm -rf /tmp/foo");
        assert!(
            commands.iter().any(|c| c.starts_with("rm")),
            "& 区切り後の rm がコマンド文字列として独立抽出されるべき: {:?}",
            commands
        );
    }

    // --- フォールバックパーサー: ラッパーコマンド展開のテスト ---

    #[test]
    fn test_fallback_wrapper_no_duplicate_command() {
        // ラッパー展開でコマンドが二重追加されないことを確認
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("sudo rm -rf /tmp");
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(
            rm_count, 1,
            "rm は1回だけ抽出されるべき（二重追加バグの回帰テスト）: {:?}",
            commands
        );
    }

    #[test]
    fn test_fallback_nested_wrappers_no_duplicate() {
        // 二重ラッパーでもコマンドが重複しない
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("timeout 10 sudo rm -rf /tmp");
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "二重ラッパーでも rm は1回だけ: {:?}", commands);
    }

    #[test]
    fn test_fallback_wrapper_shell_c_extracts_inner_command() {
        // wrapper の実行対象が shell -c の場合、内側の危険コマンドまで抽出する
        let parser = ShellParser::new();
        let commands =
            parser.extract_commands_from_segment_fallback("sudo -u root bash -c 'rm -rf /tmp'");
        assert!(
            commands.contains(&"rm".to_string()),
            "sudo 経由の bash -c から rm が抽出されるべき: {:?}",
            commands
        );
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "rm は1回だけ: {:?}", commands);
    }

    #[test]
    fn test_fallback_wrapper_command_only_no_args() {
        // ラッパーの直後にコマンドだけで引数なし（境界ケース）
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("sudo rm");
        assert!(
            commands.contains(&"rm".to_string()),
            "sudo rm から rm が抽出されるべき: {:?}",
            commands
        );
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "rm は1回だけ: {:?}", commands);
    }

    #[test]
    fn test_fallback_env_wrapper_with_assignment() {
        // env VAR=x を経由したコマンド抽出
        let parser = ShellParser::new();
        let commands =
            parser.extract_commands_from_segment_fallback("env HOME=/tmp rm -rf /var/data");
        assert!(
            commands.contains(&"rm".to_string()),
            "env ラッパー経由で rm が抽出されるべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_fallback_wrapper_with_remaining_args() {
        // ラッパー展開後の残り引数が正しく処理される
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("sudo -u root rm -rf /var");
        assert!(
            commands.contains(&"rm".to_string()),
            "sudo -u root 経由で rm が抽出されるべき: {:?}",
            commands
        );
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "rm は1回だけ: {:?}", commands);
    }

    // === parse_shell_tokens の未閉じクォートエッジケース ===

    #[test]
    fn test_parse_shell_tokens_unclosed_single_quote_no_panic() {
        // 未閉じシングルクォートでパニックしないことを確認
        let tokens = parse_shell_tokens("echo 'hello world");
        assert!(!tokens.is_empty(), "未閉じクォートでもトークンが返るべき");
        assert_eq!(tokens[0], "echo");
    }

    #[test]
    fn test_parse_shell_tokens_unclosed_double_quote_no_panic() {
        // 未閉じダブルクォートでパニックしないことを確認
        let tokens = parse_shell_tokens("rm \"unterminated arg");
        assert_eq!(tokens[0], "rm");
    }

    // === split_by_logical_ops のエッジケース ===

    #[test]
    fn test_split_by_logical_ops_preserves_quoted_operators() {
        // クォート内の && はコマンド区切りとして扱わない
        let result = ShellParser::split_by_logical_ops(r#"echo "a && b" && rm file"#);
        assert_eq!(result.len(), 2, "クォート内の && は分割すべきでない");
        assert_eq!(result[1].trim(), "rm file");
    }

    #[test]
    fn test_split_by_logical_ops_single_pipe_not_split() {
        // 単一の | はパイプであり論理演算子ではないため分割しない
        let result = ShellParser::split_by_logical_ops("cat file | grep error || echo fail");
        assert_eq!(result.len(), 2, "|| で分割されるべき");
        assert!(result[0].contains("cat file | grep error"));
    }

    // === split_respecting_quotes: クォート内の区切り文字を保護 ===

    #[test]
    fn test_split_respecting_quotes_semicolon_in_single_quote() {
        // シングルクォート内のセミコロンは区切りとして扱わない
        let result = ShellParser::split_respecting_quotes("printf 'hello; world'", ';');
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "printf 'hello; world'");
    }

    #[test]
    fn test_split_respecting_quotes_pipe_in_double_quote() {
        // ダブルクォート内のパイプは区切りとして扱わない
        let result = ShellParser::split_respecting_quotes(r#"echo "safe | text""#, '|');
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], r#"echo "safe | text""#);
    }

    #[test]
    fn test_split_respecting_quotes_separator_outside_quotes() {
        // クォート外の区切り文字は通常通り分割する
        let result = ShellParser::split_respecting_quotes("a; b; c", ';');
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_respecting_quotes_paren_protection() {
        // 括弧内のセミコロンは区切らない（サブシェル内のコマンド連鎖を維持）
        let result = ShellParser::split_respecting_quotes("(a; b); c", ';');
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "(a; b)");
        assert_eq!(result[1], "c");
    }

    #[test]
    fn test_extract_commands_does_not_split_inside_single_quote_for_rm() {
        // 偽陽性回帰テスト: シングルクォート内に rm 文字列があっても誤ブロックしない。
        // AST/フォールバックの両方で printf が最初のコマンドとして検出され、rm は漏れない。
        let mut parser = ShellParser::new();
        let cmds = parser.extract_commands("printf 'hello; rm -rf /'");
        assert!(
            cmds.iter().any(|c| c == "printf"),
            "printf should be detected, got: {:?}",
            cmds
        );
        assert!(
            !cmds.iter().any(|c| c == "rm"),
            "rm should NOT leak from quoted string, got: {:?}",
            cmds
        );
    }

    #[test]
    fn test_extract_commands_does_not_split_inside_double_quote_for_rm() {
        // 偽陽性回帰テスト: ダブルクォート内のパイプ + rm を誤検出しない。
        let mut parser = ShellParser::new();
        let cmds = parser.extract_commands(r#"echo "safe | rm -rf /""#);
        assert!(
            cmds.iter().any(|c| c == "echo"),
            "echo should be detected, got: {:?}",
            cmds
        );
        assert!(
            !cmds.iter().any(|c| c == "rm"),
            "rm should NOT leak from quoted string, got: {:?}",
            cmds
        );
    }

    // === セキュリティ回帰防止: 危険コマンド検出バイパスの修正 ===

    #[test]
    fn test_extract_commands_number_arg_does_not_swallow_command() {
        // 「値を取るフラグ + 数値」で number ノードが脱落し、フラグが後続コマンドを
        // 消費して検出漏れになるバグの回帰防止。
        let mut parser = ShellParser::new();
        for command in [
            "ls | xargs -n 1 rm -rf",
            "nice -n 10 rm -rf /tmp/x",
            "sudo -u 1000 rm -rf /tmp/x",
        ] {
            let commands = parser.extract_commands(command);
            assert!(
                commands.iter().any(|c| command_key(c) == "rm"),
                "{command}: rm が検出されるべき: {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_bash_c_double_dash() {
        // `bash -c -- 'script'` で -- を挟むとスクリプトを取りこぼすバグの回帰防止。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -c -- 'rm -rf /tmp/x'");
        assert!(
            commands.iter().any(|c| command_key(c) == "rm"),
            "bash -c -- 'rm...' で rm が検出されるべき: {commands:?}"
        );
    }

    #[test]
    fn test_extract_commands_env_split_string() {
        // env -S / --split-string は文字列をコマンドとして再評価するため再帰抽出する。
        let mut parser = ShellParser::new();
        for command in [
            "env -S'rm -rf /tmp/x'",
            "env -S 'rm -rf /tmp/x'",
            "env --split-string='rm -rf /tmp/x'",
            "env --split-string 'rm -rf /tmp/x'",
        ] {
            let commands = parser.extract_commands(command);
            assert!(
                commands.iter().any(|c| command_key(c) == "rm"),
                "{command}: rm が検出されるべき: {commands:?}"
            );
        }
    }

    #[test]
    fn test_extract_commands_env_split_string_is_env_specific() {
        // env 以外の -S（例: sort -S はメモリサイズ指定）は誤ってコマンド抽出しない。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sort -S 1G input.txt");
        assert!(commands.iter().any(|c| command_key(c) == "sort"));
        assert!(
            !commands.iter().any(|c| command_key(c) == "rm"),
            "sort -S を誤ってコマンド抽出してはならない: {commands:?}"
        );
    }

    #[test]
    fn test_extract_commands_pathological_nesting_fails_closed() {
        // 多段ネストのコマンド置換でスタックオーバーフロー（フェイルオープン）せず、
        // 安全側（危険コマンド候補）に倒すことを確認する。
        let mut deep = String::from("echo safe");
        for _ in 0..200 {
            deep = format!("echo $({deep})");
        }
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(&deep);
        assert!(
            commands
                .iter()
                .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd")),
            "深いネストは block コマンドを返すべき: {commands:?}"
        );
    }

    #[test]
    fn test_extract_commands_long_input_fails_closed() {
        // 極端に長い入力も安全側に倒す。
        let long = format!("echo {}", "a".repeat(70_000));
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(&long);
        assert!(
            commands
                .iter()
                .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd")),
            "長大入力は block コマンドを返すべき"
        );
    }

    // === fail-closed / 深いネスト・ラッパー連鎖・ブレースグループ・プロセス置換の回帰テスト ===
    // （一時的な REPRO 調査テストを確実な回帰テストへ置き換えたもの。これらの
    //  セキュリティ的振る舞いは AST 経路とフォールバック経路の双方で成立する。）

    fn rm_blocked(cmd: &str) -> bool {
        let mut p = ShellParser::new();
        p.extract_commands(cmd)
            .iter()
            .any(|c| command_key(c) == "rm")
    }

    /// 長さ上限（65536）未満かつ括弧/ブレース無しで深い構造を作る && / パイプ / ;
    /// の連鎖でも、rm を検出（深さ上限内）または fail-closed（深さ上限超過）で
    /// 必ずブロック側に倒れること。AST 走査の深さ上限をすり抜ける fail-open を防ぐ。
    #[test]
    fn test_deep_logical_chains_block_under_len_limit() {
        for sep in ["&& ", "| ", "; "] {
            // 上限内（直接検出）と上限超過（fail-closed）の両経路を網羅する。
            for n in [400usize, 700] {
                let cmd = format!("true {sep}").repeat(n) + "rm -rf /tmp/x";
                assert!(cmd.len() < 65_536, "sep={sep:?} n={n} len={}", cmd.len());
                assert!(
                    rm_blocked(&cmd),
                    "sep={sep:?} n={n}: 深い連鎖で rm を取りこぼした (fail-open)"
                );
            }
        }
        // 大規模でもクラッシュせずブロック側に倒れること。
        let huge = "true && ".repeat(5000) + "rm -rf /tmp/x";
        assert!(huge.len() < 65_536);
        assert!(rm_blocked(&huge), "大規模な連鎖で rm を取りこぼした");
    }

    /// ブレースコマンドグループ `{ ...; }` 内の危険コマンドを検出すること。
    /// （以前フォールバック経路では先頭 `{` をコマンド名と誤認し rm を取りこぼしていた）
    #[test]
    fn test_brace_command_group_detects_inner_command() {
        for cmd in [
            "{ rm -rf /tmp/x; }",
            "{ rm -rf /tmp/x ; }",
            "true; { rm -rf /a; }",
            "{ echo hi; rm -rf /a; }",
        ] {
            assert!(
                rm_blocked(cmd),
                "{cmd}: ブレースグループ内の rm を取りこぼした"
            );
        }
    }

    /// 深いラッパー連鎖（`sudo sudo ... rm`）でスタックオーバーフロー（fail-open）せず、
    /// fail-closed でブロックすること。
    /// （以前 process_wrapper_args / expand_wrapper_commands_fallback が深さ無制限の
    ///  再帰だったため、長さ上限未満でもクラッシュ＝fail-open し得た）
    #[test]
    fn test_deep_wrapper_chain_fails_closed_without_overflow() {
        let cmd = "sudo ".repeat(5000) + "rm -rf /tmp/x";
        assert!(cmd.len() < 65_536, "len={}", cmd.len());
        assert!(
            rm_blocked(&cmd),
            "深いラッパー連鎖は fail-closed でブロックすべき"
        );
    }

    /// プロセス置換 `<(...)` / `>(...)` 内の危険コマンドを検出すること。
    /// プロセス置換は内側のコマンドを実際に実行するため。
    /// （以前フォールバック経路は `$()`/バッククォートのみ見ており取りこぼしていた）
    #[test]
    fn test_process_substitution_detects_inner_command() {
        for cmd in [
            "diff <(rm -rf /tmp/x) <(ls)",
            "tee >(rm -rf /tmp/x)",
            "cat <(rm -rf /a)",
        ] {
            assert!(rm_blocked(cmd), "{cmd}: プロセス置換内の rm を取りこぼした");
        }
    }

    /// リダイレクト `< file` / `> file` はプロセス置換 `<(` / `>(` と区別され、
    /// 過剰検出しないこと（直後が `(` のときだけプロセス置換として扱う）。
    #[test]
    fn test_redirection_not_misdetected_as_process_substitution() {
        for cmd in ["cat < file.txt", "echo done > out.txt", "echo a >> log"] {
            let mut p = ShellParser::new();
            let cmds = p.extract_commands(cmd);
            assert!(
                !cmds
                    .iter()
                    .any(|c| matches!(command_key(c).as_str(), "rm" | "kill" | "dd")),
                "{cmd}: リダイレクトを危険コマンドと誤検出した: {cmds:?}"
            );
        }
    }
}
