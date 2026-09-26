//! タイムアウト対応のコマンド実行ユーティリティ。

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt as _;
use tracing::{debug, warn};

/// タイムアウトによりコマンドが終了された場合の終了コード。
pub(crate) const TIMEOUT_EXIT_CODE: i32 = 124;

/// タイムアウト時にstderrに付加されるプレフィックス。
const TIMEOUT_STDERR_PREFIX: &str = "[Command timed out after";

/// 子プロセスが正常終了した後、リーダースレッドがパイプに残ったデータを
/// 読み切るために与える猶予秒数。
/// プロセス終了後の残データはパイプバッファ分（数十KB）に限られ通常は即座に
/// 読み終わるが、deadline を流用すると deadline 際どい正常終了で出力が失われ
/// 誤タイムアウトになる。一方、バックグラウンドの孫プロセスがパイプを保持し
/// 続けるケースでは EOF が来ないため、この猶予で打ち切ってプロセスグループを
/// kill する（孫プロセスによる timeout 回避の防止を維持する）。
const OUTPUT_DRAIN_GRACE_SECS: u64 = 5;

/// stdout/stderr ごとにメモリへ保持する最大バイト数。
///
/// 子プロセスのパイプ自体は最後まで読み続けてデッドロックを防ぐが、保持量を制限して
/// 大量出力する formatter/linter が claw-hooks を OOM 終了させるのを防ぐ。
const MAX_CAPTURED_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// 出力が保持上限を超えた場合に末尾へ付けるマーカー。
const OUTPUT_TRUNCATED_MARKER: &[u8] = b"\n[output truncated by claw-hooks]\n";

/// 子プロセスの終了・リーダースレッドの完了を待つポーリングの初期間隔。
///
/// 以前は固定 100ms（`try_wait`）/ 20ms（リーダー join）の `sleep` だったため、
/// 数ミリ秒で終わる formatter/linter でも 1 コマンドあたり必ず 100ms 以上待たされ、
/// 拡張子フックを 3 本設定すると 1 回のファイル編集でエージェントが 0.35 秒
/// ブロックされていた。短命なコマンドを取りこぼさないよう 1ms から始める。
const POLL_BACKOFF_INITIAL: Duration = Duration::from_millis(1);

/// ポーリング間隔の上限。
///
/// 長時間動くコマンドで 1ms ポーリングを続けると無駄に CPU を焼くため、
/// 指数的に伸ばしてここで頭打ちにする。タイムアウト判定の粒度もこの値になる。
const POLL_BACKOFF_MAX: Duration = Duration::from_millis(50);

/// 子プロセスの起動 (パイプの作成から fork / exec まで) を 1 つずつにするロック。
///
/// macOS の std はパイプを `pipe()` で作ってから `FD_CLOEXEC` を立てるため、Linux の
/// `pipe2(O_CLOEXEC)` と違って不可分でない。その間に別のスレッドが子プロセスを起動すると、
/// その子がこちらのパイプの書き込み側を継承して持ち続け、こちらのコマンドが終わっても
/// パイプが EOF にならない。並列に走るフックの結果が出力の排出の猶予
/// (`OUTPUT_DRAIN_GRACE_SECS`) まで待たされ、失敗したフックはタイムアウトと誤って
/// 報告される (macOS の CI で、並列のテストの `true` が 5 秒待たされて見つかった)。
/// 起動を直列にして、パイプの作成と別の起動が重ならないようにする。
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// `SPAWN_LOCK` を取って子プロセスを起動する。claw-hooks の子プロセスの起動は必ずここを通す。
///
/// ロックは `spawn()` の間だけ持つ (起動したコマンドの実行は並列のまま)。
pub fn spawn_serialized(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    let _guard = SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cmd.spawn()
}

/// 実行ファイルのパスから、ログやエラー表示に使用できるファイル名だけを返す。
///
/// 設定された絶対パスにはユーザー名や非公開のディレクトリ構成が含まれ得るため、
/// Unix/Windows のどちらの区切り文字も取り除く。実行不能な空文字やルートだけの
/// 値には、内容を露出しない固定ラベルを使用する。
pub(crate) fn program_label(program: &str) -> &str {
    program
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or("<unknown>")
}

/// Unix で子プロセスを新しいプロセスグループに配置する。
///
/// 効果: プロセスグループID == 子プロセスPID となるため、子の孫プロセス
/// （例: `sh -c 'sleep 600'` の `sleep`）も同じプロセスグループに属し、
/// `killpg(pid, SIGKILL)` でグループ全体を停止できる。
#[cfg(unix)]
fn configure_unix_process_group(cmd: &mut Command) {
    cmd.process_group(0);
}

/// Unix で子プロセスのプロセスグループ全体を SIGKILL で停止する。
/// `child.kill()` は子プロセスのみを対象とするため、`sh -c 'sleep'` のような
/// シェル経由のケースで孫プロセスがゾンビ/孤児として残るのを防ぐ。
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let pgid = pid as i32;
    // killpg は EPERM/ESRCH 等の失敗もあり得るが、フェイルセーフに留める。
    // 戻り値は意図的に無視する（後続で SIGKILL 直送 + wait で確実に回収する）。
    unsafe {
        let _ = libc::killpg(pgid, libc::SIGKILL);
    }
}

/// タイムアウトメタデータ付きのコマンド出力。
pub struct TimedOutput {
    /// キャプチャされたプロセス出力。
    pub output: Output,
    /// `run_with_timeout_tracked` によりタイムアウトでプロセスが強制終了された場合にtrue。
    pub timed_out: bool,
}

/// リーダースレッドの join 結果。
///
/// 出力自体は共有バッファ（`SharedOutput`）側に蓄積されるため、ここでは
/// 「EOF まで読み切れたか」だけを返す。
enum ReaderJoin {
    Finished,
    TimedOut,
}

