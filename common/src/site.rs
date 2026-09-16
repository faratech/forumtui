//! Site configuration: which XenForo forum this process talks to, how it
//! signs in, what it is called, and which add-ons it may assume.
//!
//! One site per process. The built-in default is windowsforum.com exactly as
//! the client has always shipped it ([`SiteConfig::windowsforum`]); a
//! `config.json` beside the token store adds other forums, or overrides
//! fields of the built-in one. Selection and precedence are documented on
//! [`resolve`]. Nothing here touches the network: a config error is decided
//! before the terminal is set up, and is loud.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bbcode::{self, Rgb};
use crate::config;
use crate::error::{Error, Result};

/// The name of the compiled-in site. Its files keep the legacy flat layout
/// (`<config dir>/token.json`) so existing installs never notice.
pub const BUILTIN_NAME: &str = "windowsforum";

/// Whether this binary has a site built in (the `builtin-windowsforum`
/// cargo feature). The Forum Terminal (TUI) edition does; "Terminal for
/// XenForo" does not and asks on first run.
pub const HAS_BUILTIN_SITE: bool = cfg!(feature = "builtin-windowsforum");

/// What the product is called in this edition (the `--help` banner, the
/// Setup screen, packaging). The command is `wftui` in both.
pub const PRODUCT_NAME: &str = if HAS_BUILTIN_SITE { "Forum Terminal (TUI)" } else { "Terminal for XenForo" };

/// The edition's mark in release asset names: the two editions share one
/// update feed, and the updater must never install the other one's binary.
pub const EDITION_SUFFIX: &str = if HAS_BUILTIN_SITE { "" } else { "-xf" };

/// Where the client's own project lives — the `+url` half of the UA for any
/// site that is not the built-in one (hard rule 6 keeps the UA distinctive;
/// it must not advertise another forum's address).
pub const PROJECT_URL: &str = "https://github.com/faratech/forumtui";

/// Scopes every screen needs on a stock XenForo 2.3. XFMG (`media:read`)
/// and XFRM (`resource:read`) are added by [`SiteConfig::effective_scopes`]
/// when the site says it has them — requesting a scope the client row
/// lacks fails the whole handshake (#695).
pub const BASE_SCOPES: [&str; 12] = [
    "node:read",
    "thread:read",
    "thread:write",
    "user:read",
    "conversation:read",
    "conversation:write",
    "alert:read",
    "search:read",
    "search:write",
    "attachment:read",
    "attachment:write",
    "profile_post:read",
];

/// `g` chords the client owns; a site's quick-node letters may not collide.
pub const RESERVED_CHORDS: [char; 10] = ['l', 'i', 'a', 'm', 'r', 'd', 'h', 'p', 'g', 'u'];

/// The most quick destinations a site may declare: they are also the `1`-`9`
/// keys on the Home screen.
pub const MAX_QUICK: usize = 9;

/// How the client obtains an authorization code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMode {
    /// TuiLink relay if the site has it, else loopback, else paste.
    #[default]
    Auto,
    /// The `WindowsForum/TuiLink` add-on's short link + poll.
    TuiLink,
    /// A browser on this machine redirects to a listener on 127.0.0.1.
    Loopback,
    /// The browser is elsewhere: the user pastes the redirected URL back.
    Paste,
}

impl LoginMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LoginMode::Auto => "auto",
            LoginMode::TuiLink => "tuilink",
            LoginMode::Loopback => "loopback",
            LoginMode::Paste => "paste",
        }
    }

    /// The order the `m` key cycles through on the sign-in screen.
    pub fn next(self) -> LoginMode {
        match self {
            LoginMode::Auto => LoginMode::TuiLink,
            LoginMode::TuiLink => LoginMode::Loopback,
            LoginMode::Loopback => LoginMode::Paste,
            LoginMode::Paste => LoginMode::Auto,
        }
    }
}

/// Which add-ons the site runs. `None` on the two relays means "find out":
/// a 404 from the relay route flips them off for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub xfmg: bool,
    pub xfrm: bool,
    pub tuilink: Option<bool>,
    pub drafts_relay: Option<bool>,
}

/// What the chrome calls the site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brand {
    /// The wordmark in the header and on the sign-in screen.
    pub name: String,
    /// The 1-4 character chip in front of it (` WF `).
    pub mark: String,
    /// The leading part of `name` drawn bold (`Windows` in `WindowsForum`).
    pub bold_prefix: String,
    /// The header band colour; `None` is the default blue.
    pub chrome_bg: Option<Rgb>,
    /// A PNG for the sign-in screen on pixel tiers, resolved against the
    /// config dir when relative. `None` on the built-in site means the
    /// embedded logo; on any other site, no logo.
    pub logo: Option<PathBuf>,
}

impl Brand {
    /// `(bold, rest)` — the two halves of the wordmark.
    pub fn split(&self) -> (&str, &str) {
        match self.name.strip_prefix(self.bold_prefix.as_str()) {
            Some(rest) if !self.bold_prefix.is_empty() => (self.bold_prefix.as_str(), rest),
            _ => ("", self.name.as_str()),
        }
    }
}

