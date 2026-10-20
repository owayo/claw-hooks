//! 祖先プロセスの情報（親の PID と argv）を読む。
//!
//! Claude Code のチームメイトの判定（`service::teammate_probe`）が使う。split-pane の
//! チームメイトは別プロセスの claude として起動され、その argv にチームの識別子が並ぶため、
//! フックの祖先をたどって argv を読めば、Stop を出したセッションがチームメイトかを見分けられる。
//!
//! - 読むのは自分と同じ実効 UID のプロセスだけ。ほかのユーザーのプロセスに着いたら
//!   `ReadError::OtherUser` を返し、呼び出し側はそこを探索の終わりとして扱う
//!   （チームメイトの claude は、必ずフックと同じユーザーで動いている）
//! - argv の読み取りは呼び出し側が渡す予算（バイト）までに抑え、超えるものは
//!   `ReadError::TooLarge` にする（切り詰めた argv では判定しない）
//! - argv を読む前後で親の PID・実効 UID・起動時刻を取り直し、食い違えば（PID が別の
//!   プロセスに再利用された・親が変わった）`ReadError::Changed` にする。所有者の確認も
//!   この取り直しの内側で行い、確認の直後に PID がほかのユーザーのプロセスへ再利用されても、
//!   そのプロセスの argv を返さない
//! - 対応するのは macOS（`proc_pidinfo` と `sysctl(KERN_PROCARGS2)`）と Linux（`/proc`）。
//!   ほかの OS では常に `ReadError::Unsupported` を返す
//!
//! argv は対象のプロセス自身が書き換えられる（Linux の `/proc/<pid>/cmdline` も macOS の
//! `KERN_PROCARGS2` も、そのプロセスのメモリを読む）。ここで読んだ値は権限の判断には使わない。

/// 1 プロセスで読む argv の領域の大きさの上限（バイト）。macOS の `KERN_PROCARGS2` は
/// 環境変数も含むため、引数と環境変数の合計の上限（`ARG_MAX`。macOS は 1 MiB）より大きくとる。
pub const MAX_ARGS_BYTES: usize = 2 * 1024 * 1024;

/// 1 つのプロセスについて読み取った情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    /// 親プロセスの PID。
    pub ppid: u32,
    /// argv（argv[0] を含む）。各要素は NUL を除いたバイト列で、UTF-8 とは限らない。
    pub args: Vec<Vec<u8>>,
    /// argv を取り出すために OS から読んだバイト数（macOS は環境変数の分も含む）。
    /// 走査全体の読み取り量の予算に使う。
    pub read_bytes: usize,
}

/// プロセスの情報を読めなかった理由。
///
/// macOS と Linux 以外では `Unsupported` しか返さないため、ほかの値はそこで使われない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub enum ReadError {
    /// ほかのユーザーのプロセス。argv は読まない。
    OtherUser,
    /// プロセスが存在しない（読む前に終わった）。
    Gone,
    /// 読んでいる間に親の PID・実効 UID・起動時刻が変わった（PID の再利用など）。
    Changed,
    /// argv の領域が予算を超える。
    TooLarge,
    /// 権限・形式などで読めない。
    Unreadable,
    /// この OS では読めない。
    Unsupported,
}

/// プロセスの情報を読む手段。テストでは偽のプロセス表を渡す。
pub trait ProcessTable {
    /// `pid` のプロセスの親の PID と argv を読む。argv の領域が `budget` バイトを超えるときは
    /// 読まずに `ReadError::TooLarge` を返す。
    fn read(&self, pid: u32, budget: usize) -> Result<ProcessRecord, ReadError>;
}

/// OS からプロセスの情報を読む。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessTable;

impl ProcessTable for SystemProcessTable {
    fn read(&self, pid: u32, budget: usize) -> Result<ProcessRecord, ReadError> {
        imp::read(pid, budget)
    }
}

/// argv を読む前後で取り直して比べる値。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    ppid: u32,
    /// 実効 UID。
    uid: u32,
    start_time: (u64, u64),
}