/// リーダースレッドがパイプから読み出した内容を、上限付きで蓄積するバッファ。
///
/// スレッドの戻り値ではなく共有バッファへ逐次追記するのは、join を諦めた場合でも
/// 「そこまでに読めた分」を親スレッドから回収するため。戻り値方式では join できないと
/// 出力が丸ごと失われ、正常終了したフックの診断結果まで捨ててしまう。
#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    /// 読み出したチャンクを上限内で追記する。上限超過分は捨て、切り詰めた事実だけ残す。
    fn push(&mut self, chunk: &[u8]) {
        let remaining = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        let retained = chunk.len().min(remaining);
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.truncated |= retained < chunk.len();
    }

    /// 蓄積した出力を取り出す。切り詰めが発生していた場合は末尾にマーカーを付ける。
    fn take(&mut self) -> Vec<u8> {
        let mut bytes = std::mem::take(&mut self.bytes);
        if std::mem::take(&mut self.truncated) {
            let content_limit =
                MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(OUTPUT_TRUNCATED_MARKER.len());
            bytes.truncate(content_limit);
            bytes.extend_from_slice(OUTPUT_TRUNCATED_MARKER);
        }
        bytes
    }
}

/// リーダースレッドと親スレッドで共有する出力バッファ。
type SharedOutput = Arc<Mutex<CapturedOutput>>;

/// 共有バッファから、その時点までに読めた出力を取り出す。
///
/// リーダースレッドは `read` でブロックしている間ロックを握らないため、join を
/// 諦めた後でもここで安全に回収できる。ロックが毒されていても出力を失わないよう
/// `into_inner` で中身を救う（出力の欠落は誤ブロックに直結するため）。
fn take_shared_output(shared: &SharedOutput) -> Vec<u8> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// deadline を超えない範囲でポーリング間隔だけ待ち、次の間隔（2 倍・上限あり）を返す。
///
/// deadline 直前に上限いっぱい眠ると実際の待ち時間が設定値を超えてしまうため、
/// 残り時間で頭打ちにする。
fn sleep_with_backoff(backoff: Duration, deadline: Instant) -> Duration {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(backoff.min(remaining));
    (backoff * 2).min(POLL_BACKOFF_MAX)
}

#[cfg(unix)]
fn timeout_exit_status() -> ExitStatus {
    // Unixのwaitステータスは上位バイトに終了コードをエンコードする。
    ExitStatus::from_raw(TIMEOUT_EXIT_CODE << 8)
}

#[cfg(windows)]
fn timeout_exit_status() -> ExitStatus {
    ExitStatus::from_raw(TIMEOUT_EXIT_CODE as u32)
}

#[cfg(test)]
/// `run_with_timeout` によるタイムアウト出力かどうかを判定する。
pub fn is_timeout_output(output: &Output) -> bool {
    output.status.code() == Some(TIMEOUT_EXIT_CODE)
        && output.stderr.starts_with(TIMEOUT_STDERR_PREFIX.as_bytes())
}

/// リーダースレッドを deadline まで待って join する。
///
/// 子プロセスが正常終了していても、バックグラウンドの孫プロセスが stdout/stderr
/// のパイプを継承して保持し続けると `read_to_end` が EOF を受け取れず `join` が
/// 無期限にブロックし、設定したタイムアウトが無効化される。これを防ぐため、
/// deadline 超過時は未完了として返し、呼び出し元でプロセスグループ kill へ進める。
fn join_reader_before_deadline(
    handle: std::thread::JoinHandle<()>,
    deadline: Instant,
) -> ReaderJoin {
    let mut backoff = POLL_BACKOFF_INITIAL;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return ReaderJoin::TimedOut;
        }
        backoff = sleep_with_backoff(backoff, deadline);
    }
    // is_finished が true なので join は即座に返る。
    let _ = handle.join();
    ReaderJoin::Finished
}

/// リーダーを EOF まで排出しつつ、共有バッファへ上限内で蓄積する。
fn read_output_bounded(mut reader: impl Read, sink: &SharedOutput) {
    let mut buffer = [0u8; 8192];

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            // EINTR はシグナル割り込みによる中断でパイプはまだ生きている。ここで
            // 打ち切るとパイプを排出しきれず、大量出力する子プロセスがパイプ満杯で
            // ブロックしたまま誤タイムアウトになるため、読み取りを継続する。
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        sink.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(&buffer[..read]);
    }
}

/// タイムアウト通知の stderr 本文を組み立てる。
fn timeout_notice(timeout_secs: u64, command_desc: &str) -> Vec<u8> {
    format!(
        "{} {}s: {}]\n",
        TIMEOUT_STDERR_PREFIX, timeout_secs, command_desc
    )
    .into_bytes()
}

fn timeout_output(timeout_secs: u64, command_desc: &str) -> Output {
    Output {
        status: timeout_exit_status(),
        stdout: Vec::new(),
        stderr: timeout_notice(timeout_secs, command_desc),
    }
}