/// A `g <key>` chord, a palette row and a Home-screen digit for one forum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickNode {
    pub key: char,
    pub label: String,
    pub node_id: u32,
}

/// One site, fully resolved: every field has a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteConfig {
    /// Slug (`[a-z0-9-]+`); also the per-site directory name.
    pub name: String,
    pub origin: String,
    pub oauth_client_id: String,
    /// Explicit scope list; `None` derives one from `features`.
    pub scopes: Option<Vec<String>>,
    pub features: Features,
    pub login: LoginMode,
    pub brand: Brand,
    pub quick: Vec<QuickNode>,
    /// Members whose posts wear the `AI` chip (a site's bot accounts).
    pub bot_user_ids: Vec<u32>,
    /// Words dropped from thread prefixes in narrow rows (`"Windows "` on a
    /// Windows forum carries no information).
    pub prefix_strip: Vec<String>,
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self::windowsforum()
    }
}

impl SiteConfig {
    /// windowsforum.com, exactly as the client shipped before sites were
    /// configurable. `windowsforum_equals_the_shipped_constants` pins every
    /// value so the default can never drift.
    pub fn windowsforum() -> Self {
        SiteConfig {
            name: BUILTIN_NAME.into(),
            origin: config::BASE_URL.into(),
            oauth_client_id: config::DEFAULT_OAUTH_CLIENT_ID.into(),
            scopes: None,
            features: Features {
                xfmg: true,
                xfrm: true,
                tuilink: Some(true),
                drafts_relay: Some(true),
            },
            login: LoginMode::Auto,
            brand: Brand {
                name: "WindowsForum".into(),
                mark: "WF".into(),
                bold_prefix: "Windows".into(),
                chrome_bg: Some(Rgb { r: 0x0F, g: 0x6C, b: 0xBD }),
                logo: None,
            },
            quick: vec![
                QuickNode { key: 'n', label: "Windows News".into(), node_id: 4 },
                QuickNode { key: 's', label: "Security Alerts".into(), node_id: 84 },
                QuickNode { key: 't', label: "Windows Tutorials".into(), node_id: 305 },
            ],
            bot_user_ids: vec![125_694],
            prefix_strip: vec!["Windows ".into()],
        }
    }

    /// A site the file names but the binary knows nothing about: no origin,
    /// no client id, no add-ons, a brand made from the slug.
    pub fn blank(name: &str) -> Self {
        let mark: String = name.chars().take(2).collect::<String>().to_ascii_uppercase();
        SiteConfig {
            name: name.into(),
            origin: String::new(),
            oauth_client_id: String::new(),
            scopes: None,
            features: Features::default(),
            login: LoginMode::Auto,
            brand: Brand {
                name: name.into(),
                mark: if mark.is_empty() { "XF".into() } else { mark },
                bold_prefix: String::new(),
                chrome_bg: None,
                logo: None,
            },
            quick: Vec::new(),
            bot_user_ids: Vec::new(),
            prefix_strip: Vec::new(),
        }
    }

    pub fn is_builtin(&self) -> bool {
        self.name == BUILTIN_NAME
    }

    /// The site a generic-edition process runs as *before* Setup has named
    /// one: enough for the chrome to draw the product's own name, never
    /// valid (`https://setup.invalid`), never written to disk.
    pub fn placeholder() -> Self {
        let mut site = Self::blank("setup");
        site.origin = "https://setup.invalid".into();
        site.oauth_client_id = "-".into();
        site.brand.name = PRODUCT_NAME.into();
        site.brand.mark = "XF".into();
        site
    }

    pub fn is_placeholder(&self) -> bool {
        self.name == "setup" && self.origin == "https://setup.invalid"
    }

    /// The scopes the authorize request asks for.
    pub fn effective_scopes(&self) -> Vec<String> {
        if let Some(explicit) = &self.scopes {
            return explicit.clone();
        }
        let mut scopes: Vec<String> = BASE_SCOPES.iter().map(|s| s.to_string()).collect();
        if self.features.xfmg {
            scopes.push("media:read".into());
        }
        if self.features.xfrm {
            scopes.push("resource:read".into());
        }
        scopes
    }

    /// `wftui/<ver> (+<url>)` — the site's own origin for the built-in site
    /// (Cloudflare's bot rule on windowsforum.com is keyed to it, hard rule
    /// 6), the project URL for every other site.
    pub fn user_agent(&self) -> String {
        if self.is_builtin() {
            config::user_agent()
        } else {
            config::user_agent_for_project()
        }
    }

