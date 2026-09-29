//! フック呼び出しをまたいで残す小さな状態の置き場所。
//!
//! claw-hooks は 1 イベント = 1 プロセスで動くため、「同じセッションで一度だけ」知らせる通知や、
//! Stop フックの再試行の予定は、プロセスの外に残さないと次の呼び出しへ引き継げない。
//!
//! ここに置くのは、失われても実害の小さい状態に限る。通知の記録が消えれば通知が 1 回増え、
//! 再試行の予定が消えれば再試行が 1 回減る（後続の stage は次の停止まで持ち越される）だけで、
//! どちらも判定を緩める方向には働かない。
//!
//! - 置き場所: `dirs::cache_dir()/claw-hooks/<kind>/`（ユーザーごと。リポジトリの中には置かない）
//! - ファイル名: キーを FNV-1a 64 で 16 進にしたもの。セッション ID・パス・コマンドは生で残さない
//! - ディレクトリがシンボリックリンク・通常のディレクトリでない・自分以外の所有・グループや
//!   他人が書き込める、のどれかなら「使えない」扱いにする。呼び出し側は知らせる側
//!   （通知を出す・再試行を予定しない）に倒す

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing::{debug, warn};

/// 1 回の掃除で調べるエントリの上限。ディレクトリが異常に膨らんでいても、フック 1 回の
/// 待ち時間を掃除で延ばさない。
const PRUNE_SCAN_LIMIT: usize = 1000;

/// キーの長さ（16 進 16 桁）。掃除の対象をこの形の名前に限り、手で置かれたファイルを消さない。
const KEY_HEX_LEN: usize = 16;

/// `mark_once` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkOutcome {
    /// 今回初めて記録した。
    First,
    /// 既に記録があった。
    AlreadyMarked,
    /// 状態を置けない（キャッシュディレクトリが無い・異常・書き込めない）。
    Unavailable,
}

/// フック呼び出しをまたいで残す状態の置き場所。
#[derive(Debug, Clone)]
pub struct StateStore {
    root: Option<PathBuf>,
}

impl StateStore {
    /// ユーザーのキャッシュディレクトリ（`dirs::cache_dir()/claw-hooks`）を使う。
    ///
    /// ユニットテストでは本物のキャッシュディレクトリへ書かないよう、常に無効の置き場所を返す
    /// （状態を使うテストは `StateStore::at` で一時ディレクトリを渡す）。
    pub fn for_user() -> Self {
        if cfg!(test) {
            return Self::disabled();
        }
        Self {
            root: dirs::cache_dir().map(|dir| dir.join("claw-hooks")),
        }
    }

    /// 指定したディレクトリを置き場所にする（テスト用）。
    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    /// 状態を一切残さない置き場所。記録は常に `Unavailable`、読み出しは常に `None` になる。
    pub fn disabled() -> Self {
        Self { root: None }
    }