/// パイプ排出の猶予切れをタイムアウトとして返す `Output` を組み立てる。
///
/// `timeout_output` と違い、猶予までに読めた出力を捨てずに残す。失敗したフックの
/// 診断内容がそのまま手掛かりになるうえ、`is_timeout_output` は stderr の前方一致で
/// 判定するため、通知を先頭に置けばタイムアウト判定も従来どおり成立する。
fn drain_timeout_output(
    grace_secs: u64,
    command_desc: &str,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Output {
    let mut notice = timeout_notice(grace_secs, command_desc);
    notice.extend_from_slice(&stderr);
    Output {
        status: timeout_exit_status(),
        stdout,
        stderr: notice,
    }
}

/// タイムアウト付きでコマンドを実行し、タイムアウトメタデータを返す。
///
/// `Output` のみが必要な場合は `run_with_timeout` を使用する。
pub fn run_with_timeout_tracked(
    mut child: std::process::Child,
    timeout_secs: u64,
    command_desc: &str,
) -> Result<TimedOutput, String> {
    let deadline = match Instant::now().checked_add(Duration::from_secs(timeout_secs)) {
        Some(deadline) => deadline,
        None => {
            #[cfg(unix)]
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Invalid timeout {}s for command '{}': deadline is out of range",
                timeout_secs, command_desc
            ));
        }
    };

    // スレッドで読み取るためにstdout/stderrハンドルの所有権を取得
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // 読み取り結果は共有バッファへ逐次追記する。join できなかった場合でも
    // 親スレッドが「そこまでに読めた分」を回収できるようにするため。
    let stdout_buf: SharedOutput = Arc::default();
    let stderr_buf: SharedOutput = Arc::default();

    // スレッドでstdoutを読み取る（パイプバッファのデッドロックを防止）
    let stdout_thread = {
        let sink = Arc::clone(&stdout_buf);
        std::thread::spawn(move || {
            if let Some(stdout) = stdout_handle {
                read_output_bounded(stdout, &sink);
            }
        })
    };

    // スレッドでstderrを読み取る
    let stderr_thread = {
        let sink = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            if let Some(stderr) = stderr_handle {
                read_output_bounded(stderr, &sink);
            }
        })
    };

    // try_waitポーリングでタイムアウト付きの子プロセス待機
    let mut wait_backoff = POLL_BACKOFF_INITIAL;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) => {
                if Instant::now() >= deadline {
                    warn!(
                        "⏰ Command timed out after {}s: {}",
                        timeout_secs, command_desc
                    );
                    // Unix では子プロセスのプロセスグループ全体を SIGKILL する。
                    // `child.kill()` は直接の子のみを対象とするため、
                    // `sh -c 'sleep'` のような孫プロセスが孤児として残ってしまう。
                    // configure_unix_process_group で setpgid 済みのため
                    // 子の PID == プロセスグループID で killpg 可能。
                    #[cfg(unix)]
                    kill_process_group(child.id());
                    let _ = child.kill();
                    // ゾンビプロセスを回収
                    let _ = child.wait();
                    warn!("💀 Process killed (SIGKILL): {}", command_desc);
                    break (timeout_exit_status(), true);
                }
                wait_backoff = sleep_with_backoff(wait_backoff, deadline);
            }
            Err(e) => {
                return Err(format!(
                    "Failed to wait for command '{}': {}",
                    command_desc, e
                ));
            }
        }
    };

    if timed_out {
        // リーダースレッドをjoinしない：子サブプロセス（例：sh → sleep）が
        // パイプハンドルを保持し続ける可能性があり、join()が無期限にブロックされるため。
        // スレッドはプロセス終了時にクリーンアップされる。
        return Ok(TimedOutput {
            output: timeout_output(timeout_secs, command_desc),
            timed_out: true,
        });
    }

    // リーダースレッドから出力を収集する（正常終了後は通常すぐ完了する）。
    // 子は正常終了済みなので、残るのはパイプに残ったデータの読み切りだけ。
    // ここで元の deadline を流用すると、deadline 際どく正常終了したコマンドで
    // `Instant::now() >= deadline` が即真になり、まだ読み終えていない出力を
    // 捨てて誤って「タイムアウト」と判定してしまう（report=true の Stop フックで
    // 正常成功なのに誤った Block を返す）。そのため終了確定時点からの短い猶予
    // deadline を用いる。バックグラウンドの孫プロセスがパイプを保持し続けると
    // join は無期限ブロックするため、この猶予で諦めてプロセスグループを kill する。
    let drain_deadline = Instant::now()
        .checked_add(Duration::from_secs(OUTPUT_DRAIN_GRACE_SECS))
        .unwrap_or(deadline);
    let stdout_join = join_reader_before_deadline(stdout_thread, drain_deadline);
    let stderr_join = join_reader_before_deadline(stderr_thread, drain_deadline);

    // join を諦めた場合でも、共有バッファにはそこまでに読めた出力が入っている。
    // 子プロセスは既に終了しているので、子が書いた分はこの時点で読み切れており、
    // 以降パイプへ流れ込むのは孫プロセスの出力だけ（＝回収する必要がない）。
    let stdout = take_shared_output(&stdout_buf);
    let stderr = take_shared_output(&stderr_buf);

    if matches!(stdout_join, ReaderJoin::TimedOut) || matches!(stderr_join, ReaderJoin::TimedOut) {
        // 直接の子プロセスが正常終了していても、バックグラウンドの孫プロセスが
        // stdout/stderr を保持している限りパイプは EOF にならない。Unix では同じ
        // プロセスグループを停止して、`sh -c 'sleep ... &'` のようなプロセスリークと
        // timeout 回避を防ぐ（ここは判定結果によらず常に実施する）。
        #[cfg(unix)]
        kill_process_group(child.id());
        let _ = child.kill();
        let _ = child.wait();

        if status.success() {
            // フック本体は exit 0 で完了しており、出力も取得済み。孫プロセスが
            // パイプを握っていただけなので、これをタイムアウト扱いにすると
            // 「成功したフックが偽のブロックを返す」ことになる。Claude/Codex の Stop で
            // block は「拒否」ではなく「停止させず reason を継続プロンプトにする」意味なので、
            // 成功したフックのせいでエージェントが作業へ引き戻されてしまう。
            // よって読めた出力とともに正常終了として返す。
            warn!(
                "🧹 Killed background grandchild holding the output pipe after {}s (command succeeded): {}",
                OUTPUT_DRAIN_GRACE_SECS, command_desc
            );
            return Ok(TimedOutput {
                output: Output {
                    status,
                    stdout,
                    stderr,
                },
                timed_out: false,
            });
        }

        // 子が失敗終了した場合のみタイムアウト扱いにする。ここで待ったのは
        // drain 猶予であって hook_timeout ではないため、実際に待った秒数を通知に出す。
        warn!(
            "⏰ Command output pipe timed out after {}s: {}",
            OUTPUT_DRAIN_GRACE_SECS, command_desc
        );
        return Ok(TimedOutput {
            output: drain_timeout_output(OUTPUT_DRAIN_GRACE_SECS, command_desc, stdout, stderr),
            timed_out: true,
        });
    }

    Ok(TimedOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        timed_out: false,
    })
}

