//! Composer draft persistence (issue #715).
//!
//! Esc in the composer was an unconditional screen pop, so a long reply was
//! destroyed by one keystroke with no confirmation and no way back. Rather
//! than make the common case (an empty draft) pay for a confirm prompt, the
//! draft is kept and offered back the next time that same composer opens.
//!
//! Stored like the token set: one JSON file in the config dir, `0600`, and
//! written atomically (temp file + rename), so a crash mid-write cannot leave
//! a half-file. Unlike the token set an unreadable store is never an error
//! the user sees — a draft is a convenience, and failing to read one must not
//! stop them writing a new post.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
#[cfg(unix)]
use crate::token::restrict_permissions;
use crate::token::tmp_sibling;

/// The largest draft that is written to disk. A draft above this stays in
/// memory for the session — the editor deliberately supports very large
/// pastes (there is a 70,000-row paste test), and neither a multi-megabyte
/// `drafts.json` nor the fsync it costs on every Esc is a fair price for
/// keeping one.
pub const MAX_DRAFT_BYTES: usize = 256 * 1024;

/// How many drafts are kept. Past this the oldest by `saved_at` is dropped,
/// so an unbounded browse-and-abandon session cannot grow the file forever.
pub const MAX_DRAFTS: usize = 32;

/// Which composer a draft belongs to.
///
/// Deliberately the *identity* of the target and nothing else: an edit of
/// post 5 and a fresh reply to the thread that holds it are different
/// composers, so they must not share a draft.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DraftKey {
    ThreadReply(u32),
    EditPost(u32),
    NewThread(u32),
    ConversationReply(u32),
}

impl DraftKey {
    /// The on-disk form. JSON object keys must be strings, so the map is
    /// keyed by this rather than by the enum.
    pub fn as_key(&self) -> String {
        match self {
            DraftKey::ThreadReply(id) => format!("thread:{id}"),
            DraftKey::EditPost(id) => format!("post:{id}"),
            DraftKey::NewThread(id) => format!("forum:{id}"),
            DraftKey::ConversationReply(id) => format!("conversation:{id}"),
        }
    }

    /// The key XenForo uses for the same composer, or `None` for a kind XF
    /// has no draft for (#716).
    ///
    /// These are not ours to choose: they come from the entity relation
    /// conditions XF matches on — `Thread::DraftReplies` is `thread-$thread_id`,
    /// `Forum::DraftThreads` is `forum-$node_id`, and
    /// `ConversationMaster::DraftReplies` is
    /// `conversation-reply-$conversation_id`. Get one wrong and the draft is
    /// invisible to the website rather than broken, which is worse.
    ///
    /// `EditPost` returns `None` on purpose, and that `None` is the whole
    /// "edits stay local" rule: XF's edit form has no draft at all, so there
    /// is no key to sync to.
    pub fn xf_key(&self) -> Option<String> {
        match self {
            DraftKey::ThreadReply(id) => Some(format!("thread-{id}")),
            DraftKey::NewThread(id) => Some(format!("forum-{id}")),
            DraftKey::ConversationReply(id) => Some(format!("conversation-reply-{id}")),
            DraftKey::EditPost(_) => None,
        }
    }

    /// The inverse. An unknown kind — `report-…`, or one an add-on adds
    /// later — is `None` and the draft is skipped, never guessed at.
    pub fn from_xf_key(s: &str) -> Option<Self> {
        // `conversation-reply-` first: `split_once('-')` on it would otherwise
        // yield the kind "conversation" and a non-numeric rest.
        if let Some(id) = s.strip_prefix("conversation-reply-") {
            return id.parse().ok().map(DraftKey::ConversationReply);
        }
        if let Some(id) = s.strip_prefix("thread-") {
            return id.parse().ok().map(DraftKey::ThreadReply);
        }
        if let Some(id) = s.strip_prefix("forum-") {
            return id.parse().ok().map(DraftKey::NewThread);
        }
        None
    }