/// 前後で取った `Identity` が一致し、実効 UID が `euid` のときだけ、argv をそのプロセスの
/// ものとして返す。`args` は argv と、OS から読んだバイト数を返す。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_consistent(
    euid: u32,
    identity: impl Fn() -> Result<Identity, ReadError>,
    args: impl FnOnce() -> Result<(Vec<Vec<u8>>, usize), ReadError>,
) -> Result<ProcessRecord, ReadError> {
    let before = identity()?;
    if before.uid != euid {
        return Err(ReadError::OtherUser);
    }
    let (args, read_bytes) = args()?;
    let after = identity()?;
    if before != after {
        return Err(ReadError::Changed);
    }
    Ok(ProcessRecord {
        ppid: before.ppid,
        args,
        read_bytes,
    })
}

/// 自分の実効 UID。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn effective_uid() -> u32 {
    // SAFETY: geteuid は引数を取らず、失敗しない。
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Identity, ProcessRecord, ReadError, effective_uid, read_consistent};
    use std::mem::MaybeUninit;

    pub(super) fn read(pid: u32, budget: usize) -> Result<ProcessRecord, ReadError> {
        let pid = libc::c_int::try_from(pid).map_err(|_| ReadError::Unreadable)?;
        read_consistent(effective_uid(), || identity(pid), || args(pid, budget))
    }

    /// `proc_pidinfo` の失敗を `ReadError` にする（失敗すると 0 を返し、errno を設定する）。
    fn pidinfo_error(written: libc::c_int) -> ReadError {
        if written != 0 {
            // 構造体の一部だけが書かれた（起きない想定）。errno は当てにならない。
            return ReadError::Unreadable;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => ReadError::Gone,
            _ => ReadError::Unreadable,
        }
    }

    /// `proc_pidinfo` で `T` の大きさの情報を読む。
    ///
    /// # Safety
    ///
    /// `flavor` は、`T` の大きさの構造体を書く `proc_pidinfo` の種類でなければならない。
    unsafe fn pidinfo<T>(pid: libc::c_int, flavor: libc::c_int) -> Result<T, ReadError> {
        let mut info = MaybeUninit::<T>::zeroed();
        let size = std::mem::size_of::<T>() as libc::c_int;
        // SAFETY: `info` は `size` バイトの書き込み先で、`proc_pidinfo` は最大 `size` バイトを書く。
        let written = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
        if written != size {
            return Err(pidinfo_error(written));
        }
        // SAFETY: `proc_pidinfo` が構造体の全体（`size` バイト）を書いた。
        Ok(unsafe { info.assume_init() })
    }

    fn identity(pid: libc::c_int) -> Result<Identity, ReadError> {
        // SAFETY: `PROC_PIDTBSDINFO` は `proc_bsdinfo` を書く。
        match unsafe { pidinfo::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO) } {
            Ok(info) => Ok(Identity {
                ppid: info.pbi_ppid,
                uid: info.pbi_uid,
                start_time: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            }),
            Err(ReadError::Unreadable) => {
                // ほかのユーザーのプロセスは `PROC_PIDTBSDINFO` を読めない（EPERM）。
                // 所有者だけは `PROC_PIDT_SHORTBSDINFO` で読めるので、それで見分ける。
                // SAFETY: `PROC_PIDT_SHORTBSDINFO` は `proc_bsdshortinfo` を書く。
                let short = unsafe {
                    pidinfo::<libc::proc_bsdshortinfo>(pid, libc::PROC_PIDT_SHORTBSDINFO)
                }?;
                if short.pbsi_uid != effective_uid() {
                    Err(ReadError::OtherUser)
                } else {
                    Err(ReadError::Unreadable)
                }
            }
            Err(error) => Err(error),
        }
    }

    fn args(pid: libc::c_int, budget: usize) -> Result<(Vec<Vec<u8>>, usize), ReadError> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: 書き込み先を渡さない呼び出しは、必要な大きさを `size` に書くだけ。
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 || size == 0 {
            return Err(ReadError::Unreadable);
        }
        if size > budget {
            return Err(ReadError::TooLarge);
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` は `size` バイトの書き込み先で、`sysctl` は最大 `size` バイトを書き、
        // 書いた大きさを `size` に返す。
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            return Err(ReadError::Unreadable);
        }
        buf.truncate(size);
        let args = parse_procargs2(&buf).ok_or(ReadError::Unreadable)?;
        Ok((args, buf.len()))
    }

    /// `KERN_PROCARGS2` の結果から argv を取り出す。
    ///
    /// 並びは `argc`（`int`）、実行ファイルのパスと NUL、境界合わせの NUL、`argc` 個の
    /// NUL 終端の argv、環境変数。環境変数まで読まないよう、`argc` 個だけを取り出す。
    /// 実行ファイルのパスの後の NUL は境界合わせと区別できないため、空の argv[0] は
    /// 読み飛ばしてしまう（`ps` と同じ制約）。
    pub(super) fn parse_procargs2(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
        let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
        let rest = &buf[4..];
        let mut pos = rest.iter().position(|&b| b == 0)?;
        while rest.get(pos) == Some(&0) {
            pos += 1;
        }
        let mut args = Vec::with_capacity(argc.min(1024));
        for _ in 0..argc {
            let tail = rest.get(pos..)?;
            let len = tail.iter().position(|&b| b == 0)?;
            args.push(tail[..len].to_vec());
            pos += len + 1;
        }
        Some(args)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Identity, ProcessRecord, ReadError, effective_uid, read_consistent};
    use std::io::{ErrorKind, Read};

    /// `/proc/<pid>/stat` と `/proc/<pid>/status` の大きさの上限（どちらも通常は数 KiB）。
    const MAX_PROC_FILE_BYTES: usize = 64 * 1024;

    pub(super) fn read(pid: u32, budget: usize) -> Result<ProcessRecord, ReadError> {
        read_consistent(effective_uid(), || identity(pid), || args(pid, budget))
    }

    fn io_error(error: std::io::Error) -> ReadError {
        match error.kind() {
            ErrorKind::NotFound => ReadError::Gone,
            _ => ReadError::Unreadable,
        }
    }

    /// `limit` バイトを超えるファイルは `TooLarge` にする（切り詰めた内容で判定しない）。
    fn read_limited(path: &str, limit: usize) -> Result<Vec<u8>, ReadError> {
        let file = std::fs::File::open(path).map_err(io_error)?;
        let mut contents = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut contents)
            .map_err(io_error)?;
        if contents.len() > limit {
            return Err(ReadError::TooLarge);
        }
        Ok(contents)
    }

    fn identity(pid: u32) -> Result<Identity, ReadError> {
        let stat = read_limited(&format!("/proc/{pid}/stat"), MAX_PROC_FILE_BYTES)?;
        let (ppid, start_time) = parse_stat(&stat).ok_or(ReadError::Unreadable)?;
        // `/proc/<pid>` の所有者は、dump できないプロセスでは同じユーザーでも root になる。
        // 実効 UID は `status` の `Uid:` 行から読む。
        let status = read_limited(&format!("/proc/{pid}/status"), MAX_PROC_FILE_BYTES)?;
        let uid = parse_status_euid(&status).ok_or(ReadError::Unreadable)?;
        Ok(Identity {
            ppid,
            uid,
            start_time: (start_time, 0),
        })
    }

    fn args(pid: u32, budget: usize) -> Result<(Vec<Vec<u8>>, usize), ReadError> {
        let cmdline = read_limited(&format!("/proc/{pid}/cmdline"), budget)?;
        let args = parse_cmdline(&cmdline).ok_or(ReadError::Unreadable)?;
        Ok((args, cmdline.len()))
    }

    /// `/proc/<pid>/stat` から親の PID（4 番目の項目）と起動時刻（22 番目）を取り出す。
    ///
    /// 2 番目の項目（`(comm)`）は空白や `)` を含み得るため、最後の `)` より後だけを
    /// 空白で区切る。
    pub(super) fn parse_stat(stat: &[u8]) -> Option<(u32, u64)> {
        let close = stat.iter().rposition(|&b| b == b')')?;
        let rest = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
        let mut fields = rest.split_ascii_whitespace();
        // 3 番目（状態）を読み飛ばし、4 番目が親の PID。
        let ppid = fields.nth(1)?.parse().ok()?;
        // 5 番目から数えて 18 個先（22 番目）が起動時刻（起動からのクロック数）。
        let start_time = fields.nth(17)?.parse().ok()?;
        Some((ppid, start_time))
    }

    /// `/proc/<pid>/status` の `Uid:` 行（実・実効・保存・ファイルシステムの順）から
    /// 実効 UID を取り出す。
    pub(super) fn parse_status_euid(status: &[u8]) -> Option<u32> {
        let status = std::str::from_utf8(status).ok()?;
        let line = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
        line.split_ascii_whitespace().nth(1)?.parse().ok()
    }

    /// `/proc/<pid>/cmdline`（NUL 区切りで、末尾も NUL）から argv を取り出す。
    ///
    /// 空のとき（終了したプロセス・カーネルスレッド）と、末尾が NUL でないとき（プロセスが
    /// 自分の argv を書き換えて形が崩れた）は、読めない扱いにする。
    pub(super) fn parse_cmdline(cmdline: &[u8]) -> Option<Vec<Vec<u8>>> {
        let body = cmdline.strip_suffix(&[0])?;
        Some(body.split(|&b| b == 0).map(<[u8]>::to_vec).collect())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{ProcessRecord, ReadError};

    pub(super) fn read(_pid: u32, _budget: usize) -> Result<ProcessRecord, ReadError> {
        Err(ReadError::Unsupported)
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// どの OS にも存在しない PID（macOS の上限は 99999、Linux の `pid_max` は 2^22 まで）。
    const MISSING_PID: u32 = 0x7fff_fff0;

    const EUID: u32 = 501;

    fn identity(ppid: u32, uid: u32, start: u64) -> Identity {
        Identity {
            ppid,
            uid,
            start_time: (start, 0),
        }
    }

    /// 呼ばれるたびに `identities` を順に返す `identity` と、argv を読んだかの記録。
    fn read_with(identities: &[Identity]) -> (Result<ProcessRecord, ReadError>, bool) {
        let calls = Cell::new(0);
        let read_args = Cell::new(false);
        let result = read_consistent(
            EUID,
            || {
                let index = calls.get();
                calls.set(index + 1);
                Ok(identities[index.min(identities.len() - 1)])
            },
            || {
                read_args.set(true);
                Ok((vec![b"claude".to_vec()], 7))
            },
        );
        (result, read_args.get())
    }

    /// 自分自身の argv と親の PID は、標準ライブラリが返すものと一致する。
    #[test]
    fn test_reads_own_process() {
        use std::os::unix::ffi::OsStrExt;

        let record = SystemProcessTable
            .read(std::process::id(), MAX_ARGS_BYTES)
            .expect("own process must be readable");
        let expected: Vec<Vec<u8>> = std::env::args_os()
            .map(|arg| arg.as_bytes().to_vec())
            .collect();
        assert_eq!(record.args, expected);
        assert_eq!(record.ppid, std::os::unix::process::parent_id());
        assert!(record.read_bytes > 0);
    }

    #[test]
    fn test_own_process_over_budget_is_too_large() {
        assert_eq!(
            SystemProcessTable.read(std::process::id(), 1),
            Err(ReadError::TooLarge)
        );
    }

    #[test]
    fn test_missing_process_is_gone() {
        assert_eq!(
            SystemProcessTable.read(MISSING_PID, MAX_ARGS_BYTES),
            Err(ReadError::Gone)
        );
    }

    #[test]
    fn test_consistent_identity_is_accepted() {
        let (result, read_args) = read_with(&[identity(7, EUID, 5)]);
        assert_eq!(
            result,
            Ok(ProcessRecord {
                ppid: 7,
                args: vec![b"claude".to_vec()],
                read_bytes: 7,
            })
        );
        assert!(read_args);
    }

    #[test]
    fn test_other_user_is_rejected_before_reading_args() {
        let (result, read_args) = read_with(&[identity(7, 0, 5)]);
        assert_eq!(result, Err(ReadError::OtherUser));
        assert!(!read_args, "ほかのユーザーのプロセスの argv は読まない");
    }

    #[test]
    fn test_changed_identity_is_rejected() {
        // 起動時刻が変わった（PID の再利用）
        let (result, _) = read_with(&[identity(7, EUID, 5), identity(7, EUID, 6)]);
        assert_eq!(result, Err(ReadError::Changed));
        // 親が変わった（親が終わって init に付け替えられた）
        let (result, _) = read_with(&[identity(7, EUID, 5), identity(1, EUID, 5)]);
        assert_eq!(result, Err(ReadError::Changed));
        // 所有者が変わった（ほかのユーザーのプロセスに再利用された）
        let (result, _) = read_with(&[identity(7, EUID, 5), identity(7, 0, 5)]);
        assert_eq!(result, Err(ReadError::Changed));
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::super::imp::parse_procargs2;

        fn procargs(argc: i32, body: &[u8]) -> Vec<u8> {
            let mut buf = argc.to_ne_bytes().to_vec();
            buf.extend_from_slice(body);
            buf
        }

        #[test]
        fn test_parse_procargs2_reads_only_argc_entries() {
            let buf = procargs(
                3,
                b"/usr/local/bin/claude\0\0\0\0claude\0--agent-id\0\0HOME=/x\0PATH=/y\0",
            );
            assert_eq!(
                parse_procargs2(&buf),
                Some(vec![b"claude".to_vec(), b"--agent-id".to_vec(), Vec::new()])
            );
        }

        #[test]
        fn test_parse_procargs2_rejects_missing_terminator() {
            let buf = procargs(2, b"/bin/sh\0sh\0-c");
            assert_eq!(parse_procargs2(&buf), None);
        }

        #[test]
        fn test_parse_procargs2_rejects_short_buffer() {
            assert_eq!(parse_procargs2(&[1, 0]), None);
            assert_eq!(parse_procargs2(&procargs(-1, b"/bin/sh\0sh\0")), None);
        }
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::imp::{parse_cmdline, parse_stat, parse_status_euid};

        #[test]
        fn test_parse_stat_handles_parentheses_in_comm() {
            let stat = b"1234 (a) b (c)) S 77 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10";
            assert_eq!(parse_stat(stat), Some((77, 98765)));
        }

        #[test]
        fn test_parse_stat_rejects_truncated_input() {
            assert!(parse_stat(b"1234 (sh) S 77 1234").is_none());
            assert!(parse_stat(b"1234 sh S 77").is_none());
        }

        #[test]
        fn test_parse_status_euid_reads_effective_uid() {
            let status = b"Name:\tclaude\nUmask:\t0022\nState:\tS (sleeping)\nPPid:\t77\nUid:\t1000\t1001\t1002\t1003\nGid:\t1000\t1000\t1000\t1000\n";
            assert_eq!(parse_status_euid(status), Some(1001));
            assert_eq!(parse_status_euid(b"Name:\tclaude\nPPid:\t77\n"), None);
        }

        #[test]
        fn test_parse_cmdline_keeps_empty_arguments() {
            assert_eq!(
                parse_cmdline(b"sh\0-c\0\0x\0"),
                Some(vec![
                    b"sh".to_vec(),
                    b"-c".to_vec(),
                    Vec::new(),
                    b"x".to_vec()
                ])
            );
        }

        #[test]
        fn test_parse_cmdline_rejects_empty_or_unterminated() {
            assert_eq!(parse_cmdline(b""), None);
            assert_eq!(parse_cmdline(b"claude\0--team-name=t"), None);
        }
    }
}