/// タイムアウト付きでコマンドを実行する。
///
/// パイプ接続されたstdout/stderrでコマンドを起動し、別スレッドで出力を読み取り
/// （パイプバッファのデッドロック防止）、子プロセスの終了またはデッドライン到達まで
/// `try_wait` でポーリングする。タイムアウト時は子プロセスをSIGKILLで強制終了し
/// 回収する。タイムアウト時の終了コードは124。
pub fn run_with_timeout(
    child: std::process::Child,
    timeout_secs: u64,
    command_desc: &str,
) -> Result<Output, String> {
    run_with_timeout_tracked(child, timeout_secs, command_desc).map(|result| result.output)
}

/// プログラムと引数から `Command` を組み立てる。
///
/// Windows では `cmd /c` を経由して `.cmd` / `.bat` のラッパー（例: `npx.cmd`）を解決する。
fn build_command(program: &str, args: &[String]) -> Command {
    if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    }
}

/// パイプ接続されたstdout/stderrと追加の環境変数でコマンドを起動する。
/// ストップフックがループ防止用の環境変数を子プロセスに伝播するために使用。
///
/// Unix では子プロセスを新しいプロセスグループに配置し、タイムアウト時に
/// プロセスグループ全体を確実に停止できるようにする。
pub fn spawn_piped_with_env(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<std::process::Child, String> {
    let mut cmd = build_command(program, args);
    // stdin は明示的に閉じる。継承したままだと、入力を読むコマンド
    // （`-m` なしの `git commit`、`cat` を含むパイプライン等）が端末からの入力を
    // 待ち続け、タイムアウトまでフック全体がハングする。閉じておけば即座に EOF を
    // 受け取って終了する。detached 側（`spawn_detached_with_env`）と挙動も揃う。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    configure_unix_process_group(&mut cmd);
    spawn_serialized(&mut cmd)
        .map_err(|e| format!("Failed to execute '{}': {}", program_label(program), e))
}

/// 標準入力へ `input` を流し込みつつ、stdout/stderr をパイプにしてコマンドを起動する。
///
/// command hooks の判定器の起動に使う（判定材料の JSON を stdin で渡す）。
/// 返した `Child` は `run_with_timeout_tracked` で待つ。
///
/// - `input` は別スレッドで書き込んでから stdin を閉じる。起動元のスレッドで書くと、
///   子が stdin を読まないまま stdout へパイプの容量以上を書いたときに互いの空きを
///   待って止まり、しかもその書き込みにはタイムアウトが効かない。書き込みスレッドは
///   join しない（子が終了・強制終了されれば書き込みは失敗して戻る）。
/// - 子が stdin を読まずに終了した場合の書き込みエラー（EPIPE）は無視する。
///   入力を読まずに判定を返す判定器もあり、起動の失敗ではない。
/// - `cwd` が `Some` ならそのディレクトリで起動する（`None` なら claw-hooks の cwd を継承）。
/// - Unix では新しいプロセスグループに置き、タイムアウト時に孫プロセスまで停止できるようにする。
pub fn spawn_piped_with_input(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    cwd: Option<&Path>,
    input: Vec<u8>,
) -> Result<Child, String> {
    let mut cmd = build_command(program, args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    configure_process_group(&mut cmd);
    let mut child = spawn_serialized(&mut cmd)
        .map_err(|e| format!("Failed to execute '{}': {}", program_label(program), e))?;

    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            if let Err(e) = stdin.write_all(&input)
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                // 入力の本文は残さず、失敗の種類だけを記録する。
                debug!("Failed to write stdin of a child process: {:?}", e.kind());
            }
            // ここで stdin が drop され、子は EOF を受け取る。
        });
    }
    Ok(child)
}