    /// Parse the on-disk form. An unrecognised key is `None` and its draft is
    /// dropped: a store written by a newer build must not brick an older one.
    pub fn from_key(s: &str) -> Option<Self> {
        let (kind, id) = s.split_once(':')?;
        let id: u32 = id.parse().ok()?;
        match kind {
            "thread" => Some(DraftKey::ThreadReply(id)),
            "post" => Some(DraftKey::EditPost(id)),
            "forum" => Some(DraftKey::NewThread(id)),
            "conversation" => Some(DraftKey::ConversationReply(id)),
            _ => None,
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// The key this draft's uploads share (#709). Kept so a resumed draft
    /// still carries the files that were attached before Esc.
    #[serde(default)]
    pub attachment_key: Option<String>,
    /// What this draft is a reply to, in words — "Windows 11 won't boot",
    /// "New thread in Windows Support". Stored rather than derived because
    /// the drafts list has only the key to go on otherwise, and "thread-51465"
    /// is not an answer to "what was I writing?" (#716). Empty for a draft
    /// that arrived from the website, which sends no title for the target.
    #[serde(default)]
    pub label: String,
    /// Unix seconds, for the eviction order and for telling the user how old
    /// the thing they just got back is.
    #[serde(default)]
    pub saved_at: i64,
    /// This draft came from the website and its copy there carries
    /// attachments this client cannot re-attach (#716): XF stores a
    /// `temp_hash`, and the API's attachment keys are rows that *wrap* a
    /// hash, so a hash cannot be turned back into a usable key. Say so on
    /// resume rather than let the files vanish silently.
    #[serde(default)]
    pub remote_attachments: bool,
    /// Edit drafts are local because XF has no edit-draft kind. Keep the
    /// original target and seed so a restart can restore the editor with the
    /// post's current text underneath the newer draft instead of inventing a
    /// zero thread id or discarding the baseline.
    #[serde(default)]
    pub edit_thread_id: Option<u32>,
    #[serde(default)]
    pub edit_thread_title: Option<String>,
    #[serde(default)]
    pub edit_seed_body: Option<String>,
}

impl Draft {
    /// Nothing worth keeping. An empty draft is *removed* rather than stored,
    /// so opening a composer, thinking better of it and pressing Esc does not
    /// leave a blank draft to be resumed later.
    pub fn is_empty(&self) -> bool {
        self.title.trim().is_empty() && self.body.trim().is_empty()
    }

    pub fn too_big_to_persist(&self) -> bool {
        self.title.len() + self.body.len() > MAX_DRAFT_BYTES
    }
}

/// Draft ownership is established only after the API verifies the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub origin: String,
    pub user_id: u32,
}

impl Owner {
    pub fn new(origin: &str, user_id: u32) -> Self {
        Self {
            origin: origin.trim_end_matches('/').to_owned(),
            user_id,
        }
    }
}

/// The latest desired website state for one draft slot. Revisions are minted
/// under the store lock and never reused, including across account changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayKind {
    Save,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayOperation {
    pub revision: u64,
    pub kind: RelayKind,
}

#[derive(Default)]
pub struct Snapshot {
    pub drafts: HashMap<DraftKey, Draft>,
    pub operations: HashMap<DraftKey, RelayOperation>,
    pub revision: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredDrafts {
    version: u32,
    owner: Option<Owner>,
    drafts: HashMap<String, Draft>,
    /// Retained when the owner is cleared. An old process must never settle
    /// a new operation after the same account signs in again.
    revision: u64,
    revisions: HashMap<String, u64>,
    operations: HashMap<String, RelayOperation>,
    /// Version 2 input only. Migration saves a private backup before replacing it.
    #[serde(skip_serializing)]
    owed_deletes: Vec<String>,
}

impl StoredDrafts {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            drafts: self
                .drafts
                .iter()
                .filter_map(|(key, value)| Some((DraftKey::from_key(key)?, value.clone())))
                .collect(),
            operations: self
                .operations
                .iter()
                .filter_map(|(key, value)| Some((DraftKey::from_key(key)?, *value)))
                .collect(),
            revision: self.revision,
        }
    }

    fn next_revision(&mut self, key: DraftKey) -> Result<u64> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::TokenStore("draft revision counter exhausted".into()))?;
        self.revisions.insert(key.as_key(), self.revision);
        Ok(self.revision)
    }

    fn require_owner(&self, owner: &Owner) -> Result<()> {
        if self.owner.as_ref() == Some(owner) {
            Ok(())
        } else {
            Err(Error::TokenStore(
                "draft account changed; refusing a stale save".into(),
            ))
        }
    }

    fn trim_drafts(&mut self, protect_dirty: bool) {
        let mut keys: Vec<_> = self
            .drafts
            .iter()
            .map(|(key, draft)| {
                let dirty = protect_dirty && self.operations.contains_key(key);
                (key.clone(), draft.saved_at, dirty)
            })
            .collect();
        keys.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        for (key, _, _) in keys.into_iter().skip(MAX_DRAFTS) {
            self.drafts.remove(&key);
            self.operations.remove(&key);
            self.revisions.insert(key, self.revision);
        }
    }
}