    /// The host name, for messages ("Can't reach forum.example.com").
    pub fn host(&self) -> &str {
        let rest = self
            .origin
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.origin);
        rest.split('/').next().unwrap_or(rest)
    }

    /// Everything a bad file can get wrong, checked once at startup.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: String| Error::Config(format!("site {:?}: {what}", self.name));
        if self.name.is_empty()
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(Error::Config(format!(
                "site name {:?} must be lowercase letters, digits and dashes",
                self.name
            )));
        }
        if self.origin.is_empty() {
            return Err(bad("\"origin\" is required (https://forum.example.com)".into()));
        }
        config::validate_base_url(&self.origin)?;
        if self.oauth_client_id.trim().is_empty() {
            return Err(bad(
                "\"oauth_client_id\" is required — a public (PKCE) OAuth client registered in the \
                 forum's Admin CP (Setup → Service providers → OAuth clients)"
                    .into(),
            ));
        }
        if let Some(scopes) = &self.scopes {
            for s in scopes {
                let ok = s
                    .split_once(':')
                    .is_some_and(|(h, t)| !h.is_empty() && matches!(t, "read" | "write"));
                if !ok {
                    return Err(bad(format!("scope {s:?} is not <family>:read|write")));
                }
            }
        }
        let mark = &self.brand.mark;
        if mark.is_empty() || mark.len() > 4 || !mark.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(bad("\"brand.mark\" must be 1-4 printable ASCII characters".into()));
        }
        if self.brand.name.trim().is_empty() {
            return Err(bad("\"brand.name\" must not be empty".into()));
        }
        if !self.brand.bold_prefix.is_empty() && !self.brand.name.starts_with(&self.brand.bold_prefix) {
            return Err(bad(format!(
                "\"brand.bold_prefix\" {:?} is not a prefix of \"brand.name\" {:?}",
                self.brand.bold_prefix, self.brand.name
            )));
        }
        if self.quick.len() > MAX_QUICK {
            return Err(bad(format!("at most {MAX_QUICK} \"quick\" entries (they are the 1-9 keys)")));
        }
        let mut seen = Vec::new();
        for q in &self.quick {
            if !q.key.is_ascii_lowercase() {
                return Err(bad(format!("quick key {:?} must be a lowercase letter", q.key)));
            }
            if RESERVED_CHORDS.contains(&q.key) {
                return Err(bad(format!(
                    "quick key {:?} is a chord the client uses (reserved: {})",
                    q.key,
                    RESERVED_CHORDS.iter().collect::<String>()
                )));
            }
            if seen.contains(&q.key) {
                return Err(bad(format!("quick key {:?} is used twice", q.key)));
            }
            seen.push(q.key);
            if q.label.trim().is_empty() || q.node_id == 0 {
                return Err(bad("every \"quick\" entry needs a label and a node_id".into()));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ the file

/// `config.json` as written: every field optional, unknown keys ignored so a
/// newer build's file loads on an older one.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_site: Option<String>,
    pub sites: Vec<RawSite>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawSite {
    pub name: String,
    pub origin: Option<String>,
    pub oauth_client_id: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub features: RawFeatures,
    pub login: Option<LoginMode>,
    pub brand: RawBrand,
    pub quick: Option<Vec<RawQuick>>,
    pub bot_user_ids: Option<Vec<u32>>,
    pub prefix_strip: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawFeatures {
    pub xfmg: Option<bool>,
    pub xfrm: Option<bool>,
    pub tuilink: Option<bool>,
    pub drafts_relay: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawBrand {
    pub name: Option<String>,
    pub mark: Option<String>,
    pub bold_prefix: Option<String>,
    pub chrome_bg: Option<String>,
    pub logo: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawQuick {
    pub key: String,
    pub label: String,
    pub node_id: u32,
}

impl RawSite {
    /// Lay this entry over `base` (the built-in site, or a blank one).
    fn apply(&self, mut site: SiteConfig, config_dir: &Path) -> Result<SiteConfig> {
        if let Some(v) = &self.origin {
            site.origin = v.trim().trim_end_matches('/').to_string();
        }
        if let Some(v) = &self.oauth_client_id {
            site.oauth_client_id = v.trim().to_string();
        }
        if let Some(v) = &self.scopes {
            site.scopes = Some(v.clone());
        }
        if let Some(v) = self.features.xfmg {
            site.features.xfmg = v;
        }
        if let Some(v) = self.features.xfrm {
            site.features.xfrm = v;
        }
        // `null` in the file is "auto", the same as absent; `false`/`true`
        // pin it. Serde folds both to `None`, so only `Some` is an override.
        if let Some(v) = self.features.tuilink {
            site.features.tuilink = Some(v);
        }
        if let Some(v) = self.features.drafts_relay {
            site.features.drafts_relay = Some(v);
        }
        if let Some(v) = self.login {
            site.login = v;
        }
        if let Some(v) = &self.brand.name {
            site.brand.name = v.trim().to_string();
            // A renamed brand keeps a bold prefix only if it still fits.
            if !site.brand.name.starts_with(&site.brand.bold_prefix) {
                site.brand.bold_prefix.clear();
            }
        }
        if let Some(v) = &self.brand.mark {
            site.brand.mark = v.trim().to_string();
        }
        if let Some(v) = &self.brand.bold_prefix {
            site.brand.bold_prefix = v.clone();
        }
        if let Some(v) = &self.brand.chrome_bg {
            site.brand.chrome_bg = match bbcode::parse_color(v) {
                Some(rgb) => Some(rgb),
                None => {
                    return Err(Error::Config(format!(
                        "site {:?}: \"brand.chrome_bg\" {v:?} is not a CSS colour",
                        self.name
                    )));
                }
            };
        }
        if let Some(v) = &self.brand.logo {
            let p = PathBuf::from(v);
            site.brand.logo = Some(if p.is_absolute() { p } else { config_dir.join(p) });
        }
        if let Some(v) = &self.quick {
            site.quick = v
                .iter()
                .map(|q| {
                    let mut chars = q.key.chars();
                    let key = match (chars.next(), chars.next()) {
                        (Some(c), None) => c,
                        _ => {
                            return Err(Error::Config(format!(
                                "site {:?}: quick key {:?} must be one character",
                                self.name, q.key
                            )));
                        }
                    };
                    Ok(QuickNode { key, label: q.label.trim().to_string(), node_id: q.node_id })
                })
                .collect::<Result<Vec<_>>>()?;
        }
        if let Some(v) = &self.bot_user_ids {
            site.bot_user_ids = v.clone();
        }
        if let Some(v) = &self.prefix_strip {
            site.prefix_strip = v.clone();
        }
        Ok(site)
    }
}

impl Config {
    /// A missing file is the built-in site alone. Anything else that fails
    /// — unreadable, not JSON, wrong shape — is a hard error naming the
    /// file and, for a parse error, the line and column: a config mistake
    /// must stop the client on the normal screen, never be quietly
    /// replaced by the default.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(Error::Config(format!("cannot read {}: {e}", path.display()))),
        };
        let config: Config = serde_json::from_str(&text).map_err(|e| {
            Error::Config(format!(
                "{} line {} column {}: {}",
                path.display(),
                e.line(),
                e.column(),
                e
            ))
        })?;
        let mut names: Vec<&str> = Vec::new();
        for site in &config.sites {
            if names.contains(&site.name.as_str()) {
                return Err(Error::Config(format!(
                    "{}: site {:?} is listed twice",
                    path.display(),
                    site.name
                )));
            }
            names.push(&site.name);
        }
        Ok(config)
    }

    /// The names the file knows, plus the built-in one in the edition that
    /// has it.
    pub fn site_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.sites.iter().map(|s| s.name.clone()).collect();
        if HAS_BUILTIN_SITE && !names.iter().any(|n| n == BUILTIN_NAME) {
            names.push(BUILTIN_NAME.into());
        }
        names
    }

    /// What `--init-config` writes: the built-in site named, and one stub
    /// to fill in. `_comment` keys are ignored on load, so the file
    /// documents itself.
    pub fn example_json() -> String {
        let mut sites = Vec::new();
        if HAS_BUILTIN_SITE {
            sites.push(serde_json::json!({
                "_comment": "The built-in default. Listing it is optional; any field given here overrides the compiled-in value.",
                "name": BUILTIN_NAME
            }));
        }
        sites.push(serde_json::json!({
                    "_comment": "Another XenForo 2.3 forum. `oauth_client_id` is a PUBLIC client the forum's admin creates under Setup → Service providers → OAuth clients, with redirect URIs http://127.0.0.1/callback and (if the TuiLink add-on is installed) https://<origin>/tui-done.",
                    "name": "example",
                    "origin": "https://forum.example.com",
                    "oauth_client_id": "PASTE-CLIENT-ID",
                    "login": "auto",
                    "features": { "xfmg": false, "xfrm": false, "tuilink": null, "drafts_relay": null },
                    "brand": {
                        "name": "ExampleForum",
                        "bold_prefix": "Example",
                        "mark": "EX",
                        "chrome_bg": "#3A7D44",
                        "logo": null
                    },
                    "quick": [ { "key": "n", "label": "News", "node_id": 12 } ],
                    "bot_user_ids": [],
                    "prefix_strip": []
        }));
        serde_json::to_string_pretty(&serde_json::json!({
            "_comment": "wftui sites. `default_site` picks one when neither `wftui <site>` nor WFTUI_SITE says. Unknown keys are ignored.",
            "default_site": if HAS_BUILTIN_SITE { BUILTIN_NAME } else { "example" },
            "sites": sites,
        }))
        .expect("static JSON")
            + "\n"
    }
}

/// `<config dir>/config.json`.
pub fn config_path() -> PathBuf {
    config::config_dir().join("config.json")
}

/// Add (or replace) one site in `config.json` and make it the default —
/// what the generic edition's Setup screen writes. The file is read as
/// plain JSON so every key the loader would ignore survives, and written
/// temp-file-plus-rename like the token store, so a crash leaves either the
/// old file or the new one.
pub fn save_site(path: &Path, site: &SiteConfig) -> Result<()> {
    let mut root: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(Error::Config(format!("cannot read {}: {e}", path.display()))),
    };
    if !root.is_object() {
        return Err(Error::Config(format!("{}: not a JSON object", path.display())));
    }
    let entry = serde_json::json!({
        "name": site.name,
        "origin": site.origin,
        "oauth_client_id": site.oauth_client_id,
        "login": site.login.as_str(),
        "features": {
            "xfmg": site.features.xfmg,
            "xfrm": site.features.xfrm,
            "tuilink": site.features.tuilink,
            "drafts_relay": site.features.drafts_relay,
        },
        "brand": {
            "name": site.brand.name,
            "bold_prefix": site.brand.bold_prefix,
            "mark": site.brand.mark,
        },
    });
    let obj = root.as_object_mut().expect("checked above");
    let sites = obj
        .entry("sites")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let Some(list) = sites.as_array_mut() else {
        return Err(Error::Config(format!("{}: \"sites\" is not a list", path.display())));
    };
    match list.iter_mut().find(|s| s.get("name").and_then(|n| n.as_str()) == Some(site.name.as_str())) {
        Some(existing) => *existing = entry,
        None => list.push(entry),
    }
    obj.insert("default_site".into(), serde_json::Value::String(site.name.clone()));
    let body = serde_json::to_string_pretty(&root).map_err(Error::Json)? + "\n";
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        "config.json.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// The site the Setup screen's three answers describe, validated. The slug
/// is the host with dots as dashes; the mark is the name's first two
/// letters. Everything else is what `blank` gives and `config.json` can
/// change later.
pub fn site_from_setup(origin: &str, client_id: &str, name: &str) -> Result<SiteConfig> {
    let origin = origin.trim().trim_end_matches('/');
    let host = origin.split_once("://").map(|(_, r)| r).unwrap_or(origin);
    let host = host.split('/').next().unwrap_or(host).split(':').next().unwrap_or(host);
    let slug: String = host
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if slug.is_empty() {
        return Err(Error::Config("\"origin\" is required (https://forum.example.com)".into()));
    }
    let mut site = SiteConfig::blank(&slug);
    site.origin = origin.to_string();
    site.oauth_client_id = client_id.trim().to_string();
    let name = name.trim();
    if !name.is_empty() {
        site.brand.name = name.to_string();
        let mark: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).take(2).collect::<String>().to_ascii_uppercase();
        if !mark.is_empty() {
            site.brand.mark = mark;
        }
    }
    site.validate()?;
    Ok(site)
}

/// Pick and build the site for this process.
///
/// Selection, first hit wins: `site_arg` (`wftui <name>` / `--site`) →
/// `WFTUI_SITE` → the file's `default_site` → the only site the file lists
/// → the built-in one. Field precedence, highest first: the `WFTUI_BASE_URL`
/// / `WFTUI_OAUTH_CLIENT_ID` environment (tests and staging depend on it) →
/// the file's entry → the compiled-in default. With no file and no
/// environment the result is [`SiteConfig::windowsforum`], byte for byte.
pub fn resolve(config: &Config, site_arg: Option<&str>, config_dir: &Path) -> Result<SiteConfig> {
    resolve_or_setup(config, site_arg, config_dir)?.ok_or_else(|| {
        Error::Config(format!(
            "no site configured — run wftui once to set one up, or `wftui --init-config` and edit {}",
            config_dir.join("config.json").display()
        ))
    })
}

/// [`resolve`], but `Ok(None)` when nothing names a site and this edition
/// has none built in: the first run of "Terminal for XenForo", which the
/// client answers with its Setup screen rather than an error.
pub fn resolve_or_setup(
    config: &Config,
    site_arg: Option<&str>,
    config_dir: &Path,
) -> Result<Option<SiteConfig>> {
    let env_site = std::env::var("WFTUI_SITE").ok().filter(|s| !s.trim().is_empty());
    let name: String = match (site_arg, env_site, &config.default_site) {
        (Some(arg), _, _) => arg.trim().to_string(),
        (None, Some(env), _) => env.trim().to_string(),
        (None, None, Some(default)) => default.trim().to_string(),
        (None, None, None) => match config.sites.as_slice() {
            [only] => only.name.clone(),
            _ if HAS_BUILTIN_SITE => BUILTIN_NAME.into(),
            _ => return Ok(None),
        },
    };
    let entry = config.sites.iter().find(|s| s.name == name);
    let base = if name == BUILTIN_NAME {
        SiteConfig::windowsforum()
    } else if entry.is_some() {
        SiteConfig::blank(&name)
    } else {
        return Err(Error::Config(format!(
            "unknown site {name:?}; known sites: {}",
            config.site_names().join(", ")
        )));
    };
    let mut site = match entry {
        Some(raw) => raw.apply(base, config_dir)?,
        None => base,
    };
    if let Ok(origin) = std::env::var("WFTUI_BASE_URL")
        && !origin.trim().is_empty()
    {
        site.origin = origin.trim().trim_end_matches('/').to_string();
    }
    if let Ok(id) = std::env::var("WFTUI_OAUTH_CLIENT_ID")
        && !id.trim().is_empty()
    {
        site.oauth_client_id = id.trim().to_string();
    }
    site.validate()?;
    Ok(Some(site))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ENV_LOCK;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wftui-site-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Serialises on `ENV_LOCK` and clears the three variables `resolve`
    /// reads, restoring them after — a test must not see a sibling's site.
    fn without_env<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(&str, Option<String>)> = ["WFTUI_SITE", "WFTUI_BASE_URL", "WFTUI_OAUTH_CLIENT_ID"]
            .into_iter()
            .map(|k| (k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            unsafe { std::env::remove_var(k) };
        }
        let out = f();
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        out
    }

    /// The default must never drift: these are the values the client
    /// shipped with before sites were configurable, spelled out literally.
    #[test]
    fn windowsforum_equals_the_shipped_constants() {
        let wf = SiteConfig::windowsforum();
        assert_eq!(wf.name, "windowsforum");
        assert_eq!(wf.origin, "https://windowsforum.com");
        assert_eq!(wf.oauth_client_id, "6014883021104153");
        assert_eq!(wf.scopes, None);
        assert_eq!(
            wf.features,
            Features { xfmg: true, xfrm: true, tuilink: Some(true), drafts_relay: Some(true) }
        );
        assert_eq!(wf.login, LoginMode::Auto);
        assert_eq!(wf.brand.name, "WindowsForum");
        assert_eq!(wf.brand.mark, "WF");
        assert_eq!(wf.brand.bold_prefix, "Windows");
        assert_eq!(wf.brand.split(), ("Windows", "Forum"));
        assert_eq!(wf.brand.chrome_bg, Some(Rgb { r: 0x0F, g: 0x6C, b: 0xBD }));
        assert_eq!(wf.brand.logo, None);
        assert_eq!(
            wf.quick,
            vec![
                QuickNode { key: 'n', label: "Windows News".into(), node_id: 4 },
                QuickNode { key: 's', label: "Security Alerts".into(), node_id: 84 },
                QuickNode { key: 't', label: "Windows Tutorials".into(), node_id: 305 },
            ]
        );
        assert_eq!(wf.bot_user_ids, vec![125_694]);
        assert_eq!(wf.prefix_strip, vec!["Windows ".to_string()]);
        assert_eq!(wf.host(), "windowsforum.com");
        assert!(wf.is_builtin());
        wf.validate().unwrap();
        assert_eq!(SiteConfig::default(), wf);
    }

    #[test]
    fn effective_scopes_follow_features_unless_explicit() {
        let wf = SiteConfig::windowsforum();
        let scopes = wf.effective_scopes();
        assert_eq!(scopes.len(), 14);
        assert!(scopes.iter().any(|s| s == "media:read") && scopes.iter().any(|s| s == "resource:read"));
        for s in BASE_SCOPES {
            assert!(scopes.iter().any(|x| x == s), "missing {s}");
        }
        let mut plain = SiteConfig::blank("plain");
        assert_eq!(plain.effective_scopes(), BASE_SCOPES.map(String::from).to_vec());
        plain.features.xfrm = true;
        assert!(plain.effective_scopes().ends_with(&["resource:read".to_string()]));
        plain.scopes = Some(vec!["thread:read".into()]);
        assert_eq!(plain.effective_scopes(), vec!["thread:read".to_string()]);
    }

    #[test]
    fn user_agent_is_wf_only_for_the_builtin_site() {
        let wf = SiteConfig::windowsforum().user_agent();
        assert_eq!(wf, config::user_agent());
        assert!(wf.contains("+https://windowsforum.com"));
        let other = SiteConfig::blank("other").user_agent();
        assert!(other.starts_with("wftui/"), "{other}");
        assert!(other.contains(PROJECT_URL), "{other}");
        assert!(!other.contains("windowsforum"), "{other}");
    }

    #[test]
    fn load_missing_file_is_builtin_only() {
        let dir = scratch("missing");
        let cfg = Config::load(&dir.join("config.json")).unwrap();
        assert!(cfg.sites.is_empty() && cfg.default_site.is_none());
        if HAS_BUILTIN_SITE {
            assert_eq!(cfg.site_names(), vec![BUILTIN_NAME.to_string()]);
            let site = without_env(|| resolve(&cfg, None, &dir).unwrap());
            assert_eq!(site, SiteConfig::windowsforum(), "no file, no env: exactly the built-in site");
        } else {
            assert!(cfg.site_names().is_empty());
        }
    }

    /// The generic edition has nothing to fall back on: with no file and
    /// nothing selecting a site, `resolve_or_setup` says so (`None`) and
    /// `resolve` is an error that names the fix. The built-in site's data is
    /// still reachable by name in both editions.
    #[test]
    fn resolve_without_a_site_is_none_only_in_the_generic_edition() {
        let dir = scratch("none");
        let cfg = Config::default();
        without_env(|| {
            let fallback = resolve_or_setup(&cfg, None, &dir).unwrap();
            assert_eq!(fallback.is_some(), HAS_BUILTIN_SITE);
            if !HAS_BUILTIN_SITE {
                let err = resolve(&cfg, None, &dir).unwrap_err().to_string();
                assert!(err.contains("no site configured") && err.contains("config.json"), "{err}");
            }
            assert_eq!(resolve(&cfg, Some("windowsforum"), &dir).unwrap(), SiteConfig::windowsforum());
        });
        assert_eq!(PRODUCT_NAME, if HAS_BUILTIN_SITE { "Forum Terminal (TUI)" } else { "Terminal for XenForo" });
        assert_eq!(EDITION_SUFFIX, if HAS_BUILTIN_SITE { "" } else { "-xf" });
    }

    #[test]
    fn placeholder_is_never_valid() {
        let p = SiteConfig::placeholder();
        assert!(p.is_placeholder());
        assert!(!SiteConfig::windowsforum().is_placeholder());
        assert!(!SiteConfig::blank("setup").is_placeholder(), "the name alone is not the marker");
        assert_eq!(p.brand.name, PRODUCT_NAME);
        assert_eq!(p.brand.mark, "XF");
        assert!(p.validate().is_err() || p.oauth_client_id == "-", "never written as a real site");
        assert!(!p.is_builtin());
    }

    #[test]
    fn load_valid_file_merges_over_builtin() {
        let dir = scratch("merge");
        let path = dir.join("config.json");
        std::fs::write(&path, Config::example_json()).unwrap();
        let cfg = Config::load(&path).unwrap();
        if HAS_BUILTIN_SITE {
            assert_eq!(cfg.default_site.as_deref(), Some("windowsforum"));
            assert_eq!(cfg.site_names(), vec!["windowsforum".to_string(), "example".to_string()]);
        } else {
            assert_eq!(cfg.default_site.as_deref(), Some("example"));
            assert_eq!(cfg.site_names(), vec!["example".to_string()]);
        }
        without_env(|| {
            if HAS_BUILTIN_SITE {
                assert_eq!(resolve(&cfg, None, &dir).unwrap(), SiteConfig::windowsforum());
            }
            let ex = resolve(&cfg, Some("example"), &dir).unwrap();
            assert_eq!(ex.origin, "https://forum.example.com");
            assert_eq!(ex.oauth_client_id, "PASTE-CLIENT-ID");
            assert_eq!(ex.brand.split(), ("Example", "Forum"));
            assert_eq!(ex.brand.mark, "EX");
            assert_eq!(ex.brand.chrome_bg, Some(Rgb { r: 0x3A, g: 0x7D, b: 0x44 }));
            assert_eq!(ex.features, Features { xfmg: false, xfrm: false, tuilink: None, drafts_relay: None });
            assert_eq!(ex.quick, vec![QuickNode { key: 'n', label: "News".into(), node_id: 12 }]);
            assert_eq!(ex.effective_scopes().len(), 12);
            assert!(!ex.is_builtin());
        });

        // Overriding one field of the built-in site keeps every other.
        std::fs::write(
            &path,
            r#"{"sites":[{"name":"windowsforum","brand":{"mark":"W"},"features":{"xfmg":false},"quick":[]}]}"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        let site = without_env(|| resolve(&cfg, None, &dir).unwrap());
        assert_eq!(site.brand.mark, "W");
        assert_eq!(site.brand.name, "WindowsForum");
        assert!(!site.features.xfmg && site.features.xfrm);
        assert!(site.quick.is_empty());
        assert_eq!(site.oauth_client_id, "6014883021104153");
        assert_eq!(site.effective_scopes().len(), 13);
    }

    #[test]
    fn load_corrupt_file_names_path_and_line() {
        let dir = scratch("corrupt");
        let path = dir.join("config.json");
        std::fs::write(&path, "{\n  \"sites\": [ {\"name\": \"x\",}\n]}").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("config.json") && err.contains("line 2"), "{err}");
        std::fs::write(&path, r#"{"sites":[{"name":"a"},{"name":"a"}]}"#).unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("listed twice"), "{err}");
        std::fs::create_dir_all(dir.join("dir.json")).unwrap();
        assert!(Config::load(&dir.join("dir.json")).is_err(), "a directory is not a missing file");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = scratch("unknown");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"future_key": 1, "sites":[{"name":"x","origin":"https://x.example","oauth_client_id":"1","later":{"a":1},"brand":{"name":"X","emoji":"no"}}]}"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        let site = without_env(|| resolve(&cfg, Some("x"), &dir).unwrap());
        assert_eq!(site.brand.name, "X");
        assert_eq!(site.brand.mark, "X", "mark defaults from the slug");
    }

    #[test]
    fn env_overrides_beat_the_file() {
        let dir = scratch("env");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"sites":[{"name":"x","origin":"https://x.example","oauth_client_id":"file-id"}]}"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        without_env(|| {
            unsafe { std::env::set_var("WFTUI_BASE_URL", "http://127.0.0.1:1/") };
            unsafe { std::env::set_var("WFTUI_OAUTH_CLIENT_ID", "env-id") };
            let site = resolve(&cfg, Some("x"), &dir).unwrap();
            assert_eq!(site.origin, "http://127.0.0.1:1");
            assert_eq!(site.oauth_client_id, "env-id");
            let wf = resolve(&cfg, Some("windowsforum"), &dir).unwrap();
            assert_eq!(wf.origin, "http://127.0.0.1:1", "staging via env still works with no entry");
            assert_eq!(wf.user_agent(), config::user_agent(), "and it is still the WF site");
        });
    }

    #[test]
    fn select_by_arg_then_env_then_default_then_only_site() {
        let dir = scratch("select");
        let path = dir.join("config.json");
        let two = r#"{"default_site":"b","sites":[
            {"name":"a","origin":"https://a.example","oauth_client_id":"1"},
            {"name":"b","origin":"https://b.example","oauth_client_id":"2"}]}"#;
        std::fs::write(&path, two).unwrap();
        let cfg = Config::load(&path).unwrap();
        without_env(|| {
            assert_eq!(resolve(&cfg, Some("a"), &dir).unwrap().name, "a");
            assert_eq!(resolve(&cfg, None, &dir).unwrap().name, "b", "default_site");
            unsafe { std::env::set_var("WFTUI_SITE", "a") };
            assert_eq!(resolve(&cfg, None, &dir).unwrap().name, "a", "env beats default_site");
            assert_eq!(resolve(&cfg, Some("b"), &dir).unwrap().name, "b", "arg beats env");
            unsafe { std::env::remove_var("WFTUI_SITE") };
            let err = resolve(&cfg, Some("nope"), &dir).unwrap_err().to_string();
            assert!(err.contains("unknown site") && err.contains("a, b"), "{err}");
            assert_eq!(err.contains("windowsforum"), HAS_BUILTIN_SITE, "{err}");
        });
        std::fs::write(&path, r#"{"sites":[{"name":"only","origin":"https://o.example","oauth_client_id":"1"}]}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        without_env(|| {
            assert_eq!(resolve(&cfg, None, &dir).unwrap().name, "only", "the only listed site wins over the built-in");
            assert_eq!(resolve(&cfg, Some("windowsforum"), &dir).unwrap().name, "windowsforum", "but the built-in is always reachable");
        });
    }

    #[test]
    fn quick_keys_must_be_unique_lowercase_and_unreserved() {
        let mut site = SiteConfig::blank("q");
        site.origin = "https://q.example".into();
        site.oauth_client_id = "1".into();
        site.validate().unwrap();
        let q = |k: char| QuickNode { key: k, label: "L".into(), node_id: 1 };
        site.quick = vec![q('m')];
        assert!(site.validate().unwrap_err().to_string().contains("reserved"));
        site.quick = vec![q('N')];
        assert!(site.validate().unwrap_err().to_string().contains("lowercase"));
        site.quick = vec![q('n'), q('n')];
        assert!(site.validate().unwrap_err().to_string().contains("twice"));
        site.quick = (0..10).map(|i| q((b'b' + i as u8) as char)).collect();
        assert!(site.validate().unwrap_err().to_string().contains("at most 9"));
        site.quick = vec![QuickNode { key: 'n', label: "".into(), node_id: 1 }];
        assert!(site.validate().is_err());
        site.quick = vec![q('n'), q('o'), q('s')];
        site.validate().unwrap();
    }

    #[test]
    fn mark_is_ascii_and_at_most_four_cells() {
        let mut site = SiteConfig::blank("m");
        site.origin = "https://m.example".into();
        site.oauth_client_id = "1".into();
        for bad in ["", "ABCDE", "日本", "A B"] {
            site.brand.mark = bad.into();
            assert!(site.validate().is_err(), "{bad:?}");
        }
        for good in ["X", "WF", "ABCD", "x-1"] {
            site.brand.mark = good.into();
            site.validate().unwrap();
        }
    }

    #[test]
    fn bold_prefix_must_prefix_name() {
        let mut site = SiteConfig::blank("b");
        site.origin = "https://b.example".into();
        site.oauth_client_id = "1".into();
        site.brand.name = "日本フォーラム".into();
        site.brand.bold_prefix = "日本".into();
        site.validate().unwrap();
        assert_eq!(site.brand.split(), ("日本", "フォーラム"));
        site.brand.bold_prefix = "本".into();
        assert!(site.validate().is_err());
        site.brand.bold_prefix.clear();
        assert_eq!(site.brand.split(), ("", "日本フォーラム"));
        // Renaming the built-in brand without renaming the prefix drops it
        // rather than failing.
        let dir = scratch("bold");
        let raw: RawSite = serde_json::from_str(r#"{"name":"windowsforum","brand":{"name":"Elsewhere"}}"#).unwrap();
        let site = raw.apply(SiteConfig::windowsforum(), &dir).unwrap();
        assert_eq!(site.brand.split(), ("", "Elsewhere"));
    }

    #[test]
    fn example_json_round_trips() {
        let text = Config::example_json();
        let cfg: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg.sites.len(), if HAS_BUILTIN_SITE { 2 } else { 1 });
        let dir = scratch("example");
        without_env(|| {
            for name in cfg.site_names() {
                resolve(&cfg, Some(&name), &dir).unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        });
        assert!(text.contains("PASTE-CLIENT-ID") && text.contains("_comment"));
    }

    #[test]
    fn logo_and_colour_are_validated_and_resolved() {
        let dir = scratch("logo");
        let raw: RawSite = serde_json::from_str(
            r#"{"name":"l","origin":"https://l.example","oauth_client_id":"1","brand":{"logo":"logo.png","chrome_bg":"rgb(1, 2, 3)"}}"#,
        )
        .unwrap();
        let site = raw.apply(SiteConfig::blank("l"), &dir).unwrap();
        assert_eq!(site.brand.logo, Some(dir.join("logo.png")));
        assert_eq!(site.brand.chrome_bg, Some(Rgb { r: 1, g: 2, b: 3 }));
        let raw: RawSite = serde_json::from_str(r#"{"name":"l","brand":{"chrome_bg":"blueish"}}"#).unwrap();
        assert!(raw.apply(SiteConfig::blank("l"), &dir).unwrap_err().to_string().contains("CSS colour"));
        assert_eq!(LoginMode::Auto.next().next().next().next(), LoginMode::Auto);
        assert_eq!(serde_json::from_str::<LoginMode>("\"paste\"").unwrap(), LoginMode::Paste);
    }
}
