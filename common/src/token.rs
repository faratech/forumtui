//! Token persistence. The OAuth token set is stored as a single JSON file in
//! the user's config dir with `0600` permissions on unix, written atomically
//! (temp file + rename) so a crash can never leave a half-written store.

use std::io::Write;
use std::path::{Path, PathBuf};

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
                .create(true)
                .truncate(true)
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
        let dest = sibling_with_suffix(&self.path, "corrupt");
        let _ = std::fs::rename(&self.path, &dest);
    }
}

pub(crate) fn tmp_sibling(path: &Path) -> PathBuf {
    sibling_with_suffix(path, "tmp")
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
        TokenSet {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1_800_000_000,
            scope: "node:read".into(),
        }
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