/// This separate lock is held across a relay request. The store's own lock
/// is held only while reading or committing a small JSON transaction.
pub struct RelayLock(#[allow(dead_code)] crate::token::StoreLock);

#[derive(Clone)]
pub struct Store {
    path: std::path::PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Self::with_path(crate::config::drafts_path())
    }

    pub fn for_site(name: &str) -> Self {
        Self::with_path(crate::config::drafts_path_for(name))
    }

    pub fn with_path(path: std::path::PathBuf) -> Self {
        Store { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn ensure_parent(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::TokenStore("draft path has no parent".into()))?;
        std::fs::create_dir_all(parent)?;
        Ok(())
    }

    fn read_locked(&self) -> Result<StoredDrafts> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoredDrafts::default());
            }
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice::<StoredDrafts>(&bytes) {
            Ok(stored) if stored.version == 3 => Ok(stored),
            Ok(mut stored) if stored.version == 2 => {
                // A surviving local draft is the newest local intent even
                // when v2 still owes deletion of an older website copy.
                let mut keys: Vec<_> = stored
                    .drafts
                    .keys()
                    .chain(stored.owed_deletes.iter())
                    .filter_map(|key| DraftKey::from_key(key))
                    .filter(|key| key.xf_key().is_some())
                    .collect();
                keys.sort_by_key(DraftKey::as_key);
                keys.dedup();
                stored.operations.clear();
                for key in keys {
                    let kind = match stored.drafts.get(&key.as_key()) {
                        Some(draft) if !draft.is_empty() && !draft.too_big_to_persist() => {
                            RelayKind::Save
                        }
                        _ if stored.owed_deletes.contains(&key.as_key()) => RelayKind::Delete,
                        _ => continue,
                    };
                    let revision = stored.next_revision(key)?;
                    stored
                        .operations
                        .insert(key.as_key(), RelayOperation { revision, kind });
                }
                let backup =
                    crate::token::sibling_with_suffix(&tmp_sibling(&self.path), "v2-backup");
                Self::write_private(&backup, &bytes)?;
                stored.version = 3;
                stored.owed_deletes.clear();
                self.write_locked(&stored)?;
                Ok(stored)
            }
            _ => {
                // Preserve unknown versioned formats, including invalid v3
                // records. A newer client's words must never be overwritten.
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if value.get("version").is_some() {
                        return Err(Error::TokenStore("unsupported draft store format".into()));
                    }
                    std::fs::remove_file(&self.path)?;
                } else {
                    let dest = crate::token::sibling_with_suffix(&self.path, "corrupt");
                    std::fs::rename(&self.path, dest)?;
                }
                Ok(StoredDrafts::default())
            }
        }
    }

    pub fn activate(&self, owner: &Owner) -> Result<HashMap<DraftKey, Draft>> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        if stored.owner.as_ref() != Some(owner) {
            stored.version = 3;
            stored.owner = Some(owner.clone());
            stored.drafts.clear();
            stored.operations.clear();
            stored.revisions.clear();
            self.write_locked(&stored)?;
        }
        Ok(stored.snapshot().drafts)
    }

    pub fn snapshot(&self, owner: &Owner) -> Result<Snapshot> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let stored = self.read_locked()?;
        stored.require_owner(owner)?;
        Ok(stored.snapshot())
    }

    pub fn load(&self, owner: &Owner) -> HashMap<DraftKey, Draft> {
        self.snapshot(owner)
            .map(|snapshot| snapshot.drafts)
            .unwrap_or_default()
    }

    /// Save the local text and its desired relay operation in ONE transaction.
    /// A relay failure or process exit cannot leave locally durable words
    /// without the obligation that protects them from a stale remote merge.
    /// Oversized text remains memory-only: its revision fences old requests,
    /// but no replayable Save is persisted without its payload.
    pub fn apply(
        &self,
        owner: &Owner,
        changes: &[(DraftKey, Option<Draft>)],
    ) -> Result<HashMap<DraftKey, RelayOperation>> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        if stored.version == 0 {
            stored.version = 3;
            stored.owner = Some(owner.clone());
        }
        stored.require_owner(owner)?;
        let mut changed = HashMap::new();
        for (key, draft) in changes {
            let revision = stored.next_revision(*key)?;
            let draft = draft.as_ref().filter(|draft| !draft.is_empty());
            let kind = if draft.is_some() {
                RelayKind::Save
            } else {
                RelayKind::Delete
            };
            let op = RelayOperation { revision, kind };
            if key.xf_key().is_some() {
                changed.insert(*key, op);
            }
            if let Some(draft) = draft.filter(|draft| !draft.too_big_to_persist()) {
                stored.drafts.insert(key.as_key(), draft.clone());
                if key.xf_key().is_some() {
                    stored.operations.insert(key.as_key(), op);
                }
            } else {
                stored.drafts.remove(&key.as_key());
                if key.xf_key().is_some() && kind == RelayKind::Delete {
                    stored.operations.insert(key.as_key(), op);
                } else {
                    stored.operations.remove(&key.as_key());
                }
            }
        }
        // Eviction is local capacity management, not a website delete.
        stored.trim_drafts(false);
        self.write_locked(&stored)?;
        Ok(changed)
    }

    /// Read work only AFTER acquiring the key's relay lock. An old waiter
    /// skips work superseded by this or another process before dispatch.
    pub fn relay_work(
        &self,
        owner: &Owner,
        key: DraftKey,
        expected: RelayOperation,
        transient: Option<&Draft>,
    ) -> Result<Option<Draft>> {
        let _lock = crate::token::lock_store(&self.path)?;
        let stored = self.read_locked()?;
        stored.require_owner(owner)?;
        if stored.revisions.get(&key.as_key()) != Some(&expected.revision) {
            return Ok(None);
        }
        match (stored.operations.get(&key.as_key()), expected.kind) {
            (Some(op), _) if *op != expected => Ok(None),
            (Some(_), RelayKind::Delete) => Ok(Some(Draft::default())),
            (Some(_), RelayKind::Save) => Ok(stored.drafts.get(&key.as_key()).cloned()),
            (None, RelayKind::Save) => Ok(transient
                .filter(|draft| draft.too_big_to_persist())
                .cloned()),
            _ => Ok(None),
        }
    }

    /// Only an acknowledgment for the CURRENT operation may retire it.
    pub fn settle(&self, owner: &Owner, key: DraftKey, expected: RelayOperation) -> Result<bool> {
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        stored.require_owner(owner)?;
        if stored.revisions.get(&key.as_key()) != Some(&expected.revision) {
            return Ok(false);
        }
        match stored.operations.get(&key.as_key()) {
            Some(op) if *op == expected => {
                stored.operations.remove(&key.as_key());
                self.write_locked(&stored)?;
                Ok(true)
            }
            None if expected.kind == RelayKind::Save => Ok(true), // memory-only oversized draft
            _ => Ok(false),
        }
    }

    /// Import a website copy only if no local mutation superseded the list's
    /// snapshot and no unsynced intent owns this slot. Remote imports never
    /// create Save operations of their own.
    pub fn merge_remote(
        &self,
        owner: &Owner,
        key: DraftKey,
        draft: &Draft,
        observed: u64,
    ) -> Result<bool> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        if stored.version == 0 {
            stored.version = 3;
            stored.owner = Some(owner.clone());
        }
        stored.require_owner(owner)?;
        if stored.operations.contains_key(&key.as_key())
            || stored
                .revisions
                .get(&key.as_key())
                .is_some_and(|revision| *revision > observed)
        {
            return Ok(false);
        }
        stored.next_revision(key)?;
        if !draft.is_empty() && !draft.too_big_to_persist() {
            stored.drafts.insert(key.as_key(), draft.clone());
        } else {
            stored.drafts.remove(&key.as_key());
        }
        // A remote list must not evict unsynced local words merely because
        // its unrelated drafts carry newer server timestamps.
        stored.trim_drafts(true);
        let accepted = stored.drafts.contains_key(&key.as_key()) || draft.too_big_to_persist();
        self.write_locked(&stored)?;
        Ok(accepted)
    }

    /// Nonblocking kernel lock; callers run the filesystem attempt off the
    /// event loop and wait asynchronously before retrying a busy key.
    pub fn try_relay_lock(&self, key: DraftKey) -> Result<Option<RelayLock>> {
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        let anchor = self
            .path
            .with_file_name(format!("{name}.relay-{}", key.as_key().replace(':', "-")));
        crate::token::try_lock_store(&anchor).map(|lock| lock.map(RelayLock))
    }

    pub fn erase(&self, owner: &Owner) -> Result<()> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        if stored.owner.as_ref() == Some(owner) {
            stored.owner = None;
            stored.drafts.clear();
            stored.operations.clear();
            stored.revisions.clear();
            // Keep the global revision even when the account is cleared.
            self.write_locked(&stored)?;
        }
        Ok(())
    }

    fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.write(true).create_new(true).open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        restrict_permissions(path);
        Ok(())
    }

    fn write_locked(&self, stored: &StoredDrafts) -> Result<()> {
        let body = serde_json::to_vec_pretty(stored)?;
        let tmp = tmp_sibling(&self.path);
        let result = (|| {
            Self::write_private(&tmp, &body)?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wftui-drafts-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("drafts.json")
    }

    fn owner() -> Owner {
        Owner::new("http://127.0.0.1:9", 7)
    }

    #[test]
    fn a_delete_survives_reload_and_only_its_exact_revision_settles() {
        let path = scratch("owed");
        let store = Store::with_path(path.clone());
        store.activate(&owner()).unwrap();
        let key = DraftKey::ThreadReply(51465);
        let first = store.apply(&owner(), &[(key, None)]).unwrap()[&key];
        let reopened = Store::with_path(path);
        assert_eq!(reopened.snapshot(&owner()).unwrap().operations[&key], first);
        let second = reopened.apply(&owner(), &[(key, None)]).unwrap()[&key];
        assert!(!store.settle(&owner(), key, first).unwrap());
        assert_eq!(store.snapshot(&owner()).unwrap().operations[&key], second);
        assert!(store.settle(&owner(), key, second).unwrap());
        assert!(reopened.snapshot(&owner()).unwrap().operations.is_empty());
        let other = Owner::new(&owner().origin, 8);
        assert!(reopened.snapshot(&other).is_err());
    }

    fn draft(body: &str, saved_at: i64) -> Draft {
        Draft {
            title: String::new(),
            body: body.into(),
            attachment_key: None,
            saved_at,
            remote_attachments: false,
            label: String::new(),
            edit_thread_id: None,
            edit_thread_title: None,
            edit_seed_body: None,
        }
    }

    #[test]
    fn a_saved_draft_comes_back_verbatim() {
        let store = Store::with_path(scratch("roundtrip"));
        let mut map = HashMap::new();
        map.insert(
            DraftKey::ThreadReply(42),
            Draft {
                title: "t".into(),
                body: "line one\n\nline two [B]bold[/B]".into(),
                attachment_key: Some("key-1".into()),
                saved_at: 1_700_000_000,
                remote_attachments: false,
                label: "A thread".into(),
                edit_thread_id: None,
                edit_thread_title: None,
                edit_seed_body: None,
            },
        );
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(
            store.load(&owner()),
            map,
            "a draft must survive the round trip byte for byte"
        );
    }

    /// Every key kind must survive, or a resumed conversation reply would
    /// come back as a thread reply and post to the wrong place.
    #[test]
    fn every_key_kind_round_trips_and_stays_distinct() {
        let keys = [
            DraftKey::ThreadReply(7),
            DraftKey::EditPost(7),
            DraftKey::NewThread(7),
            DraftKey::ConversationReply(7),
        ];
        // Same id in all four: the kind is the only thing telling them apart.
        for k in keys {
            assert_eq!(DraftKey::from_key(&k.as_key()), Some(k), "{k:?}");
        }
        let store = Store::with_path(scratch("kinds"));
        let map: HashMap<DraftKey, Draft> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (*k, draft(&format!("body {i}"), 1)))
            .collect();
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(store.load(&owner()), map);
    }

    #[test]
    fn an_empty_draft_is_not_stored() {
        let store = Store::with_path(scratch("empty"));
        let mut map = HashMap::new();
        map.insert(DraftKey::ThreadReply(1), draft("   \n  ", 1));
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert!(store.load(&owner()).is_empty(), "whitespace is not a draft");
        assert!(
            store.load(&owner()).is_empty(),
            "only ownership metadata remains"
        );
    }

    /// The editor supports very large pastes; `drafts.json` should not have
    /// to. An oversized draft is skipped whole rather than truncated — half a
    /// restored post would be worse than none.
    #[test]
    fn an_oversized_draft_is_skipped_not_truncated() {
        let store = Store::with_path(scratch("big"));
        let mut map = HashMap::new();
        map.insert(
            DraftKey::ThreadReply(1),
            draft(&"x".repeat(MAX_DRAFT_BYTES + 1), 2),
        );
        map.insert(DraftKey::ThreadReply(2), draft("small", 1));
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let back = store.load(&owner());
        assert!(
            !back.contains_key(&DraftKey::ThreadReply(1)),
            "oversized draft skipped"
        );
        assert_eq!(
            back[&DraftKey::ThreadReply(2)].body,
            "small",
            "the small one still saves"
        );
    }

    #[test]
    fn only_the_newest_drafts_are_kept() {
        let store = Store::with_path(scratch("cap"));
        let map: HashMap<DraftKey, Draft> = (0..MAX_DRAFTS as u32 + 10)
            .map(|i| (DraftKey::ThreadReply(i), draft("b", i as i64)))
            .collect();
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let back = store.load(&owner());
        assert_eq!(back.len(), MAX_DRAFTS);
        assert!(
            back.contains_key(&DraftKey::ThreadReply(MAX_DRAFTS as u32 + 9)),
            "newest kept"
        );
        assert!(
            !back.contains_key(&DraftKey::ThreadReply(0)),
            "oldest evicted"
        );
    }

    /// A corrupt convenience file must never stand between the user and the
    /// composer: it is moved aside and the session starts with no drafts,
    /// rather than surfacing an error (contrast `token::Store::load`, where a
    /// corrupt store *is* worth reporting).
    #[test]
    fn a_corrupt_store_is_quarantined_and_reads_as_no_drafts() {
        let path = scratch("corrupt");
        let store = Store::with_path(path.clone());
        std::fs::write(&path, b"{not json").unwrap();
        assert!(store.load(&owner()).is_empty());
        assert!(!path.exists(), "the corrupt file is moved out of the way");
        assert!(
            path.with_file_name("drafts.json.corrupt").exists(),
            "and kept, so a curious user can see what was there"
        );
        // The next save must then work rather than fight the old file.
        let mut map = HashMap::new();
        map.insert(DraftKey::NewThread(3), draft("after", 1));
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(store.load(&owner()), map);
    }

    /// A store written by a newer build must not brick an older one: an
    /// unknown key kind drops that draft and keeps the rest.
    #[test]
    fn an_unknown_key_kind_is_dropped_not_fatal() {
        let path = scratch("unknown");
        let store = Store::with_path(path.clone());
        std::fs::write(
            &path,
            br#"{"version":2,"owner":{"origin":"http://127.0.0.1:9","user_id":7},"drafts":{"profile:9":{"body":"future"},"thread:1":{"body":"now"}}}"#,
        )
        .unwrap();
        let back = store.load(&owner());
        assert_eq!(back.len(), 1);
        assert_eq!(back[&DraftKey::ThreadReply(1)].body, "now");
    }

    /// #716: these three strings are XenForo's, not ours — they come from the
    /// entity relation conditions XF matches drafts on. A wrong one does not
    /// error, it just makes the draft invisible to the website, so pin them
    /// literally rather than round-tripping through our own formatter.
    #[test]
    fn xf_keys_are_the_ones_xenforo_matches_on() {
        assert_eq!(
            DraftKey::ThreadReply(51465).xf_key().as_deref(),
            Some("thread-51465")
        );
        assert_eq!(
            DraftKey::NewThread(88).xf_key().as_deref(),
            Some("forum-88")
        );
        assert_eq!(
            DraftKey::ConversationReply(5).xf_key().as_deref(),
            Some("conversation-reply-5")
        );
    }

    /// XF's edit form has no draft, so an edit has nowhere to sync to. This
    /// `None` is the entire "edits stay local" rule.
    #[test]
    fn an_edit_draft_has_no_xf_key() {
        assert_eq!(DraftKey::EditPost(500).xf_key(), None);
    }

    #[test]
    fn xf_keys_round_trip_and_stay_distinct() {
        for k in [
            DraftKey::ThreadReply(7),
            DraftKey::NewThread(7),
            DraftKey::ConversationReply(7),
        ] {
            let key = k.xf_key().expect("has an xf key");
            assert_eq!(DraftKey::from_xf_key(&key), Some(k), "{key}");
        }
    }

    /// `conversation-reply-N` starts with neither `thread-` nor `forum-`, but
    /// a naive `split_once('-')` reads it as kind "conversation"; and a kind
    /// this build does not know (a report draft, or an add-on's) must be
    /// skipped rather than guessed at.
    #[test]
    fn unknown_or_malformed_xf_keys_are_rejected() {
        for bad in [
            "report-1",
            "conversation-5",
            "thread-",
            "thread-x",
            "forum-1x",
            "../etc/passwd",
            "",
        ] {
            assert_eq!(DraftKey::from_xf_key(bad), None, "{bad:?} must not parse");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let store = Store::with_path(scratch("perms"));
        let mut map = HashMap::new();
        map.insert(DraftKey::ThreadReply(1), draft("private words", 1));
        store
            .apply(
                &owner(),
                &map.iter()
                    .map(|(k, v)| (*k, Some(v.clone())))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "an unsent post is private");
    }

    /// #565: no test may resolve the operator's real config dir. `Store::new`
    /// follows `WFTUI_CONFIG_DIR` exactly as the token store does.
    #[test]
    fn guard_the_draft_store_never_resolves_the_real_config_dir() {
        let _lock = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("WFTUI_CONFIG_DIR").ok();
        unsafe { std::env::set_var("WFTUI_CONFIG_DIR", "/tmp/wftui-drafts-guard") };
        let path = Store::new().path().to_path_buf();
        match prev {
            Some(v) => unsafe { std::env::set_var("WFTUI_CONFIG_DIR", v) },
            None => unsafe { std::env::set_var("WFTUI_CONFIG_DIR", "/tmp/wftui-test-cfg") },
        }
        assert!(
            !path.starts_with(crate::config::default_config_root()),
            "drafts must follow WFTUI_CONFIG_DIR, got {path:?}"
        );
        assert!(path.starts_with("/tmp/wftui-drafts-guard"), "got {path:?}");
    }
    #[test]
    fn concurrent_stores_merge_explicit_edits_without_resurrecting_deletions() {
        let path = scratch("concurrent");
        let a = Store::with_path(path.clone());
        let b = Store::with_path(path);
        let owner = owner();
        a.activate(&owner).unwrap();
        let first = DraftKey::EditPost(1);
        let second = DraftKey::EditPost(2);
        a.apply(&owner, &[(first, Some(draft("one", 1)))]).unwrap();
        b.apply(&owner, &[(second, Some(draft("two", 2)))]).unwrap();
        assert_eq!(a.load(&owner).len(), 2);
        a.apply(&owner, &[(first, None)]).unwrap();
        b.apply(&owner, &[(second, Some(draft("new two", 3)))])
            .unwrap();
        let read = a.load(&owner);
        assert!(!read.contains_key(&first));
        assert_eq!(read[&second].body, "new two");
    }

    #[test]
    fn account_and_origin_changes_reject_old_writers_and_logout_leaves_no_drafts() {
        let store = Store::with_path(scratch("owners"));
        let a = owner();
        let b = Owner::new(&a.origin, 8);
        let other_origin = Owner::new("http://127.0.0.1:10", 8);
        let change = [(DraftKey::EditPost(1), Some(draft("private", 1)))];
        store.activate(&a).unwrap();
        store.apply(&a, &change).unwrap();
        assert_eq!(store.activate(&a).unwrap().len(), 1);
        assert!(store.activate(&b).unwrap().is_empty());
        assert!(store.load(&a).is_empty());
        assert!(store.apply(&a, &change).is_err());
        store.apply(&b, &change).unwrap();
        assert!(store.activate(&other_origin).unwrap().is_empty());
        assert!(store.apply(&b, &change).is_err());
        store.apply(&other_origin, &change).unwrap();
        store.erase(&other_origin).unwrap();
        assert!(store.load(&other_origin).is_empty());
        assert!(store.apply(&other_origin, &change).is_err());
    }

    #[test]
    fn upgrade_discards_unowned_drafts_but_does_not_destroy_a_future_store() {
        let path = scratch("migration");
        let store = Store::with_path(path.clone());
        std::fs::write(&path, r#"{"thread:1":{"body":"legacy"}}"#).unwrap();
        assert!(store.activate(&owner()).unwrap().is_empty());
        let future = r#"{"version":4,"owner":null,"drafts":{}}"#;
        std::fs::write(&path, future).unwrap();
        assert!(store.activate(&owner()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), future);
    }

    #[test]
    fn version_two_migration_backs_up_and_prefers_surviving_local_words() {
        let path = scratch("v2-backup");
        let original = br#"{"version":2,"owner":{"origin":"http://127.0.0.1:9","user_id":7},"drafts":{"thread:1":{"body":"new local words"},"post:3":{"body":"local edit"}},"owed_deletes":["thread:1","thread:2"]}"#;
        std::fs::write(&path, original).unwrap();
        let store = Store::with_path(path.clone());
        let snapshot = store.snapshot(&owner()).unwrap();
        assert_eq!(
            snapshot.drafts[&DraftKey::ThreadReply(1)].body,
            "new local words"
        );
        assert_eq!(
            snapshot.operations[&DraftKey::ThreadReply(1)].kind,
            RelayKind::Save
        );
        assert_eq!(
            snapshot.operations[&DraftKey::ThreadReply(2)].kind,
            RelayKind::Delete
        );
        assert!(!snapshot.operations.contains_key(&DraftKey::EditPost(3)));
        let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with("v2-backup"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                backups[0].metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()["version"],
            3
        );
    }

    #[test]
    fn revisions_survive_owner_tombstones_and_stale_settlement_cannot_clear_new_intent() {
        let store = Store::with_path(scratch("revisions"));
        let key = DraftKey::ThreadReply(7);
        let first = store
            .apply(&owner(), &[(key, Some(draft("first", 1)))])
            .unwrap()[&key];
        store.erase(&owner()).unwrap();
        store.activate(&owner()).unwrap();
        let second = store
            .apply(&owner(), &[(key, Some(draft("second", 2)))])
            .unwrap()[&key];
        assert!(second.revision > first.revision);
        assert!(!store.settle(&owner(), key, first).unwrap());
        let third = store.apply(&owner(), &[(key, None)]).unwrap()[&key];
        assert!(!store.settle(&owner(), key, second).unwrap());
        assert_eq!(store.snapshot(&owner()).unwrap().operations[&key], third);
    }

    #[test]
    fn dirty_and_already_settled_sibling_edits_both_fence_a_stale_remote_list() {
        let path = scratch("merge-revision");
        let a = Store::with_path(path.clone());
        let b = Store::with_path(path);
        a.activate(&owner()).unwrap();
        let observed = a.snapshot(&owner()).unwrap().revision;
        let key = DraftKey::ThreadReply(7);
        let saved = b
            .apply(&owner(), &[(key, Some(draft("latest local", 2)))])
            .unwrap()[&key];
        assert!(
            !a.merge_remote(&owner(), key, &draft("stale remote", 1), observed)
                .unwrap()
        );
        b.settle(&owner(), key, saved).unwrap();
        assert!(
            !a.merge_remote(&owner(), key, &draft("stale remote", 1), observed)
                .unwrap()
        );
        assert_eq!(a.load(&owner())[&key].body, "latest local");
    }

    #[test]
    fn the_first_website_draft_creates_an_absent_store_parent() {
        let path = scratch("first-import")
            .parent()
            .unwrap()
            .join("new-site")
            .join("drafts.json");
        assert!(!path.parent().unwrap().exists());
        let store = Store::with_path(path);
        let key = DraftKey::ThreadReply(7);
        assert!(
            store
                .merge_remote(&owner(), key, &draft("typed in the browser", 1), 0)
                .unwrap()
        );
        assert_eq!(store.load(&owner())[&key].body, "typed in the browser");
        assert!(store.snapshot(&owner()).unwrap().operations.is_empty());
    }

    #[test]
    fn oversize_text_has_no_durable_save_without_payload_and_fences_old_work() {
        let store = Store::with_path(scratch("oversize-intent"));
        let key = DraftKey::ThreadReply(7);
        let old = store
            .apply(&owner(), &[(key, Some(draft("small old", 1)))])
            .unwrap()[&key];
        let huge = draft(&"x".repeat(MAX_DRAFT_BYTES + 1), 2);
        let current = store.apply(&owner(), &[(key, Some(huge.clone()))]).unwrap()[&key];
        let snapshot = store.snapshot(&owner()).unwrap();
        assert!(!snapshot.drafts.contains_key(&key));
        assert!(!snapshot.operations.contains_key(&key));
        assert!(
            store
                .relay_work(&owner(), key, old, None)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .relay_work(&owner(), key, current, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .relay_work(&owner(), key, current, Some(&huge))
                .unwrap(),
            Some(huge)
        );
    }

    #[test]
    fn relay_file_lock_is_shared_with_another_process() {
        let path = scratch("process-lock");
        let store = Store::with_path(path.clone());
        let key = DraftKey::ThreadReply(7);
        let held = store.try_relay_lock(key).unwrap().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "drafts::tests::relay_lock_child", "--nocapture"])
            .env("WFTUI_DRAFT_RELAY_LOCK_TEST", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(held);
        assert!(store.try_relay_lock(key).unwrap().is_some());
    }

    #[test]
    fn relay_lock_child() {
        let Some(path) = std::env::var_os("WFTUI_DRAFT_RELAY_LOCK_TEST") else {
            return;
        };
        assert!(
            Store::with_path(path.into())
                .try_relay_lock(DraftKey::ThreadReply(7))
                .unwrap()
                .is_none()
        );
    }
}