    /// キーの各要素を長さ付きで連結し、FNV-1a 64 で 16 進 16 桁にする。
    ///
    /// 長さを前置するので `["ab", "c"]` と `["a", "bc"]` は別のキーになる。
    /// 暗号学的な強度は要らない（衝突しても通知が 1 回減るか、再試行の予定が別セッションの
    /// ものと入れ替わる確率が 2^-64 程度あるだけ）ため、依存を増やさず自前で計算する。
    pub fn key(parts: &[&str]) -> String {
        const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET_BASIS;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(PRIME);
            }
        };
        for part in parts {
            feed(&(part.len() as u64).to_le_bytes());
            feed(part.as_bytes());
        }
        format!("{:016x}", hash)
    }

    /// `kind` のディレクトリにキーの記録を作る。同じキーは 1 回目だけ `First` を返す。
    ///
    /// 複数のフックプロセスが同時に呼んでも、`create_new`（O_CREAT | O_EXCL）により
    /// `First` を受け取るのは 1 つだけになる。新しく記録したときだけ、`max_age` より古い
    /// 記録を掃除する。
    pub fn mark_once(&self, kind: &str, key: &str, max_age: Duration) -> MarkOutcome {
        let Some(dir) = self.kind_dir(kind) else {
            return MarkOutcome::Unavailable;
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(key))
        {
            Ok(_) => {
                Self::prune(&dir, max_age);
                MarkOutcome::First
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => MarkOutcome::AlreadyMarked,
            Err(e) => {
                debug!("state store: failed to create a marker ({:?})", e.kind());
                MarkOutcome::Unavailable
            }
        }
    }

    /// `kind` のディレクトリにキーの記録を書く（既存の記録は置き換える）。書けたら true。
    ///
    /// 一時ファイルへ書いてから rename するので、読み手が書きかけの内容を見ることは無い。
    pub fn put(&self, kind: &str, key: &str, contents: &[u8], max_age: Duration) -> bool {
        let Some(dir) = self.kind_dir(kind) else {
            return false;
        };
        let temp = dir.join(format!("{}.tmp-{}", key, std::process::id()));
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .and_then(|mut file| file.write_all(contents))
            .and_then(|()| fs::rename(&temp, dir.join(key)));
        match written {
            Ok(()) => {
                Self::prune(&dir, max_age);
                true
            }
            Err(e) => {
                let _ = fs::remove_file(&temp);
                debug!("state store: failed to write a record ({:?})", e.kind());
                false
            }
        }
    }

    /// `kind` のディレクトリからキーの記録を取り出して消す。取り出せたら内容を返す。
    ///
    /// 同じ記録を複数のプロセスが同時に取り出そうとしても、削除に成功した 1 つだけが
    /// 内容を受け取る（他は `None`）。読めても消せなかった場合は `None` を返す
    /// （取り出したと見なすと、同じ記録を別のプロセスも使えてしまうため）。
    pub fn take(&self, kind: &str, key: &str) -> Option<Vec<u8>> {
        let dir = self.kind_dir(kind)?;
        let path = dir.join(key);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let contents = fs::read(&path).ok()?;
        match fs::remove_file(&path) {
            Ok(()) => Some(contents),
            Err(e) => {
                debug!("state store: failed to remove a record ({:?})", e.kind());
                None
            }
        }
    }

    /// `kind` のディレクトリからキーの記録を消す（無ければ何もしない）。
    pub fn remove(&self, kind: &str, key: &str) {
        if let Some(dir) = self.kind_dir(kind)
            && let Err(e) = fs::remove_file(dir.join(key))
            && e.kind() != ErrorKind::NotFound
        {
            debug!("state store: failed to remove a record ({:?})", e.kind());
        }
    }

    /// `kind` のディレクトリを用意して返す。使えない状態なら `None`。
    fn kind_dir(&self, kind: &str) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        let dir = root.join(kind);
        if let Err(e) = create_private_dir_all(&dir) {
            debug!(
                "state store: failed to create the state directory ({:?})",
                e.kind()
            );
            return None;
        }
        if !is_private_dir(root) || !is_private_dir(&dir) {
            warn!(
                "⚠️ state store: the state directory is not a private directory owned by this user; \
                 state is not kept"
            );
            return None;
        }
        Some(dir)
    }

    /// `max_age` より古い記録を消す。シンボリックリンクは辿らず、名前がキーの形をした
    /// 通常ファイル（と書きかけの一時ファイル）だけを対象にする。
    fn prune(dir: &Path, max_age: Duration) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.take(PRUNE_SCAN_LIMIT).flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !is_state_file_name(name) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let expired = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age);
            if expired {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// 状態ファイルの名前（キー、またはキー + `.tmp-<pid>`）か。
fn is_state_file_name(name: &str) -> bool {
    let key = name.split_once(".tmp-").map_or(name, |(key, _)| key);
    key.len() == KEY_HEX_LEN && key.bytes().all(|b| b.is_ascii_hexdigit())
}

/// ディレクトリを（親も含めて）作る。Unix では新しく作る分を 0700 にする。
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// シンボリックリンクではない、自分が所有し、グループや他人が書き込めないディレクトリか。
fn is_private_dir(dir: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid は引数を取らず、失敗しない。
        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid || metadata.mode() & 0o022 != 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn test_key_is_stable_and_length_prefixed() {
        let key = StateStore::key(&["v1", "claude-code", "session-1"]);
        assert_eq!(key.len(), KEY_HEX_LEN);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(key, StateStore::key(&["v1", "claude-code", "session-1"]));
        // 区切り方が違えば別のキー（連結しただけでは同じ文字列になる組み合わせ）
        assert_ne!(StateStore::key(&["ab", "c"]), StateStore::key(&["a", "bc"]));
        assert_ne!(StateStore::key(&["a"]), StateStore::key(&["a", ""]));
    }

    #[test]
    fn test_key_matches_fnv1a_reference_value() {
        // FNV-1a 64 の実装を固定する（長さ前置の 8 バイト + 本文）。
        // 空の要素 1 つ = 0 を 8 バイト流した値。
        assert_eq!(StateStore::key(&[""]), "a8c7f832281a39c5");
    }

    #[test]
    fn test_mark_once_returns_first_only_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::at(dir.path().join("state"));
        let key = StateStore::key(&["session", "golangci-lint"]);

        assert_eq!(store.mark_once("notices", &key, DAY), MarkOutcome::First);
        assert_eq!(
            store.mark_once("notices", &key, DAY),
            MarkOutcome::AlreadyMarked
        );
        // 種類が違えば別の記録
        assert_eq!(store.mark_once("other", &key, DAY), MarkOutcome::First);
    }

    #[test]
    fn test_disabled_store_keeps_nothing() {
        let store = StateStore::disabled();
        let key = StateStore::key(&["x"]);
        assert_eq!(
            store.mark_once("notices", &key, DAY),
            MarkOutcome::Unavailable
        );
        assert!(!store.put("records", &key, b"{}", DAY));
        assert_eq!(store.take("records", &key), None);
    }

    #[test]
    fn test_put_then_take_returns_contents_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::at(dir.path());
        let key = StateStore::key(&["session"]);

        assert!(store.put("records", &key, b"first", DAY));
        assert!(
            store.put("records", &key, b"second", DAY),
            "既存の記録は置き換える"
        );
        assert_eq!(store.take("records", &key).as_deref(), Some(&b"second"[..]));
        assert_eq!(store.take("records", &key), None, "取り出した記録は消える");
    }

    #[test]
    fn test_remove_ignores_missing_record() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::at(dir.path());
        let key = StateStore::key(&["session"]);
        store.remove("records", &key);
        assert!(store.put("records", &key, b"x", DAY));
        store.remove("records", &key);
        assert_eq!(store.take("records", &key), None);
    }

    #[test]
    fn test_prune_removes_only_expired_state_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::at(dir.path());
        let old_key = StateStore::key(&["old"]);
        assert_eq!(
            store.mark_once("notices", &old_key, DAY),
            MarkOutcome::First
        );

        let notices = dir.path().join("notices");
        let old = fs::File::options()
            .write(true)
            .open(notices.join(&old_key))
            .unwrap();
        old.set_modified(SystemTime::now() - 2 * DAY).unwrap();
        // キーの形をしていないファイルは消さない
        let foreign = notices.join("keep-me.txt");
        fs::write(&foreign, "x").unwrap();
        let foreign_file = fs::File::options().write(true).open(&foreign).unwrap();
        foreign_file
            .set_modified(SystemTime::now() - 2 * DAY)
            .unwrap();

        // 新しい記録を作ると古い記録が掃除される
        let new_key = StateStore::key(&["new"]);
        assert_eq!(
            store.mark_once("notices", &new_key, DAY),
            MarkOutcome::First
        );

        assert!(!notices.join(&old_key).exists(), "期限切れの記録は消える");
        assert!(notices.join(&new_key).exists());
        assert!(foreign.exists(), "キーの形でないファイルは残す");
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_state_directory_is_not_used() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let store = StateStore::at(&link);
        let key = StateStore::key(&["x"]);
        assert_eq!(
            store.mark_once("notices", &key, DAY),
            MarkOutcome::Unavailable,
            "シンボリックリンクの置き場所は使わない"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_group_writable_state_directory_is_not_used() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();

        let store = StateStore::at(&root);
        let key = StateStore::key(&["x"]);
        assert_eq!(
            store.mark_once("notices", &key, DAY),
            MarkOutcome::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_created_directories_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("state");
        let store = StateStore::at(&root);
        let key = StateStore::key(&["x"]);
        assert_eq!(store.mark_once("notices", &key, DAY), MarkOutcome::First);
        for created in [&root, &root.join("notices")] {
            let mode = fs::metadata(created).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} は 0700 で作る", created.display());
        }
    }

    #[test]
    fn test_state_file_name_shape() {
        assert!(is_state_file_name("0123456789abcdef"));
        assert!(is_state_file_name("0123456789abcdef.tmp-42"));
        assert!(!is_state_file_name("0123456789abcde"));
        assert!(!is_state_file_name("keep-me.txt"));
    }
}
