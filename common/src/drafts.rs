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
        Self { origin: origin.trim_end_matches('/').to_owned(), user_id }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct StoredDrafts {
    version: u32,
    owner: Option<Owner>,
    drafts: HashMap<String, Draft>,
}

impl StoredDrafts {
    fn entries(&self) -> HashMap<DraftKey, Draft> {
        self.drafts.iter().filter_map(|(key, value)| {
            Some((DraftKey::from_key(key)?, value.clone()))
        }).collect()
    }
}

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
        Store {
            path: crate::config::drafts_path(),
        }
    }

    /// The store for a configured site (`config::drafts_path_for`): the
    /// built-in site's is the flat `<config dir>/drafts.json` every install
    /// already has, any other site's lives under `sites/<name>/`.
    pub fn for_site(name: &str) -> Self {
        Self::with_path(crate::config::drafts_path_for(name))
    }

    pub fn with_path(path: std::path::PathBuf) -> Self {
        Store { path }
    }

    /// Where this store reads and writes. Exposed so a caller can prove which
    /// file it is about to touch, exactly as `token::Store::path` is — both
    /// test suites assert nothing they build resolves to the operator's real
    /// config dir (#565).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn ensure_parent(&self) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| Error::TokenStore("draft path has no parent".into()))?;
        std::fs::create_dir_all(parent)?;
        Ok(())
    }

    fn read_locked(&self) -> Result<StoredDrafts> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StoredDrafts::default()),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice::<StoredDrafts>(&bytes) {
            Ok(stored) if stored.version == 2 => Ok(stored),
            _ => {
                // Pre-release unowned drafts are deliberately discarded on
                // upgrade. Keep genuinely corrupt data available for diagnosis.
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

    /// Select the verified account. Replacing an owner clears its drafts in
    /// the same transaction, so a sibling with the old owner cannot write back.
    pub fn activate(&self, owner: &Owner) -> Result<HashMap<DraftKey, Draft>> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        if stored.owner.as_ref() != Some(owner) {
            stored = StoredDrafts { version: 2, owner: Some(owner.clone()), drafts: HashMap::new() };
            self.write_locked(&stored)?;
        }
        Ok(stored.entries())
    }

    pub fn load(&self, owner: &Owner) -> HashMap<DraftKey, Draft> {
        let Ok(_lock) = crate::token::lock_store(&self.path) else { return HashMap::new() };
        match self.read_locked() {
            Ok(stored) if stored.owner.as_ref() == Some(owner) => stored.entries(),
            _ => HashMap::new(),
        }
    }

    /// Apply only explicit mutations against the freshest snapshot under the
    /// lock. Untouched keys and deletions from another instance survive.
    pub fn apply(&self, owner: &Owner, changes: &[(DraftKey, Option<Draft>)]) -> Result<()> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let mut stored = self.read_locked()?;
        if stored.version == 0 {
            stored.version = 2;
            stored.owner = Some(owner.clone());
        }
        if stored.owner.as_ref() != Some(owner) {
            return Err(Error::TokenStore("draft account changed; refusing a stale save".into()));
        }
        for (key, draft) in changes {
            match draft {
                Some(draft) if !draft.is_empty() && !draft.too_big_to_persist() => {
                    stored.drafts.insert(key.as_key(), draft.clone());
                }
                _ => { stored.drafts.remove(&key.as_key()); }
            }
        }
        let mut keys: Vec<_> = stored.drafts.iter().map(|(key, draft)| (key.clone(), draft.saved_at)).collect();
        keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (key, _) in keys.into_iter().skip(MAX_DRAFTS) { stored.drafts.remove(&key); }
        self.write_locked(&stored)
    }

    /// Leave an empty ownership tombstone on sign-out. A stale instance must
    /// not recreate private drafts after this owner was explicitly cleared.
    pub fn erase(&self, owner: &Owner) -> Result<()> {
        self.ensure_parent()?;
        let _lock = crate::token::lock_store(&self.path)?;
        let stored = self.read_locked()?;
        if stored.owner.as_ref() == Some(owner) {
            self.write_locked(&StoredDrafts { version: 2, ..Default::default() })?;
        }
        Ok(())
    }

    fn write_locked(&self, stored: &StoredDrafts) -> Result<()> {
        let body = serde_json::to_vec_pretty(stored)?;
        let tmp = tmp_sibling(&self.path);
        let result = (|| {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.write(true).create_new(true).open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            drop(f);
            #[cfg(unix)]
            restrict_permissions(&tmp);
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&tmp); }
        result
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wftui-drafts-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("drafts.json")
    }

    fn owner() -> Owner { Owner::new("http://127.0.0.1:9", 7) }

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
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
        assert_eq!(store.load(&owner()), map, "a draft must survive the round trip byte for byte");
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
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
        assert_eq!(store.load(&owner()), map);
    }

    #[test]
    fn an_empty_draft_is_not_stored() {
        let store = Store::with_path(scratch("empty"));
        let mut map = HashMap::new();
        map.insert(DraftKey::ThreadReply(1), draft("   \n  ", 1));
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
        assert!(store.load(&owner()).is_empty(), "whitespace is not a draft");
        assert!(store.load(&owner()).is_empty(), "only ownership metadata remains");
    }

    /// The editor supports very large pastes; `drafts.json` should not have
    /// to. An oversized draft is skipped whole rather than truncated — half a
    /// restored post would be worse than none.
    #[test]
    fn an_oversized_draft_is_skipped_not_truncated() {
        let store = Store::with_path(scratch("big"));
        let mut map = HashMap::new();
        map.insert(DraftKey::ThreadReply(1), draft(&"x".repeat(MAX_DRAFT_BYTES + 1), 2));
        map.insert(DraftKey::ThreadReply(2), draft("small", 1));
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
        let back = store.load(&owner());
        assert!(!back.contains_key(&DraftKey::ThreadReply(1)), "oversized draft skipped");
        assert_eq!(back[&DraftKey::ThreadReply(2)].body, "small", "the small one still saves");
    }

    #[test]
    fn only_the_newest_drafts_are_kept() {
        let store = Store::with_path(scratch("cap"));
        let map: HashMap<DraftKey, Draft> = (0..MAX_DRAFTS as u32 + 10)
            .map(|i| (DraftKey::ThreadReply(i), draft("b", i as i64)))
            .collect();
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
        let back = store.load(&owner());
        assert_eq!(back.len(), MAX_DRAFTS);
        assert!(back.contains_key(&DraftKey::ThreadReply(MAX_DRAFTS as u32 + 9)), "newest kept");
        assert!(!back.contains_key(&DraftKey::ThreadReply(0)), "oldest evicted");
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
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
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
        assert_eq!(DraftKey::ThreadReply(51465).xf_key().as_deref(), Some("thread-51465"));
        assert_eq!(DraftKey::NewThread(88).xf_key().as_deref(), Some("forum-88"));
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
        store.apply(&owner(), &map.iter().map(|(k, v)| (*k, Some(v.clone()))).collect::<Vec<_>>()).unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "an unsent post is private");
    }

    /// #565: no test may resolve the operator's real config dir. `Store::new`
    /// follows `WFTUI_CONFIG_DIR` exactly as the token store does.
    #[test]
    fn guard_the_draft_store_never_resolves_the_real_config_dir() {
        let _lock = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        b.apply(&owner, &[(second, Some(draft("new two", 3)))]).unwrap();
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
        let future = r#"{"version":3,"owner":null,"drafts":{}}"#;
        std::fs::write(&path, future).unwrap();
        assert!(store.activate(&owner()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), future);
    }

}
