//! Token persistence. The OAuth token set is stored as a single JSON file in
//! the user's config dir with `0600` permissions on unix, written atomically
//! (temp file + rename) so a crash can never leave a half-written store.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};

#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds; the API issues 2h access / 90d refresh tokens.
    pub expires_at: i64,
    pub scope: String,
    /// The site and OAuth client this grant belongs to, stamped by
    /// `WfApiClient::set_tokens`. Empty on a store written before sites
    /// were configurable — accepted as the built-in site's and back-filled
    /// on the next save. A client for a *different* origin or client id
    /// must never send this bearer (`WfApiClient::with_store` quarantines
    /// such a store instead of using it).
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub client_id: String,
}

impl TokenSet {
    /// True when this grant may be used by a client for `origin` /
    /// `client_id`: a match, or a legacy store that never recorded either.
    pub fn belongs_to(&self, origin: &str, client_id: &str) -> bool {
        (self.origin.is_empty() || self.origin == origin)
            && (self.client_id.is_empty() || self.client_id == client_id)
    }
}

// Deliberately manual: the derived Debug printed both live secrets, and one
// future `tracing::debug!("{tokens:?}")` would have logged the bearer and
// the 90-day refresh grant (#647).
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &format_args!("[redacted; {} chars]", self.access_token.len()))
            .field(
                "refresh_token",
                &format_args!("[redacted; {} chars]", self.refresh_token.len()),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("origin", &self.origin)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl TokenSet {
    /// Treat the access token as expired 60s early to absorb clock skew and
    /// in-flight request time.
    pub fn access_expired(&self, now: OffsetDateTime) -> bool {
        // Saturating: `expires_at` comes off disk and a hand-edited i64::MIN
        // must not underflow the skew margin (#642).
        now.unix_timestamp() >= self.expires_at.saturating_sub(60)
    }
}

pub struct Store {
    path: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Store {
            path: crate::config::token_path(),
        }
    }

    pub fn with_path(path: PathBuf) -> Self {
        Store { path }
    }

    /// Where this store reads and writes. Exposed so a caller can prove which
    /// file it is about to touch — the test suites of both crates assert it
    /// is their own scratch dir and never the operator's real config dir
    /// (issue #565).
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<TokenSet>> {
        if self.path.parent().is_some_and(|dir| !dir.exists()) {
            return Ok(None);
        }
        let _lock = lock_store(&self.path)?;
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::TokenStore(format!("{} is corrupt: {e}", self.path.display())))
    }

    pub fn save(&self, tokens: &TokenSet) -> Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| Error::TokenStore("token path has no parent".into()))?;
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::TokenStore(format!("cannot create {}: {e}", dir.display())))?;
        let _lock = lock_store(&self.path)?;
        let body = serde_json::to_vec_pretty(tokens)?;

        let tmp = tmp_sibling(&self.path);
        {
            #[allow(unused_mut)]
            let mut opts = std::fs::OpenOptions::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| Error::TokenStore(format!("cannot write {}: {e}", tmp.display())))?;
            f.write_all(&body)?;
            f.sync_all().ok();
        }
        #[cfg(unix)]
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| Error::TokenStore(format!("cannot finalize {}: {e}", self.path.display())))
    }

    pub fn erase(&self) -> Result<()> {
        if self.path.parent().is_some_and(|dir| !dir.exists()) {
            return Ok(());
        }
        let _lock = lock_store(&self.path)?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Move an unparseable (corrupt or old-schema) store out of the way so a
    /// future `save()` never fights it, and a curious user can inspect what
    /// was there. Best-effort: if the rename itself fails there is nothing
    /// better to do than proceed as if there were no session.
    pub fn quarantine_corrupt(&self) {
        if self.path.parent().is_some_and(|dir| !dir.exists()) {
            return;
        }
        let Ok(_lock) = lock_store(&self.path) else {
            return;
        };
        // `load()` releases its lock before the caller can decide to
        // quarantine. A sibling may have atomically installed a valid token
        // in that interval; never move that newer grant just because the
        // earlier read was corrupt.
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        if serde_json::from_slice::<TokenSet>(&bytes).is_ok() {
            return;
        }
        let dest = sibling_with_suffix(&self.path, "corrupt");
        let _ = std::fs::rename(&self.path, &dest);
    }

    /// Move a store that parses but belongs to another site or client aside
    /// as `token.json.<suffix>`, so this process starts with no session and
    /// the grant is kept for inspection rather than deleted or — worse —
    /// sent to the wrong origin. Only moves it if it still fails
    /// `belongs_to` under the lock: a sibling may have replaced it.
    pub fn quarantine_as(&self, suffix: &str, origin: &str, client_id: &str) {
        if self.path.parent().is_some_and(|dir| !dir.exists()) {
            return;
        }
        let Ok(_lock) = lock_store(&self.path) else {
            return;
        };
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        if let Ok(t) = serde_json::from_slice::<TokenSet>(&bytes)
            && t.belongs_to(origin, client_id)
        {
            return;
        }
        let dest = sibling_with_suffix(&self.path, suffix);
        let _ = std::fs::rename(&self.path, &dest);
    }

    /// Remove the file only if it still contains `expected`. A logout or a
    /// rejected refresh must not erase a newer grant another process wrote
    /// after this process took its snapshot.
    pub fn erase_if_refresh_token(&self, expected: &str) -> Result<bool> {
        if expected.is_empty() || self.path.parent().is_some_and(|dir| !dir.exists()) {
            return Ok(false);
        }
        let _lock = lock_store(&self.path)?;
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let current: TokenSet = serde_json::from_slice(&bytes)
            .map_err(|e| Error::TokenStore(format!("{} is corrupt: {e}", self.path.display())))?;
        if current.refresh_token != expected {
            return Ok(false);
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// Advisory inter-process lock shared by token and draft stores. The lock is
/// separate from the JSON file so the atomic rename never replaces the inode
/// another process is holding. `fs2` maps this to `flock` on Unix and the
/// corresponding mandatory Windows lock.
pub(crate) struct StoreLock {
    file: std::fs::File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn lock_store(path: &Path) -> Result<StoreLock> {
    let lock_path = sibling_with_suffix(path, "lock");
    #[allow(unused_mut)]
    let mut opts = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| Error::TokenStore(format!("cannot open {}: {e}", lock_path.display())))?;
    #[cfg(unix)]
    restrict_permissions(&lock_path);
    file.lock_exclusive()
        .map_err(|e| Error::TokenStore(format!("cannot lock {}: {e}", lock_path.display())))?;
    Ok(StoreLock { file })
}

/// `lock_store` without the wait: `Ok(None)` when another process holds the
/// lock. The update pass at start uses it so a sibling instance mid-download
/// can never stall this one's boot.
pub(crate) fn try_lock_store(path: &Path) -> Result<Option<StoreLock>> {
    let lock_path = sibling_with_suffix(path, "lock");
    #[allow(unused_mut)]
    let mut opts = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| Error::TokenStore(format!("cannot open {}: {e}", lock_path.display())))?;
    #[cfg(unix)]
    restrict_permissions(&lock_path);
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(StoreLock { file })),
        // `lock_contended_error` is EWOULDBLOCK on Unix and
        // ERROR_LOCK_VIOLATION on Windows; only the OS code is comparable.
        Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(None),
        Err(e) => Err(Error::TokenStore(format!("cannot lock {}: {e}", lock_path.display()))),
    }
}