/// stdout/stderr/stdin を切り離してコマンドを起動する。
///
/// `report=false` の Stop フック用。親プロセスは子を待たないため、Hook 応答を
/// コマンド完了まで遅延させない。出力は破棄されるので、必要なログはコマンド側で
/// 明示的にファイル等へ書き出すこと。
///
/// `Child` を即時ドロップすると Rust の `std::process::Child` は `wait` を呼ばないため
/// Unix ではゾンビエントリが残り続ける。Stop フックが頻繁に発火する環境では親プロセス
/// （claw-hooks）の生存中に蓄積するため、専用スレッドで `wait` を回収する。
/// スレッドは子プロセス終了後に自然に終了し、親が先に終了した場合は OS が回収する。
pub fn spawn_detached_with_env(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<u32, String> {
    let mut cmd = build_command(program, args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    configure_unix_process_group(&mut cmd);
    spawn_serialized(&mut cmd)
        .map(|mut child| {
            let pid = child.id();
            // ゾンビを防ぐためバックグラウンドで wait する。
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            pid
        })
        .map_err(|e| format!("Failed to execute '{}': {}", program_label(program), e))
}

/// 任意の `Command` ビルダーに対して、Unix では新しいプロセスグループに配置する設定を施す。
/// `Command::new(...)` を直接組み立てるパス（例: extension hook）から再利用できる。
///
/// Windows ではノーオペレーション。`#[cfg(unix)]` 制約のない呼び出し側で安全に使えるよう
/// 公開する。
pub fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    configure_unix_process_group(cmd);
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_program_label_removes_unix_and_windows_directories() {
        assert_eq!(program_label("/Users/private/bin/tool"), "tool");
        assert_eq!(program_label(r"C:\\Users\\private\\tool.exe"), "tool.exe");
        assert_eq!(program_label("cargo"), "cargo");
        assert_eq!(program_label("/"), "<unknown>");
    }

    /// テストヘルパー：追加の環境変数なしでspawnする。
    fn spawn_piped(program: &str, args: &[String]) -> Result<std::process::Child, String> {
        spawn_piped_with_env(program, args, &[])
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

    /// テストヘルパー：`read_output_bounded` は共有バッファへ書き込む形になったため、
    /// 従来どおり「読み取り結果の Vec」で検証できるよう包む。
    fn read_to_vec(reader: impl Read) -> Vec<u8> {
        let sink: SharedOutput = Arc::default();
        read_output_bounded(reader, &sink);
        take_shared_output(&sink)
    }

    /// EINTR を 1 度返してから本来のデータを返すリーダー。
    struct InterruptOnceReader {
        interrupted: bool,
        data: Vec<u8>,
        pos: usize,
    }

    impl Read for InterruptOnceReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "signal",
                ));
            }
            let remaining = self.data.len() - self.pos;
            if remaining == 0 {
                return Ok(0);
            }
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn test_read_output_bounded_preserves_small_output() {
        let input = b"small output".to_vec();
        assert_eq!(read_to_vec(Cursor::new(&input)), input);
    }

    #[test]
    fn test_read_output_bounded_truncates_large_output() {
        let input = vec![b'x'; MAX_CAPTURED_OUTPUT_BYTES + 1];
        let output = read_to_vec(Cursor::new(input));

        assert_eq!(output.len(), MAX_CAPTURED_OUTPUT_BYTES);
        assert!(output.ends_with(OUTPUT_TRUNCATED_MARKER));
    }

    #[test]
    fn test_read_output_bounded_continues_after_interrupted() {
        // EINTR で打ち切るとパイプを排出しきれず、大量出力する子プロセスが
        // パイプ満杯でブロックして誤タイムアウトになる。中断後も読み続けること。
        let reader = InterruptOnceReader {
            interrupted: false,
            data: b"after-eintr".to_vec(),
            pos: 0,
        };
        assert_eq!(read_to_vec(reader), b"after-eintr".to_vec());
    }

    // === spawn_piped テスト ===

    #[test]
    fn test_spawn_piped_valid_command() {
        let child = spawn_piped("echo", &["hello".to_string()]);
        assert!(child.is_ok(), "Should spawn valid command");
        // 子プロセスを回収
        let _ = child.unwrap().wait();
    }

    // 存在しないプログラムが起動エラーになるのは Unix だけ。Windows は cmd /c 経由で起動するので
    // cmd の起動は成功し、失敗は終了コードで返る (下の Windows 用のテスト)
    #[cfg(unix)]
    #[test]
    fn test_spawn_piped_nonexistent_command() {
        let child = spawn_piped("nonexistent-command-xyz-abc-999", &[]);
        assert!(child.is_err(), "Should fail for nonexistent command");
        assert!(
            child.unwrap_err().contains("Failed to execute"),
            "Error should indicate execution failure"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_spawn_piped_nonexistent_command_fails_with_exit_code() {
        let child = spawn_piped("nonexistent-command-xyz-abc-999", &[])
            .expect("Windows は cmd /c の起動なので成功する");
        let output = run_with_timeout(child, 10, "nonexistent-command-xyz-abc-999").unwrap();
        assert!(
            !output.status.success(),
            "存在しないコマンドは失敗の終了コードで返るべき"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_piped_error_hides_program_directory() {
        let error = spawn_piped("/private/claw-hooks-secret/nonexistent-command", &[])
            .expect_err("存在しないコマンドは起動に失敗すべき");

        assert!(error.contains("nonexistent-command"));
        assert!(
            !error.contains("/private/claw-hooks-secret"),
            "実行ファイルのディレクトリをエラーへ含めるべきではない: {error}"
        );
    }

    // === run_with_timeout テスト ===

    #[test]
    fn test_run_with_timeout_captures_stdout() {
        let child = spawn_piped("echo", &["hello-stdout".to_string()]).unwrap();
        let output = run_with_timeout(child, 10, "echo hello-stdout").unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello-stdout"),
            "Should capture stdout, got: {}",
            stdout
        );
    }

    #[test]
    fn test_run_with_timeout_captures_stderr() {
        let child = spawn_piped(
            "sh",
            &["-c".to_string(), "echo hello-stderr >&2".to_string()],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "echo stderr").unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("hello-stderr"),
            "Should capture stderr, got: {}",
            stderr
        );
    }

    #[test]
    fn test_run_with_timeout_captures_both_stdout_and_stderr() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "echo out-data; echo err-data >&2".to_string(),
            ],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "both streams").unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("out-data"), "stdout: {}", stdout);
        assert!(stderr.contains("err-data"), "stderr: {}", stderr);
    }

    #[test]
    fn test_run_with_timeout_nonzero_exit_code() {
        let child = spawn_piped("sh", &["-c".to_string(), "exit 42".to_string()]).unwrap();
        let output = run_with_timeout(child, 10, "exit 42").unwrap();

        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(42));
    }

    #[test]
    fn test_run_with_timeout_kills_on_timeout() {
        let child = spawn_piped("sleep", &["30".to_string()]).unwrap();
        // 終了の確認 (kill -0) は Unix でだけ行う
        #[cfg(unix)]
        let pid = child.id();

        let start = Instant::now();
        let result = run_with_timeout(child, 1, "sleep 30");
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Timeout should return Ok");
        let output = result.unwrap();
        assert!(
            !output.status.success(),
            "Timeout should be treated as failure"
        );
        assert_eq!(
            output.status.code(),
            Some(TIMEOUT_EXIT_CODE),
            "Timeout should use exit code 124"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
        // タイムアウト時はstdoutが空（リーダースレッドはjoinされない）
        assert!(
            output.stdout.is_empty(),
            "Stdout should be empty on timeout"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout quickly, took {:?}",
            elapsed
        );
        assert!(is_timeout_output(&output));

        // /proc（Linux）またはkill -0でプロセスが実際に終了したことを確認
        #[cfg(unix)]
        {
            let check = Command::new("kill").args(["-0", &pid.to_string()]).output();
            if let Ok(output) = check {
                assert!(
                    !output.status.success(),
                    "Process {} should be dead after timeout kill",
                    pid
                );
            }
        }
    }

    #[test]
    fn test_run_with_timeout_large_output_no_deadlock() {
        // デッドロックが発生しないことを確認するために64KB超の出力を生成（典型的なパイプバッファサイズ）
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "dd if=/dev/zero bs=1024 count=128 2>/dev/null | tr '\\0' 'A'".to_string(),
            ],
        )
        .unwrap();

        let start = Instant::now();
        let output = run_with_timeout(child, 10, "large output");
        let elapsed = start.elapsed();

        assert!(output.is_ok(), "Should not deadlock on large output");
        assert!(
            elapsed.as_secs() < 10,
            "Should complete quickly, took {:?}",
            elapsed
        );
        let out = output.unwrap();
        assert!(
            out.stdout.len() >= 128 * 1024,
            "Should capture all output: {} bytes",
            out.stdout.len()
        );
    }

    #[test]
    fn test_run_with_timeout_fast_command_under_timeout() {
        let child = spawn_piped("true", &[]).unwrap();
        let start = Instant::now();
        let result = run_with_timeout(child, 60, "true");
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(result.unwrap().status.success());
        assert!(
            elapsed.as_secs() < 2,
            "Fast command should return quickly: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_run_with_timeout_rejects_unrepresentable_deadline() {
        let child = spawn_piped("sleep", &["30".to_string()]).unwrap();
        let result = run_with_timeout(child, u64::MAX, "sleep 30");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Invalid timeout"),
            "表現できない期限はエラーとして返すべき"
        );
    }

    // === spawn_piped_with_env テスト ===

    #[test]
    fn test_spawn_piped_with_env_passes_env_vars() {
        let child = spawn_piped_with_env(
            "sh",
            &["-c".to_string(), "echo $TEST_VAR_123".to_string()],
            &[("TEST_VAR_123", "hello_from_env")],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "env test").unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello_from_env"),
            "環境変数が子プロセスに渡されるべき: {}",
            stdout
        );
    }

    #[test]
    fn test_spawn_piped_with_env_multiple_vars() {
        let child = spawn_piped_with_env(
            "sh",
            &["-c".to_string(), "echo ${VAR_A}_${VAR_B}".to_string()],
            &[("VAR_A", "alpha"), ("VAR_B", "beta")],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "multi env test").unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("alpha_beta"),
            "複数の環境変数が渡されるべき: {}",
            stdout
        );
    }

    #[test]
    fn test_spawn_piped_with_env_empty_envs() {
        let child = spawn_piped_with_env("echo", &["no-env".to_string()], &[]).unwrap();
        let output = run_with_timeout(child, 10, "empty env").unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("no-env"));
    }

    #[test]
    fn test_spawn_detached_with_env_returns_without_waiting() {
        let marker = std::env::temp_dir().join(format!(
            "claw-hooks-detached-command-{}",
            std::process::id()
        ));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        let start = Instant::now();
        let pid = spawn_detached_with_env(
            "sh",
            &[
                "-c".to_string(),
                format!("sleep 1; echo detached > '{}'", marker_path),
            ],
            &[],
        )
        .unwrap();

        assert!(pid > 0);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "デタッチ起動は子プロセス完了を待たないべき"
        );
        assert!(!marker.exists());
        assert!(wait_for_path(&marker, Duration::from_secs(3)));
        let _ = std::fs::remove_file(marker);
    }

    // 起動エラーになるのは Unix だけ (Windows は cmd /c の起動が成功する。上の spawn_piped と同じ)
    #[cfg(unix)]
    #[test]
    fn test_spawn_detached_error_hides_program_directory() {
        let error = spawn_detached_with_env(
            "/private/claw-hooks-secret/nonexistent-detached-command",
            &[],
            &[],
        )
        .expect_err("存在しないデタッチコマンドは起動に失敗すべき");

        assert!(error.contains("nonexistent-detached-command"));
        assert!(
            !error.contains("/private/claw-hooks-secret"),
            "実行ファイルのディレクトリをエラーへ含めるべきではない: {error}"
        );
    }

    #[test]
    fn test_spawn_detached_with_env_passes_env_vars() {
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-detached-env-{}", std::process::id()));
        // シェルのリダイレクトはファイルを空で作ってから書き込むため、marker へ直接書かせると
        // 「存在する」を確認した直後に空のまま読むことがある (負荷の高い CI で発生した)。
        // 一時ファイルへ書き切ってから同一ディレクトリ内の mv (rename) で置き、
        // marker が見えた時点で中身が揃っているようにする。
        let partial = marker.with_extension("partial");
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let partial_path = partial.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&partial);

        spawn_detached_with_env(
            "sh",
            &[
                "-c".to_string(),
                format!(
                    "printf %s \"$DETACHED_VAR\" > '{partial_path}' && mv '{partial_path}' '{marker_path}'"
                ),
            ],
            &[("DETACHED_VAR", "detached-env-ok")],
        )
        .unwrap();

        assert!(wait_for_path(&marker, Duration::from_secs(3)));
        let content = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(content, "detached-env-ok");
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_detached_with_env_reaps_zombie() {
        // detach 起動でも `Child` を wait しないとゾンビが残る。バックグラウンドスレッドが
        // `wait` を呼ぶことで、子プロセス終了直後に PID エントリが回収されることを確認する。
        let pid =
            spawn_detached_with_env("sh", &["-c".to_string(), "exit 0".to_string()], &[]).unwrap();
        // 子プロセスが終了し、reaper スレッドが wait を完了するまで待つ。
        // `kill(pid, 0)` で存在確認するが、ゾンビは存在扱いなので回収後は ESRCH になる。
        let mut reaped = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            // 安全性: ここでは pid に対してシグナル 0 を送って存在確認するだけで、
            // プロセス自体には影響しない。
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if rc == -1 {
                let errno = std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or_default();
                if errno == libc::ESRCH {
                    reaped = true;
                    break;
                }
            }
        }
        assert!(reaped, "PID {pid} should be reaped (no zombie)");
    }

    // === run_with_timeout_tracked テスト ===

    #[test]
    fn test_run_with_timeout_tracked_success_not_timed_out() {
        let child = spawn_piped("echo", &["tracked".to_string()]).unwrap();
        let result = run_with_timeout_tracked(child, 10, "echo tracked").unwrap();
        assert!(!result.timed_out, "正常終了時は timed_out=false");
        assert!(result.output.status.success());
    }

    #[test]
    fn test_run_with_timeout_tracked_timeout_sets_flag() {
        let child = spawn_piped("sleep", &["30".to_string()]).unwrap();
        let result = run_with_timeout_tracked(child, 1, "sleep 30").unwrap();
        assert!(result.timed_out, "タイムアウト時は timed_out=true");
        assert_eq!(result.output.status.code(), Some(TIMEOUT_EXIT_CODE));
    }

    #[test]
    fn test_run_with_timeout_nonzero_exit_preserves_stderr() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "echo error-detail >&2; exit 1".to_string(),
            ],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "exit 1 with stderr").unwrap();
        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("error-detail"),
            "非ゼロ終了時もstderrをキャプチャすべき: {}",
            stderr
        );
    }

    #[test]
    fn test_is_timeout_output_false_for_normal_exit_124() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "echo normal-error >&2; exit 124".to_string(),
            ],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "exit 124").unwrap();
        assert!(!is_timeout_output(&output));
        assert_eq!(output.status.code(), Some(TIMEOUT_EXIT_CODE));
    }

    /// Unix で `sh -c 'sleep'` のような孫プロセスがタイムアウト時に
    /// 確実に停止することを確認する。
    /// プロセスグループ kill が機能していなければ、`sh` だけ kill されて
    /// `sleep` が孤児プロセスとして残ってしまう。
    #[cfg(unix)]
    #[test]
    fn test_run_with_timeout_kills_grandchild_process() {
        // 一時マーカーファイルを設定: sleep が完走したらマーカーを書き込む。
        // タイムアウトで sleep が殺されればマーカーは書き込まれない。
        let marker = std::env::temp_dir().join(format!(
            "claw-hooks-grandchild-marker-{}",
            std::process::id()
        ));
        let marker_path = marker.to_str().unwrap();
        // 既存マーカーを掃除
        let _ = std::fs::remove_file(&marker);

        // 30秒スリープ後にマーカー作成。1秒タイムアウトで kill。
        let cmd = format!("sleep 30 && touch {}", marker_path);
        let child = spawn_piped("sh", &["-c".to_string(), cmd.clone()]).unwrap();
        let result = run_with_timeout(child, 1, &cmd);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status.code(), Some(TIMEOUT_EXIT_CODE));

        // killpg が孫プロセスを停止できたなら、しばらく待ってもマーカーは作成されない。
        // 念のため余裕を持って待機する。
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !marker.exists(),
            "孫プロセスのスリープがタイムアウトで停止しなかった: {} が残存",
            marker_path
        );
    }

    /// 直接の子プロセスが正常終了しても、バックグラウンドの孫プロセスが
    /// stdout/stderr のパイプを保持している場合は timeout として扱い、
    /// プロセスグループ全体を停止する。
    #[cfg(unix)]
    #[test]
    fn test_run_with_timeout_kills_background_grandchild_after_parent_exit() {
        let marker = std::env::temp_dir().join(format!(
            "claw-hooks-background-grandchild-marker-{}",
            std::process::id()
        ));
        let marker_path = marker.to_str().unwrap();
        let _ = std::fs::remove_file(&marker);

        // シェル自体はすぐ終了するが、バックグラウンドの sleep は stdout/stderr を
        // 継承しているため、プロセスグループを止めないとプロセスがリークする。
        let cmd = format!("sleep 30 && touch {} &", marker_path);
        let child = spawn_piped("sh", &["-c".to_string(), cmd.clone()]).unwrap();
        let result = run_with_timeout_tracked(child, 1, &cmd).unwrap();

        // 期待値の変更（旧: timed_out=true / exit 124）:
        // 旧実装はこのケースをタイムアウト扱いにしていたが、シェル自体は exit 0 で
        // 成功しており、これを Stop フックで返すと `decision:"block"` = 継続プロンプトになり、
        // 成功したフックのせいでエージェントが作業へ引き戻される（偽ブロック）。
        // このテストが本来固定したい不変条件は「孫プロセスを確実に kill すること」なので、
        // 判定は成功のまま、孫の停止だけを検証する。
        assert!(
            !result.timed_out,
            "成功した子プロセスを偽タイムアウトにしない"
        );
        assert!(result.output.status.success());

        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !marker.exists(),
            "バックグラウンド孫プロセスがタイムアウト後も動作した: {} が作成された",
            marker_path
        );
    }

    /// 孫プロセスがパイプを保持していても、正常終了した子プロセスの出力は
    /// 捨てずに返す。捨てると Stop フックの lint 結果がエージェントへ届かない。
    #[cfg(unix)]
    #[test]
    fn test_run_with_timeout_preserves_output_when_grandchild_holds_pipe() {
        let cmd = "echo IMPORTANT-LINT-RESULT; (sleep 20 &)";
        let child = spawn_piped("sh", &["-c".to_string(), cmd.to_string()]).unwrap();
        let result = run_with_timeout_tracked(child, 60, cmd).unwrap();

        assert!(!result.timed_out, "成功したフックを偽タイムアウトにしない");
        assert!(result.output.status.success());
        let stdout = String::from_utf8_lossy(&result.output.stdout);
        assert!(
            stdout.contains("IMPORTANT-LINT-RESULT"),
            "取得済みの出力を捨てるべきではない: {}",
            stdout
        );
    }

    /// 子プロセスが失敗終了した場合は従来どおりタイムアウト扱いにするが、
    /// 通知の秒数は hook_timeout ではなく実際に待った drain 猶予にする。
    #[cfg(unix)]
    #[test]
    fn test_run_with_timeout_drain_timeout_reports_grace_and_keeps_output() {
        let cmd = "echo partial-output; (sleep 20 &); exit 1";
        let child = spawn_piped("sh", &["-c".to_string(), cmd.to_string()]).unwrap();
        // hook_timeout は 60s だが、実際に待つのは drain 猶予だけ。
        let result = run_with_timeout_tracked(child, 60, cmd).unwrap();

        assert!(result.timed_out, "失敗終了 + パイプ保持はタイムアウト扱い");
        assert_eq!(result.output.status.code(), Some(TIMEOUT_EXIT_CODE));
        assert!(is_timeout_output(&result.output));

        let stderr = String::from_utf8_lossy(&result.output.stderr);
        assert!(
            stderr.contains(&format!("{}s", OUTPUT_DRAIN_GRACE_SECS)),
            "実際に待った drain 猶予の秒数を通知すべき: {}",
            stderr
        );
        assert!(
            !stderr.contains("60s"),
            "待っていない hook_timeout の秒数を出すべきではない: {}",
            stderr
        );
        let stdout = String::from_utf8_lossy(&result.output.stdout);
        assert!(
            stdout.contains("partial-output"),
            "猶予までに読めた出力は診断のために残すべき: {}",
            stdout
        );
    }

    /// 入力を読むコマンドは stdin を継承していると端末待ちでハングする。
    /// `Stdio::null()` を指定していれば即座に EOF を受け取って終了する。
    #[test]
    fn test_spawn_piped_closes_stdin() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "cat >/dev/null; echo STDIN-EOF".to_string(),
            ],
        )
        .unwrap();
        let start = Instant::now();
        let output = run_with_timeout(child, 10, "cat").unwrap();
        let elapsed = start.elapsed();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("STDIN-EOF"), "stdout: {}", stdout);
        assert!(
            elapsed < Duration::from_secs(5),
            "stdin を閉じていれば入力待ちでハングしない: {:?}",
            elapsed
        );
    }

    // === spawn_piped_with_input テスト ===

    #[cfg(unix)]
    #[test]
    fn test_spawn_piped_with_input_delivers_input_then_eof() {
        // cat は EOF まで読むので、stdin を閉じていなければタイムアウトまで終わらない
        let child = spawn_piped_with_input("cat", &[], &[], None, b"{\"a\":1}\n".to_vec()).unwrap();
        let result = run_with_timeout_tracked(child, 10, "cat").unwrap();

        assert!(!result.timed_out, "入力を書き終えたら stdin を閉じるべき");
        assert!(result.output.status.success());
        assert_eq!(result.output.stdout, b"{\"a\":1}\n");
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_piped_with_input_ignores_unread_input() {
        // パイプの容量を大きく超える入力を読まずに終了しても、失敗にもハングにもしない
        let input = vec![b'x'; 1024 * 1024];
        let child = spawn_piped_with_input(
            "sh",
            &["-c".to_string(), "echo done".to_string()],
            &[],
            None,
            input,
        )
        .unwrap();
        let start = Instant::now();
        let result = run_with_timeout_tracked(child, 10, "sh").unwrap();

        assert!(!result.timed_out);
        assert!(result.output.status.success());
        assert_eq!(result.output.stdout, b"done\n");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_piped_with_input_applies_cwd_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let child = spawn_piped_with_input(
            "sh",
            &[
                "-c".to_string(),
                "pwd -P; printf %s \"$INPUT_TEST_VAR\"".to_string(),
            ],
            &[("INPUT_TEST_VAR", "from-env")],
            Some(dir.path()),
            Vec::new(),
        )
        .unwrap();
        let result = run_with_timeout_tracked(child, 10, "sh").unwrap();

        let stdout = String::from_utf8_lossy(&result.output.stdout);
        let mut lines = stdout.lines();
        let expected_dir = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(lines.next(), Some(expected_dir.to_str().unwrap()));
        assert_eq!(lines.next(), Some("from-env"));
    }

    #[cfg(unix)]
    #[test]
    fn test_spawn_piped_with_input_error_hides_program_directory() {
        let error = spawn_piped_with_input(
            "/private/claw-hooks-secret/nonexistent-judge",
            &[],
            &[],
            None,
            Vec::new(),
        )
        .expect_err("存在しない判定器は起動に失敗すべき");

        assert!(error.contains("nonexistent-judge"));
        assert!(
            !error.contains("/private/claw-hooks-secret"),
            "実行ファイルのディレクトリをエラーへ含めるべきではない: {error}"
        );
    }

    /// 固定 100ms スリープのポーリングを指数バックオフへ置き換えたことの回帰テスト。
    /// 旧実装は即終了するコマンドでも最低 100ms（`try_wait` の固定スリープ）待っていた。
    /// 90ms は「旧実装の下限を確実に下回る」かつ「実測 2〜7ms に対して十分な余裕がある」値。
    #[test]
    fn test_run_with_timeout_returns_without_fixed_poll_delay() {
        let child = spawn_piped("true", &[]).unwrap();
        let start = Instant::now();
        let result = run_with_timeout(child, 60, "true").unwrap();
        let elapsed = start.elapsed();

        assert!(result.status.success());
        assert!(
            elapsed < Duration::from_millis(90),
            "即終了するコマンドで固定ポーリング遅延を払うべきではない: {:?}",
            elapsed
        );
    }
}