pub(crate) fn tmp_sibling(path: &Path) -> PathBuf {
    // A fixed `token.json.tmp`/`drafts.json.tmp` lets two processes sharing a
    // config directory truncate each other's in-progress JSON. The final
    // rename is atomic, but the temporary writer was not isolated. Include a
    // process-local sequence so concurrent writers each own a unique inode.
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let base = sibling_with_suffix(path, "tmp");
    let name = base
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".wftui-token.tmp".into());
    base.with_file_name(format!("{name}.{}.{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)))
}

pub(crate) fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| format!("{}.{suffix}", n.to_string_lossy()))
        .unwrap_or_else(|| format!(".wftui-token.{suffix}"));
    path.with_file_name(name)
}

#[cfg(unix)]
pub(crate) fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TokenSet {
        TokenSet { origin: String::new(), client_id: String::new(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1_800_000_000,
            scope: "node:read".into(),
        }
    }

    #[test]
    fn concurrent_store_writes_get_distinct_temp_paths() {
        let path = std::path::Path::new("/tmp/wftui-token-race/token.json");
        let first = tmp_sibling(path);
        let second = tmp_sibling(path);
        assert_ne!(first, second, "atomic writers must not share a temp inode");
        assert!(first.file_name().unwrap().to_string_lossy().contains(".tmp."));
        assert!(second.file_name().unwrap().to_string_lossy().contains(".tmp."));
    }

    #[test]
    fn round_trip_and_erase() {
        let dir = std::env::temp_dir().join(format!("wftui-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::with_path(dir.join("token.json"));

        assert!(store.load().unwrap().is_none());
        store.save(&sample()).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.refresh_token, "rt");
        store.erase().unwrap();
        assert!(store.load().unwrap().is_none());
        store.erase().unwrap(); // idempotent
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conditional_erase_does_not_remove_a_newer_grant() {
        let dir = std::env::temp_dir().join(format!("wftui-token-conditional-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::with_path(dir.join("token.json"));
        store.save(&sample()).unwrap();
        store
            .save(&TokenSet { origin: String::new(), client_id: String::new(),
                refresh_token: "new-refresh".into(),
                ..sample()
            })
            .unwrap();

        assert!(!store.erase_if_refresh_token("rt").unwrap());
        assert_eq!(store.load().unwrap().unwrap().refresh_token, "new-refresh");
        assert!(store.erase_if_refresh_token("new-refresh").unwrap());
        assert!(store.load().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("wftui-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::with_path(dir.join("token.json"));
        store.save(&sample()).unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A body from an older/incompatible schema (missing `scope`, added
    /// after this fixture's TokenSet shipped) must surface as a
    /// `TokenStore` error, never panic or silently coerce — this is exactly
    /// what `WfApiClient::new()` (issue #546) has to catch and treat as "no
    /// session".
    #[test]
    fn load_reports_a_body_missing_a_field_as_corrupt() {
        let dir = std::env::temp_dir().join(format!("wftui-token-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        std::fs::write(&path, br#"{"access_token":"a","refresh_token":"b","expires_at":1}"#).unwrap();
        let store = Store::with_path(path);

        let err = store.load().unwrap_err();
        match err {
            Error::TokenStore(msg) => assert!(msg.contains("corrupt"), "{msg}"),
            other => panic!("expected TokenStore, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_corrupt_renames_the_file_and_load_then_sees_no_session() {
        let dir = std::env::temp_dir().join(format!("wftui-token-quarantine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        std::fs::write(&path, b"not json at all").unwrap();
        let store = Store::with_path(path.clone());

        assert!(store.load().is_err());
        store.quarantine_corrupt();
        assert!(!path.exists());
        assert!(dir.join("token.json.corrupt").exists());
        assert!(store.load().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_corrupt_does_not_move_a_valid_replacement() {
        let dir = std::env::temp_dir().join(format!("wftui-token-quarantine-valid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        let store = Store::with_path(path.clone());

        std::fs::write(&path, b"not json at all").unwrap();
        // Model the sibling's atomic replacement after the failed load.
        store.save(&sample()).unwrap();
        store.quarantine_corrupt();

        assert!(path.exists(), "a valid replacement must remain in place");
        assert!(!dir.join("token.json.corrupt").exists());
        assert_eq!(store.load().unwrap().unwrap().refresh_token, "rt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expiry_has_skew_margin() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let mut t = sample();
        t.expires_at = 1_100;
        assert!(!t.access_expired(now));
        t.expires_at = 1_050;
        assert!(t.access_expired(now)); // within the 60s window
    }

    /// `expires_at` comes off disk and a hand-edited i64::MIN must not
    /// underflow the 60s skew margin (#642): it simply counts as expired.
    #[test]
    fn a_minimal_expiry_counts_as_expired_not_a_panic() {
        let now = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let mut t = sample();
        t.expires_at = i64::MIN;
        assert!(t.access_expired(now));
    }

    /// The manual Debug must never print the bearer or the 90-day refresh
    /// grant (#647) — one future `tracing::debug!("{tokens:?}")` would
    /// otherwise leak both.
    #[test]
    fn debug_redacts_both_secrets() {
        let t = sample();
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("\"at\""), "Debug leaked the access token: {dbg}");
        assert!(!dbg.contains("\"rt\""), "Debug leaked the refresh token: {dbg}");
        assert!(dbg.contains("redacted"), "Debug must say it redacted: {dbg}");
    }
}
