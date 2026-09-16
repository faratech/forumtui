//! Self-update, ported from htop-win's `installer.rs` (#722).
//!
//! Two halves, both here because both are plain files and HTTP:
//!
//! - **Background, from the running client**: [`check_and_stage`] asks the
//!   release feed for the latest version, downloads the bare binary for this
//!   platform, verifies it against the release's `SHA256SUMS.txt` and its
//!   own PE/ELF header, and *stages* it under `<config dir>/update/` — a
//!   complete `pending-<id>/` directory published by one same-volume rename,
//!   so a reader never sees half of one.
//! - **At the next start, before the terminal is touched**:
//!   [`apply_pending_update`] swaps the newest staged binary over the one
//!   that is running. Linux writes `wftui.new` beside it and renames over
//!   (the install-then-rename swap from CLAUDE.md — a plain copy over a
//!   running binary is "Text file busy"). Windows cannot overwrite a running
//!   image but can *rename* it, so it goes aside as `wftui.exe.old`, the new
//!   one moves in, and the old one is deleted when nothing holds it any more.
//!
//! Nothing here relaunches the client: the session that applied an update is
//! still the old image, says so, and skips its own check.
//!
//! The feed is off-origin (GitHub), so none of this goes through the forum's
//! gates and none of it may ever carry the bearer token — it builds its own
//! short-lived [`http::build`] client, the way the login flow does.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::config;
use crate::error::{Error, Result};
use crate::http;
use crate::token;

/// GitHub's `releases/latest` for the client's own repository.
pub const DEFAULT_FEED_URL: &str = "https://api.github.com/repos/faratech/forumtui/releases/latest";
/// The background check waits this long after start so it never competes
/// with the bootstrap fetches for the first paint.
pub const STARTUP_DELAY: Duration = Duration::from_secs(3);
/// Unauthenticated GitHub allows 60 requests an hour per address; a reader
/// who restarts often must not spend them. A manual check ignores this.
pub const MIN_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// A stripped release binary is ~8 MiB; anything under this is a CDN error
/// page or a truncated body, never wftui.
pub const MIN_BINARY_BYTES: usize = 1024 * 1024;
/// Hard cap on the streamed download, like `fetch_bytes`'s.
pub const MAX_BINARY_BYTES: usize = 64 * 1024 * 1024;
/// The one checksum asset a release carries, covering every other asset.
pub const SUMS_ASSET: &str = "SHA256SUMS.txt";
pub const MAX_SUMS_BYTES: usize = 64 * 1024;
/// A `.stage-*` directory older than this belongs to a process that died
/// mid-write; it is removed on the next pass.
const ABANDONED_STAGE_AGE: Duration = Duration::from_secs(60 * 60);
const META_NAME: &str = "meta";
const LAST_CHECK_NAME: &str = "last-check";

// ------------------------------------------------------------------ version

/// A SemVer version: numeric core plus pre-release identifiers. Build
/// metadata is parsed and dropped, as the spec says it must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

/// Strict `MAJOR.MINOR.PATCH[-pre][+build]`, with an optional leading `v`
/// because that is how release tags are spelled.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let (s, _build) = s.split_once('+').unwrap_or((s, ""));
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (s, None),
    };
    let mut parts = core.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    let pre = match pre {
        None => Vec::new(),
        Some(p) => {
            let ids: Vec<String> = p.split('.').map(str::to_string).collect();
            if ids.iter().any(|id| {
                id.is_empty()
                    || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || (id.len() > 1 && id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()))
            }) {
                return None;
            }
            ids
        }
    };
    Some(Version { core: out, pre })
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match self.core.cmp(&other.core) {
            Equal => {}
            o => return o,
        }
        // A release outranks any pre-release of the same core.
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Equal,
            (true, false) => return Greater,
            (false, true) => return Less,
            (false, false) => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                // Numeric identifiers always rank below alphanumeric ones.
                (Ok(_), Err(_)) => Less,
                (Err(_), Ok(_)) => Greater,
                (Err(_), Err(_)) => a.cmp(b),
            };
            if o != Equal {
                return o;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// True only when both parse and `candidate` is strictly newer; anything
/// unparsable is never an update.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

// --------------------------------------------------------------------- feed

/// GitHub's release object, the fields the updater reads. Every field is
/// `#[serde(default)]` per the models rule; the whole captured body lives in
/// `testdata/github_release_latest.json`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Release {
    pub tag_name: String,
    pub name: String,
    pub html_url: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub content_type: String,
}

/// The platforms a release carries a bare binary for. Names follow the
/// packaging scripts: Linux uses the Rust arch, Windows the MSIX one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    LinuxX86_64,
    LinuxAarch64,
    WindowsX64,
    WindowsX86,
    WindowsArm64,
}

impl Target {
    /// The platform this binary was built for, or `None` where no release
    /// asset exists (macOS, a BSD, a 32-bit Linux) and the updater stays off.
    pub fn host() -> Option<Target> {
        if cfg!(target_os = "linux") {
            if cfg!(target_arch = "x86_64") {
                Some(Target::LinuxX86_64)
            } else if cfg!(target_arch = "aarch64") {
                Some(Target::LinuxAarch64)
            } else {
                None
            }
        } else if cfg!(target_os = "windows") {
            if cfg!(target_arch = "x86_64") {
                Some(Target::WindowsX64)
            } else if cfg!(target_arch = "x86") {
                Some(Target::WindowsX86)
            } else if cfg!(target_arch = "aarch64") {
                Some(Target::WindowsArm64)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// `linux-x86_64`, `windows-arm64` — the platform half of an asset name,
    /// also what a staged generation's `meta` records.
    pub fn arch_tag(self) -> &'static str {
        match self {
            Target::LinuxX86_64 => "linux-x86_64",
            Target::LinuxAarch64 => "linux-aarch64",
            Target::WindowsX64 => "windows-x64",
            Target::WindowsX86 => "windows-x86",
            Target::WindowsArm64 => "windows-arm64",
        }
    }

    pub fn is_windows(self) -> bool {
        matches!(
            self,
            Target::WindowsX64 | Target::WindowsX86 | Target::WindowsArm64
        )
    }

    /// The release asset for `version` (without its `v`): `wftui-0.0.2-linux-x86_64`
    /// or `wftui-0.0.2-windows-x64.exe`. This is the contract with
    /// `packaging/` and `release-binaries.yml`.
    pub fn asset_name(self, version: &str) -> String {
        self.asset_name_for(crate::site::EDITION_SUFFIX, version)
    }

    /// [`asset_name`] for an explicit edition: `wftui-0.0.2-linux-x86_64`
    /// for the Forum Terminal (TUI) edition (`""`), `wftui-xf-0.0.2-…` for
    /// Terminal for XenForo (`"-xf"`). Two editions share one release, and
    /// the suffix is what keeps each updater on its own binary.
    pub fn asset_name_for(self, edition_suffix: &str, version: &str) -> String {
        let ext = if self.is_windows() { ".exe" } else { "" };
        format!("wftui{edition_suffix}-{version}-{}{ext}", self.arch_tag())
    }

    /// The file name a staged generation and the installed binary use.
    fn binary_name(self) -> &'static str {
        if self.is_windows() { "wftui.exe" } else { "wftui" }
    }
}

/// The version a tag names (`v0.0.2` → `0.0.2`).
pub fn version_of_tag(tag: &str) -> &str {
    tag.trim().strip_prefix('v').unwrap_or(tag.trim())
}

/// The binary for `target` and the checksum file, out of a release's assets.
pub fn pick_assets(release: &Release, target: Target) -> Result<(Asset, Asset)> {
    let version = version_of_tag(&release.tag_name);
    let wanted = target.asset_name(version);
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| Error::Config(format!("release {} has no asset {name}", release.tag_name)))
    };
    Ok((find(&wanted)?, find(SUMS_ASSET)?))
}

// ------------------------------------------------------------- verification

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `sha256sum` output: `<hex>  <name>` (two spaces) or `<hex> *<name>`
/// (binary mode). Lines that are not that are skipped rather than fatal —
/// a comment or a blank line must not hide the checksums after it.
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            let (hex, rest) = line.split_once(' ')?;
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let name = rest.trim_start_matches([' ', '*']);
            (!name.is_empty()).then(|| (hex.to_ascii_lowercase(), name.to_string()))
        })
        .collect()
}

pub fn expected_sha256<'a>(sums: &'a [(String, String)], name: &str) -> Option<&'a str> {
    sums.iter().find(|(_, n)| n == name).map(|(h, _)| h.as_str())
}

/// Reject anything that is not plausibly a wftui binary for `target`: a CDN
/// error page, a truncated body, or the wrong architecture's build. This is
/// the shape check; integrity is the checksum in [`verify`].
pub fn sniff_binary(bytes: &[u8], target: Target) -> Result<()> {
    if bytes.len() < MIN_BINARY_BYTES {
        return Err(Error::FetchRejected(format!(
            "binary too small ({} bytes) to be wftui",
            bytes.len()
        )));
    }
    let machine = |m: u16, want: u16, what: &str| {
        if m == want {
            Ok(())
        } else {
            Err(Error::FetchRejected(format!(
                "{what} machine 0x{m:04x} is not {} (0x{want:04x})",
                target.arch_tag()
            )))
        }
    };
    if target.is_windows() {
        if &bytes[0..2] != b"MZ" {
            return Err(Error::FetchRejected("missing MZ header".into()));
        }
        let e_lfanew = u32::from_le_bytes([bytes[0x3c], bytes[0x3d], bytes[0x3e], bytes[0x3f]]) as usize;
        let Some(hdr) = bytes.get(e_lfanew..e_lfanew + 6) else {
            return Err(Error::FetchRejected("PE header out of bounds".into()));
        };
        if &hdr[..4] != b"PE\0\0" {
            return Err(Error::FetchRejected("missing PE signature".into()));
        }
        let m = u16::from_le_bytes([hdr[4], hdr[5]]);
        let want = match target {
            Target::WindowsX64 => 0x8664,
            Target::WindowsX86 => 0x014c,
            _ => 0xaa64,
        };
        machine(m, want, "PE")
    } else {
        if &bytes[0..4] != b"\x7fELF" {
            return Err(Error::FetchRejected("missing ELF magic".into()));
        }
        let raw = [bytes[18], bytes[19]];
        // EI_DATA: 1 = little-endian, 2 = big-endian.
        let m = if bytes[5] == 2 {
            u16::from_be_bytes(raw)
        } else {
            u16::from_le_bytes(raw)
        };
        let want = match target {
            Target::LinuxX86_64 => 0x3e,
            _ => 0xb7,
        };
        machine(m, want, "ELF")
    }
}

/// The whole gate a download passes before it is staged: shape, then the
/// checksum the release published for `asset_name`. Returns the hex digest.
pub fn verify(bytes: &[u8], target: Target, sums_text: &str, asset_name: &str) -> Result<String> {
    sniff_binary(bytes, target)?;
    let sums = parse_sums(sums_text);
    let want = expected_sha256(&sums, asset_name)
        .ok_or_else(|| Error::FetchRejected(format!("{SUMS_ASSET} has no line for {asset_name}")))?;
    let got = sha256_hex(bytes);
    if got != want {
        return Err(Error::FetchRejected(format!(
            "sha256 mismatch for {asset_name}: got {got}, release says {want}"
        )));
    }
    Ok(got)
}

// ------------------------------------------------------------------ staging

/// One staged generation: `dir/wftui[.exe]` plus `dir/meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub version: String,
    pub path: PathBuf,
    pub dir: PathBuf,
}

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_generation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

fn write_synced(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(contents)?;
    f.sync_all()
}

fn meta_text(version: &str, target: Target, sha256: &str) -> String {
    format!("version={version}\narch={}\nsha256={sha256}\n", target.arch_tag())
}

fn meta_field<'a>(meta: &'a str, key: &str) -> Option<&'a str> {
    meta.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// Write the pair into a private `.stage-<id>/`, then publish it with one
/// rename to `pending-<id>/`. Both are children of `root`, so the rename is
/// same-volume and atomic; a reader lists only complete generations.
pub fn stage_pending_in(
    root: &Path,
    version: &str,
    target: Target,
    sha256: &str,
    body: &[u8],
) -> Result<Pending> {
    if parse_version(version).is_none() {
        return Err(Error::Config(format!("release version is not SemVer: {version}")));
    }
    std::fs::create_dir_all(root)?;
    let id = unique_generation_id();
    let staging = root.join(format!(".stage-{id}"));
    let generation = root.join(format!("pending-{id}"));
    std::fs::create_dir(&staging)?;
    let binary = staging.join(target.binary_name());
    let result = (|| -> std::io::Result<()> {
        write_synced(&binary, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
        }
        write_synced(&staging.join(META_NAME), meta_text(version, target, sha256).as_bytes())?;
        std::fs::rename(&staging, &generation)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::Io(e));
    }
    Ok(Pending {
        version: version.to_string(),
        path: generation.join(target.binary_name()),
        dir: generation,
    })
}

/// Every complete generation under `root` that is still worth applying over
/// `current`. Anything else is removed here: abandoned `.stage-*` dirs, a
/// `pending-*` whose meta is unreadable, is for another platform, whose
/// bytes no longer match their recorded digest, or whose version is not
/// newer than the running one (a downgrade must never be re-applied after
/// the reader installed an older build on purpose).
pub fn load_pending(root: &Path, target: Target, current: &str) -> Result<Vec<Pending>> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        if name.starts_with(".stage-") {
            let abandoned = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > ABANDONED_STAGE_AGE);
            if abandoned {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if !name.starts_with("pending-") {
            continue;
        }
        match read_generation(&path, target, current) {
            Some(p) => out.push(p),
            None => {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    Ok(out)
}

fn read_generation(dir: &Path, target: Target, current: &str) -> Option<Pending> {
    let meta = std::fs::read_to_string(dir.join(META_NAME)).ok()?;
    let version = meta_field(&meta, "version")?;
    if meta_field(&meta, "arch")? != target.arch_tag() || !is_newer(version, current) {
        return None;
    }
    let want = meta_field(&meta, "sha256")?;
    let path = dir.join(target.binary_name());
    let bytes = std::fs::read(&path).ok()?;
    if sniff_binary(&bytes, target).is_err() || sha256_hex(&bytes) != want {
        return None;
    }
    Some(Pending {
        version: version.to_string(),
        path,
        dir: dir.to_path_buf(),
    })
}

/// The highest version staged; ties go to whichever was listed first.
pub fn newest_pending(pending: &[Pending]) -> Option<&Pending> {
    pending
        .iter()
        .filter_map(|p| parse_version(&p.version).map(|v| (v, p)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, p)| p)
}

/// Cross-process exclusion over `root`: staging, pruning and applying all
/// hold it. An flock rather than htop-win's named mutex, because it is one
/// primitive on both platforms, the kernel drops it with the process, and
/// it is already how the token and draft stores exclude each other.
pub struct UpdateLock(#[allow(dead_code)] token::StoreLock);

impl UpdateLock {
    fn anchor(root: &Path) -> PathBuf {
        root.join("staging")
    }

    pub fn acquire(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        token::lock_store(&Self::anchor(root)).map(UpdateLock)
    }

    /// `Ok(None)` when another wftui holds it. Start-up uses this so a sibling
    /// mid-download can never stall this instance's boot.
    pub fn try_acquire(root: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(root)?;
        Ok(token::try_lock_store(&Self::anchor(root))?.map(UpdateLock))
    }
}

// ------------------------------------------------------------------- install

/// How the staged binary reaches the installed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Unix: write `wftui.new` beside the binary and rename over it. The
    /// running process keeps its old inode.
    RenameOver,
    /// Windows: a running image cannot be overwritten but can be renamed, so
    /// it goes aside as `wftui.exe.old` first.
    RenameAside,
}

/// Why this binary will not be replaced in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Installed from the MSIX: the package directory is read-only and the
    /// Store or the release page updates it.
    Msix,
    /// Running out of a cargo `target/` directory — a development build,
    /// which would be replaced under the compiler's feet.
    CargoTarget,
    /// No release asset exists for this platform.
    Unsupported(String),
    /// The binary's directory is not writable by this user; the manual
    /// install-then-rename command is the way.
    ReadOnly(PathBuf),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Msix => write!(f, "installed from the MSIX package; update from the release page"),
            Refusal::CargoTarget => write!(f, "running from a cargo target directory"),
            Refusal::Unsupported(what) => write!(f, "no release binary for {what}"),
            Refusal::ReadOnly(p) => write!(f, "{} is not writable", p.display()),
        }
    }
}

/// Decide the swap strategy for the binary at `exe`, or why there is none.
pub fn classify_exe(exe: &Path) -> std::result::Result<Install, Refusal> {
    classify_exe_as(exe, cfg!(windows))
}

/// The platform-independent half of [`classify_exe`], so both branches are
/// tested wherever the suite runs.
pub fn classify_exe_as(exe: &Path, windows: bool) -> std::result::Result<Install, Refusal> {
    let lossy = exe.to_string_lossy().to_ascii_lowercase();
    let parent = exe.parent();
    if lossy.contains("\\windowsapps\\")
        || lossy.contains("/windowsapps/")
        || parent.is_some_and(|p| p.join("AppxManifest.xml").exists())
    {
        return Err(Refusal::Msix);
    }
    let under_cargo_target = exe.ancestors().skip(1).any(|dir| {
        dir.file_name().is_some_and(|n| n == "target")
            && dir.parent().is_some_and(|p| p.join("Cargo.toml").exists())
    });
    if under_cargo_target {
        return Err(Refusal::CargoTarget);
    }
    Ok(if windows { Install::RenameAside } else { Install::RenameOver })
}

/// Can this user create a file beside `exe`? Probed with a real create
/// rather than mode bits, which lie under ACLs, containers and root squash.
pub fn install_dir_writable(exe: &Path) -> bool {
    let Some(dir) = exe.parent() else {
        return false;
    };
    let probe = dir.join(format!(".wftui-update-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The line a reader runs when the install directory is not theirs — the
/// same install-then-rename swap this module performs, spelled for a shell.
pub fn manual_install_command(staged: &Path, exe: &Path) -> String {
    let (staged, exe) = (staged.display(), exe.display());
    if cfg!(windows) {
        format!("move /y \"{exe}\" \"{exe}.old\" && copy /y \"{staged}\" \"{exe}\"")
    } else {
        format!("sudo install -m755 '{staged}' '{exe}.new' && sudo mv -f '{exe}.new' '{exe}'")
    }
}

/// Put `staged` at `exe`. Both strategies first materialise the bytes as
/// `<exe>.new` *beside the target* — the config dir and `/usr/local/bin` are
/// usually different filesystems, and a rename never crosses one — then
/// finish with renames only.
pub fn install_over(staged: &Path, exe: &Path, how: Install) -> std::io::Result<()> {
    let new = token::sibling_with_suffix(exe, "new");
    let _ = std::fs::remove_file(&new);
    std::fs::copy(staged, &new)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Ok(f) = std::fs::File::open(&new) {
        let _ = f.sync_all();
    }
    match how {
        Install::RenameOver => std::fs::rename(&new, exe).inspect_err(|_| {
            let _ = std::fs::remove_file(&new);
        }),
        Install::RenameAside => {
            let old = token::sibling_with_suffix(exe, "old");
            let _ = std::fs::remove_file(&old);
            if exe.exists()
                && let Err(e) = std::fs::rename(exe, &old)
            {
                let _ = std::fs::remove_file(&new);
                return Err(e);
            }
            match std::fs::rename(&new, exe) {
                Ok(()) => {
                    // Usually still mapped by the running image; the next
                    // start's `NothingPending` pass deletes it.
                    let _ = std::fs::remove_file(&old);
                    Ok(())
                }
                Err(e) => {
                    let _ = std::fs::rename(&old, exe);
                    let _ = std::fs::remove_file(&new);
                    Err(e)
                }
            }
        }
    }
}

/// What the start-up pass did. Only `Applied` may tell the session to skip
/// its own check: everything else means the running binary might still be
/// stale (htop-win #82 — a lost lock race once silenced checks for a whole
/// session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied(String),
    NothingPending,
    Refused { version: String, why: Refusal },
    Incomplete(String),
}

impl ApplyOutcome {
    pub fn just_applied(&self) -> Option<&str> {
        match self {
            ApplyOutcome::Applied(v) => Some(v),
            _ => None,
        }
    }
}

/// Swap the newest staged binary over the running one. Called first thing
/// in `main`, before the terminal or the graphics probe, with the lock
/// *tried* rather than waited for.
pub fn apply_pending_update(cfg: &UpdateConfig) -> ApplyOutcome {
    if cfg.disabled {
        return ApplyOutcome::NothingPending;
    }
    let (Some(exe), Some(target)) = (&cfg.exe, cfg.target) else {
        return ApplyOutcome::NothingPending;
    };
    if !cfg.root.exists() {
        return ApplyOutcome::NothingPending;
    }
    let _lock = match UpdateLock::try_acquire(&cfg.root) {
        Ok(Some(l)) => l,
        Ok(None) => return ApplyOutcome::Incomplete("another wftui is staging an update".into()),
        Err(e) => return ApplyOutcome::Incomplete(format!("cannot lock the update dir: {e}")),
    };
    let pending = match load_pending(&cfg.root, target, cfg.current_version) {
        Ok(p) => p,
        Err(e) => return ApplyOutcome::Incomplete(format!("cannot inspect staged updates: {e}")),
    };
    let Some(newest) = newest_pending(&pending).cloned() else {
        // Leftover from a previous Windows swap, unlocked now that the image
        // that held it has exited.
        let _ = std::fs::remove_file(token::sibling_with_suffix(exe, "old"));
        return ApplyOutcome::NothingPending;
    };
    let how = match classify_exe(exe) {
        Ok(how) => how,
        Err(why) => {
            // Nothing staged can ever apply to this binary; keep the dir
            // clean rather than re-refusing at every start.
            if matches!(why, Refusal::Msix | Refusal::CargoTarget) {
                let _ = std::fs::remove_dir_all(&newest.dir);
            }
            return ApplyOutcome::Refused { version: newest.version, why };
        }
    };
    if !install_dir_writable(exe) {
        return ApplyOutcome::Refused {
            version: newest.version,
            why: Refusal::ReadOnly(exe.clone()),
        };
    }
    match install_over(&newest.path, exe, how) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&newest.dir);
            ApplyOutcome::Applied(newest.version)
        }
        Err(e) => ApplyOutcome::Incomplete(format!("installation failed: {e}")),
    }
}

// -------------------------------------------------------------------- check

/// Everything the check needs, resolved once. [`UpdateConfig::from_env`] is
/// the only place the environment is read.
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    pub feed_url: String,
    pub root: PathBuf,
    pub current_version: &'static str,
    pub exe: Option<PathBuf>,
    pub target: Option<Target>,
    pub disabled: bool,
}

impl UpdateConfig {
    /// `current_version` is the *binary's* `env!("CARGO_PKG_VERSION")`, passed
    /// in because this crate's own version is not the release version.
    pub fn from_env(current_version: &'static str) -> Self {
        UpdateConfig {
            feed_url: config::update_feed_url(),
            root: config::update_root(),
            current_version,
            exe: std::env::current_exe().ok(),
            target: Target::host(),
            disabled: config::updates_disabled(),
        }
    }

    pub fn for_test(root: PathBuf, feed_url: String, exe: PathBuf, current_version: &'static str) -> Self {
        UpdateConfig {
            feed_url,
            root,
            current_version,
            exe: Some(exe),
            target: Target::host(),
            disabled: false,
        }
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do, and nothing was asked: disabled, no asset for this
    /// platform, or a development build.
    Skipped(String),
    /// The auto check ran less than `MIN_CHECK_INTERVAL` ago.
    Throttled,
    UpToDate { latest: String },
    /// Downloaded, verified and staged; the next start applies it.
    Staged { version: String, path: PathBuf },
    /// Newer, but this binary cannot be replaced in place (MSIX); nothing was
    /// downloaded and `html_url` is the release page to visit.
    Available { version: String, html_url: String, why: Refusal },
    /// Staged, but the install directory is not this user's: `command` is
    /// the install-then-rename line to run by hand.
    ManualInstall { version: String, staged: PathBuf, command: String },
}

fn last_check_path(root: &Path) -> PathBuf {
    root.join(LAST_CHECK_NAME)
}

/// Seconds since the last completed feed request, if any.
fn since_last_check(root: &Path) -> Option<Duration> {
    let text = std::fs::read_to_string(last_check_path(root)).ok()?;
    let then = text.trim().parse::<u64>().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(now.saturating_sub(then)))
}

fn note_check(root: &Path) {
    if std::fs::create_dir_all(root).is_ok()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        let _ = std::fs::write(last_check_path(root), now.as_secs().to_string());
    }
}

/// A non-2xx from the feed or the CDN. GitHub answers an exhausted
/// unauthenticated quota with 403 as often as 429, so both are the quiet
/// rate-limit error the app already knows how to sit out.
async fn feed_error(resp: reqwest::Response, what: &str) -> Error {
    let status = resp.status().as_u16();
    if status == 429 || status == 403 {
        const MAX_RETRY_AFTER_SECS: u64 = 3600;
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)));
        let _ = resp.bytes().await;
        return Error::RateLimited { retry_after };
    }
    let _ = resp.bytes().await;
    Error::Config(format!("{what} answered http {status}"))
}

/// GET `url` with a hard byte cap, streamed so an oversized body is cut off
/// rather than buffered. Never carries a token: the feed is not our origin.
async fn fetch_capped(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    timeout: Duration,
    what: &str,
) -> Result<Vec<u8>> {
    let mut resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .timeout(timeout)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(feed_error(resp, what).await);
    }
    if let Some(len) = resp.content_length()
        && len as usize > max_bytes
    {
        return Err(Error::FetchRejected(format!("{what} is {len} bytes, over the {max_bytes} cap")));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        buf.extend_from_slice(&chunk);
        if buf.len() > max_bytes {
            return Err(Error::FetchRejected(format!("{what} exceeded the {max_bytes} byte cap")));
        }
    }
    Ok(buf)
}

/// Ask the feed, and stage the newer binary if there is one this build can
/// use. `force` is the manual check: it ignores the interval, nothing else.
pub async fn check_and_stage(cfg: &UpdateConfig, force: bool) -> Result<Outcome> {
    if cfg.disabled {
        return Ok(Outcome::Skipped("updates are disabled (WFTUI_NO_UPDATE)".into()));
    }
    let Some(target) = cfg.target else {
        return Ok(Outcome::Skipped(format!(
            "no release binary for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )));
    };
    let Some(exe) = &cfg.exe else {
        return Ok(Outcome::Skipped("the running binary's path is unknown".into()));
    };
    let strategy = classify_exe(exe);
    if strategy == Err(Refusal::CargoTarget) {
        return Ok(Outcome::Skipped(Refusal::CargoTarget.to_string()));
    }
    if !force && since_last_check(&cfg.root).is_some_and(|age| age < MIN_CHECK_INTERVAL) {
        return Ok(Outcome::Throttled);
    }

    let client = http::build()?;
    let resp = client
        .get(&cfg.feed_url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    // Any completed request spends quota; count it whatever it said.
    note_check(&cfg.root);
    if !resp.status().is_success() {
        return Err(feed_error(resp, "release feed").await);
    }
    let release: Release = serde_json::from_slice(&resp.bytes().await?)?;
    if release.draft || release.prerelease {
        return Ok(Outcome::UpToDate { latest: cfg.current_version.to_string() });
    }
    let latest = version_of_tag(&release.tag_name).to_string();
    if !is_newer(&latest, cfg.current_version) {
        let _lock = UpdateLock::acquire(&cfg.root)?;
        // Prunes generations the running version has caught up with.
        let _ = load_pending(&cfg.root, target, cfg.current_version);
        return Ok(Outcome::UpToDate { latest });
    }
    if let Err(why) = strategy {
        return Ok(Outcome::Available { version: latest, html_url: release.html_url, why });
    }
    let already = |root: &Path| -> Result<Option<Pending>> {
        Ok(load_pending(root, target, cfg.current_version)?
            .into_iter()
            .find(|p| p.version == latest))
    };
    {
        let _lock = UpdateLock::acquire(&cfg.root)?;
        if let Some(p) = already(&cfg.root)? {
            return Ok(staged_outcome(p, exe));
        }
    }

    let (binary, sums) = pick_assets(&release, target)?;
    let sums_text = fetch_capped(&client, &sums.browser_download_url, MAX_SUMS_BYTES, config::REQUEST_TIMEOUT, SUMS_ASSET).await?;
    let sums_text = String::from_utf8_lossy(&sums_text).into_owned();
    let body = fetch_capped(&client, &binary.browser_download_url, MAX_BINARY_BYTES, config::DOWNLOAD_TIMEOUT, &binary.name).await?;
    let digest = verify(&body, target, &sums_text, &binary.name)?;

    let _lock = UpdateLock::acquire(&cfg.root)?;
    // A sibling may have staged the same version while this one downloaded.
    if let Some(p) = already(&cfg.root)? {
        return Ok(staged_outcome(p, exe));
    }
    let pending = stage_pending_in(&cfg.root, &latest, target, &digest, &body)?;
    Ok(staged_outcome(pending, exe))
}

fn staged_outcome(p: Pending, exe: &Path) -> Outcome {
    if install_dir_writable(exe) {
        Outcome::Staged { version: p.version, path: p.path }
    } else {
        Outcome::ManualInstall {
            version: p.version,
            command: manual_install_command(&p.path, exe),
            staged: p.path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wftui-update-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A body that passes `sniff_binary` for `target`: the right magic and
    /// machine, padded to the minimum size with a marker so two synthetic
    /// binaries can differ.
    pub(crate) fn synthetic_binary(target: Target, marker: u8) -> Vec<u8> {
        let mut b = vec![marker; MIN_BINARY_BYTES];
        if target.is_windows() {
            b[0..2].copy_from_slice(b"MZ");
            b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
            b[0x80..0x84].copy_from_slice(b"PE\0\0");
            let m: u16 = match target {
                Target::WindowsX64 => 0x8664,
                Target::WindowsX86 => 0x014c,
                _ => 0xaa64,
            };
            b[0x84..0x86].copy_from_slice(&m.to_le_bytes());
        } else {
            b[0..4].copy_from_slice(b"\x7fELF");
            b[5] = 1;
            let m: u16 = if target == Target::LinuxX86_64 { 0x3e } else { 0xb7 };
            b[18..20].copy_from_slice(&m.to_le_bytes());
        }
        b
    }

    fn host() -> Target {
        Target::host().expect("the suite runs on a supported platform")
    }

    #[test]
    fn version_ordering_matches_semver_precedence() {
        let v = |s: &str| parse_version(s).unwrap();
        assert!(v("1.0.0") < v("1.0.1"));
        assert!(v("1.0.9") < v("1.1.0"));
        assert!(v("1.9.9") < v("2.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-alpha.beta") < v("1.0.0-beta"));
        assert!(v("1.0.0-beta.2") < v("1.0.0-beta.11"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert_eq!(v("1.0.0+build.5"), v("1.0.0"), "build metadata is ignored");
        assert_eq!(v("v0.0.2"), v("0.0.2"), "a tag's v is not part of the version");
        assert!(is_newer("0.0.2", "0.0.1"));
        assert!(!is_newer("0.0.1", "0.0.1"));
        assert!(!is_newer("0.0.1", "0.0.2"), "a downgrade is never an update");
        assert!(is_newer("0.1.0", "0.1.0-rc.1"), "a release outranks its own rc");
        assert!(!is_newer("latest", "0.0.1"), "garbage is never an update");
    }

    #[test]
    fn parse_version_rejects_malformed_and_accepts_v_prefix() {
        for bad in ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.2.3-", "1.2.3-a..b", "a.b.c", "1.2.3-01"] {
            assert!(parse_version(bad).is_none(), "{bad:?} must not parse");
        }
        for good in ["0.0.1", "v0.0.1", " v1.2.3 ", "1.2.3-rc.1", "1.2.3-rc.1+sha.abc", "1.2.3+only"] {
            assert!(parse_version(good).is_some(), "{good:?} must parse");
        }
    }

    /// The captured `releases/latest` body (htop-win's, the same shape) must
    /// decode whole — every field the updater reads is present on the wire.
    #[test]
    fn release_fixture_deserializes_the_whole_captured_body() {
        let r: Release = serde_json::from_str(include_str!("testdata/github_release_latest.json")).unwrap();
        assert_eq!(r.tag_name, "v0.2.9");
        assert!(r.html_url.starts_with("https://github.com/"));
        assert!(!r.draft && !r.prerelease);
        assert!(r.assets.iter().any(|a| a.name.ends_with(".exe") && a.size > 0));
        assert!(r.assets.iter().all(|a| a.browser_download_url.starts_with("https://")));
    }

    #[test]
    fn asset_names_follow_the_release_naming_contract() {
        assert_eq!(Target::LinuxX86_64.asset_name_for("", "0.0.2"), "wftui-0.0.2-linux-x86_64");
        assert_eq!(Target::LinuxAarch64.asset_name_for("", "0.0.2"), "wftui-0.0.2-linux-aarch64");
        assert_eq!(Target::WindowsX64.asset_name_for("", "0.0.2"), "wftui-0.0.2-windows-x64.exe");
        assert_eq!(Target::WindowsX86.asset_name_for("", "0.0.2"), "wftui-0.0.2-windows-x86.exe");
        assert_eq!(Target::WindowsArm64.asset_name_for("", "0.0.2"), "wftui-0.0.2-windows-arm64.exe");
        // The generic edition: same shape, its own prefix, so one release can
        // carry both and neither updater takes the other's binary.
        assert_eq!(Target::LinuxX86_64.asset_name_for("-xf", "0.0.2"), "wftui-xf-0.0.2-linux-x86_64");
        assert_eq!(Target::WindowsX64.asset_name_for("-xf", "0.0.2"), "wftui-xf-0.0.2-windows-x64.exe");
        assert_eq!(
            Target::LinuxX86_64.asset_name("0.0.2"),
            Target::LinuxX86_64.asset_name_for(crate::site::EDITION_SUFFIX, "0.0.2"),
            "this build's own name follows its edition"
        );
        assert_eq!(version_of_tag("v0.0.2"), "0.0.2");
        assert_eq!(version_of_tag("0.0.2"), "0.0.2");
    }

    fn release_with(tag: &str, names: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            html_url: "https://example.invalid/r".into(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.invalid/{n}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn pick_assets_finds_binary_and_sums_per_target() {
        // A release carries both editions; this build picks its own.
        let s = crate::site::EDITION_SUFFIX;
        let linux = format!("wftui{s}-0.0.2-linux-x86_64");
        let win = format!("wftui{s}-0.0.2-windows-x64.exe");
        let r = release_with(
            "v0.0.2",
            &[&linux, &win, "SHA256SUMS.txt", "wftui-0.0.2.msixbundle", "wftui-0.0.2-linux-x86_64", "wftui-xf-0.0.2-linux-x86_64"],
        );
        let (bin, sums) = pick_assets(&r, Target::LinuxX86_64).unwrap();
        assert_eq!((bin.name.as_str(), sums.name.as_str()), (linux.as_str(), "SHA256SUMS.txt"));
        let (bin, _) = pick_assets(&r, Target::WindowsX64).unwrap();
        assert_eq!(bin.name, win);
        assert!(pick_assets(&r, Target::WindowsArm64).is_err(), "no arm64 asset in this release");
        let no_sums = release_with("v0.0.2", &[&linux]);
        assert!(pick_assets(&no_sums, Target::LinuxX86_64).is_err(), "a release without sums is unusable");
    }

    #[test]
    fn sums_parse_both_gnu_formats_and_skip_junk() {
        let hex = "a".repeat(64);
        let text = format!(
            "# comment\n\n{hex}  wftui-0.0.2-linux-x86_64\n{HEX} *wftui-0.0.2-windows-x64.exe\nnot a line\nabc  short.bin\n",
            HEX = hex.to_ascii_uppercase()
        );
        let sums = parse_sums(&text);
        assert_eq!(sums.len(), 2);
        assert_eq!(expected_sha256(&sums, "wftui-0.0.2-linux-x86_64"), Some(hex.as_str()));
        assert_eq!(expected_sha256(&sums, "wftui-0.0.2-windows-x64.exe"), Some(hex.as_str()), "lower-cased");
        assert_eq!(expected_sha256(&sums, "short.bin"), None);
    }

    #[test]
    fn sniff_accepts_synthetic_elf_and_pe_only_for_the_matching_machine() {
        for t in [Target::LinuxX86_64, Target::LinuxAarch64, Target::WindowsX64, Target::WindowsX86, Target::WindowsArm64] {
            assert!(sniff_binary(&synthetic_binary(t, 0), t).is_ok(), "{t:?}");
        }
        assert!(sniff_binary(&synthetic_binary(Target::LinuxAarch64, 0), Target::LinuxX86_64).is_err());
        assert!(sniff_binary(&synthetic_binary(Target::WindowsArm64, 0), Target::WindowsX64).is_err());
        assert!(sniff_binary(&synthetic_binary(Target::LinuxX86_64, 0), Target::WindowsX64).is_err(), "an ELF is not a PE");
        assert!(sniff_binary(&synthetic_binary(Target::WindowsX64, 0), Target::LinuxX86_64).is_err(), "a PE is not an ELF");
    }

    #[test]
    fn sniff_rejects_html_truncated_and_undersized_bodies() {
        let html = b"<html><body>Not Found</body></html>".repeat(MIN_BINARY_BYTES / 30);
        assert!(sniff_binary(&html, Target::LinuxX86_64).is_err());
        assert!(sniff_binary(&html, Target::WindowsX64).is_err());
        let mut short = synthetic_binary(host(), 0);
        short.truncate(MIN_BINARY_BYTES - 1);
        assert!(sniff_binary(&short, host()).is_err());
        assert!(sniff_binary(&[], host()).is_err());
        let mut oob = synthetic_binary(Target::WindowsX64, 0);
        oob[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(sniff_binary(&oob, Target::WindowsX64).is_err(), "e_lfanew past the end");
    }

    #[test]
    fn verify_rejects_a_sha_mismatch_and_a_missing_line() {
        let body = synthetic_binary(host(), 1);
        let name = host().asset_name("0.0.2");
        let good = format!("{}  {name}\n", sha256_hex(&body));
        assert_eq!(verify(&body, host(), &good, &name).unwrap(), sha256_hex(&body));
        let wrong = format!("{}  {name}\n", "0".repeat(64));
        assert!(matches!(verify(&body, host(), &wrong, &name), Err(Error::FetchRejected(_))));
        let other = format!("{}  something-else\n", sha256_hex(&body));
        assert!(matches!(verify(&body, host(), &other, &name), Err(Error::FetchRejected(_))));
    }

    #[test]
    fn stage_publishes_a_complete_generation_and_leaves_no_stage_dir() {
        let root = scratch("stage");
        let body = synthetic_binary(host(), 2);
        let p = stage_pending_in(&root, "0.0.2", host(), &sha256_hex(&body), &body).unwrap();
        assert!(p.dir.starts_with(&root));
        assert!(p.dir.file_name().unwrap().to_string_lossy().starts_with("pending-"));
        assert_eq!(std::fs::read(&p.path).unwrap(), body);
        let meta = std::fs::read_to_string(p.dir.join(META_NAME)).unwrap();
        assert!(meta.contains("version=0.0.2\n"));
        assert!(meta.contains(&format!("arch={}\n", host().arch_tag())));
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with(".stage-")), "{names:?}");
        assert!(stage_pending_in(&root, "not-a-version", host(), "", &body).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_stages_do_not_collide() {
        let root = scratch("concurrent");
        let handles: Vec<_> = (0..4u8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let body = synthetic_binary(host(), i);
                    stage_pending_in(&root, &format!("0.0.{}", i + 2), host(), &sha256_hex(&body), &body).unwrap()
                })
            })
            .collect();
        let dirs: std::collections::HashSet<PathBuf> = handles.into_iter().map(|h| h.join().unwrap().dir).collect();
        assert_eq!(dirs.len(), 4);
        let pending = load_pending(&root, host(), "0.0.1").unwrap();
        assert_eq!(pending.len(), 4);
        assert_eq!(newest_pending(&pending).unwrap().version, "0.0.5");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_pending_drops_corrupt_stale_wrong_arch_and_downgrade_generations() {
        let root = scratch("load");
        let body = synthetic_binary(host(), 3);
        let digest = sha256_hex(&body);
        let good = stage_pending_in(&root, "0.0.3", host(), &digest, &body).unwrap();
        let downgrade = stage_pending_in(&root, "0.0.1", host(), &digest, &body).unwrap();
        let same = stage_pending_in(&root, "0.0.2", host(), &digest, &body).unwrap();
        let tampered = stage_pending_in(&root, "0.0.4", host(), &digest, &body).unwrap();
        std::fs::write(&tampered.path, synthetic_binary(host(), 9)).unwrap();
        let wrong_arch = stage_pending_in(&root, "0.0.5", host(), &digest, &body).unwrap();
        std::fs::write(wrong_arch.dir.join(META_NAME), "version=0.0.5\narch=plan9-mips\nsha256=x\n").unwrap();
        let no_meta = stage_pending_in(&root, "0.0.6", host(), &digest, &body).unwrap();
        std::fs::remove_file(no_meta.dir.join(META_NAME)).unwrap();
        let stale_stage = root.join(".stage-stale");
        std::fs::create_dir(&stale_stage).unwrap();
        let fresh_stage = root.join(".stage-fresh");
        std::fs::create_dir(&fresh_stage).unwrap();
        // Back-date the stale one past the abandonment age.
        let old = SystemTime::now() - ABANDONED_STAGE_AGE - Duration::from_secs(60);
        std::fs::File::open(&stale_stage).unwrap().set_modified(old).unwrap();

        let pending = load_pending(&root, host(), "0.0.2").unwrap();
        assert_eq!(pending, vec![good.clone()]);
        assert!(good.dir.exists());
        for gone in [&downgrade, &same, &tampered, &wrong_arch, &no_meta] {
            assert!(!gone.dir.exists(), "{} must be removed", gone.dir.display());
        }
        assert!(!stale_stage.exists(), "an hour-old stage dir is garbage");
        assert!(fresh_stage.exists(), "a fresh stage dir may still be written to");
        assert!(load_pending(&root.join("missing"), host(), "0.0.1").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_exe_refuses_windowsapps_appxmanifest_and_cargo_target() {
        assert_eq!(
            classify_exe_as(Path::new(r"C:\Program Files\WindowsApps\MikeFara.WindowsForumTUI_0.0.1.0_x64__abc\wftui.exe"), true),
            Err(Refusal::Msix)
        );
        let root = scratch("classify");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("AppxManifest.xml"), "<Package/>").unwrap();
        assert_eq!(classify_exe_as(&pkg.join("wftui.exe"), true), Err(Refusal::Msix));
        let checkout = root.join("checkout");
        std::fs::create_dir_all(checkout.join("target").join("release")).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]").unwrap();
        assert_eq!(classify_exe_as(&checkout.join("target/release/wftui"), false), Err(Refusal::CargoTarget));
        let bin = checkout.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        assert_eq!(classify_exe_as(&bin.join("wftui"), false), Ok(Install::RenameOver), "bin/wftui in a checkout is a real install");
        let plain = root.join("target").join("wftui");
        std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
        assert_eq!(classify_exe_as(&plain, false), Ok(Install::RenameOver), "a dir merely named target is not cargo's");
        assert_eq!(classify_exe_as(&plain, true), Ok(Install::RenameAside));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_over_rename_over_swaps_the_path_and_leaves_no_temp() {
        let root = scratch("rename-over");
        let exe = root.join("wftui");
        std::fs::write(&exe, b"old").unwrap();
        let staged = root.join("staged");
        std::fs::write(&staged, b"new").unwrap();
        install_over(&staged, &exe, Install::RenameOver).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!root.join("wftui.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
        assert!(install_over(&root.join("absent"), &exe, Install::RenameOver).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new", "a failed install leaves the binary alone");
        assert!(!root.join("wftui.new").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_over_rename_aside_swaps_and_installs_fresh_when_nothing_is_there() {
        let root = scratch("rename-aside");
        let exe = root.join("wftui.exe");
        std::fs::write(&exe, b"old").unwrap();
        let staged = root.join("staged");
        std::fs::write(&staged, b"new").unwrap();
        install_over(&staged, &exe, Install::RenameAside).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!root.join("wftui.exe.new").exists());
        // `.old` is removed when nothing holds it; on this platform nothing does.
        assert!(!root.join("wftui.exe.old").exists());
        let fresh = root.join("sub").join("wftui.exe");
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        install_over(&staged, &fresh, Install::RenameAside).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn cfg_at(root: &Path, exe: &Path, feed: &str) -> UpdateConfig {
        UpdateConfig::for_test(root.to_path_buf(), feed.into(), exe.to_path_buf(), "0.0.1")
    }

    #[test]
    fn apply_pending_update_reports_applied_only_after_a_completed_install() {
        let root = scratch("apply");
        let exe = root.join("bin").join(host().binary_name());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, synthetic_binary(host(), 0)).unwrap();
        let cfg = cfg_at(&root.join("update"), &exe, "http://127.0.0.1:1/");
        assert_eq!(apply_pending_update(&cfg), ApplyOutcome::NothingPending, "no update dir yet");

        let body = synthetic_binary(host(), 7);
        let older = synthetic_binary(host(), 8);
        stage_pending_in(&cfg.root, "0.0.2", host(), &sha256_hex(&older), &older).unwrap();
        let newest = stage_pending_in(&cfg.root, "0.0.3", host(), &sha256_hex(&body), &body).unwrap();
        assert_eq!(apply_pending_update(&cfg), ApplyOutcome::Applied("0.0.3".into()));
        assert_eq!(std::fs::read(&exe).unwrap(), body, "the newest generation won");
        assert!(!newest.dir.exists(), "the applied generation is consumed");

        // The next start runs as 0.0.3: the 0.0.2 generation is a downgrade now.
        let cfg3 = UpdateConfig { current_version: "0.0.3", ..cfg.clone() };
        assert_eq!(apply_pending_update(&cfg3), ApplyOutcome::NothingPending);
        assert!(load_pending(&cfg.root, host(), "0.0.1").unwrap().is_empty(), "pruned");

        // Held by a sibling: never applied, never blocks, and never claims "applied".
        let again = synthetic_binary(host(), 9);
        stage_pending_in(&cfg.root, "0.0.4", host(), &sha256_hex(&again), &again).unwrap();
        let held = UpdateLock::acquire(&cfg.root).unwrap();
        assert!(matches!(apply_pending_update(&cfg), ApplyOutcome::Incomplete(_)));
        drop(held);
        assert_eq!(apply_pending_update(&cfg), ApplyOutcome::Applied("0.0.4".into()));

        let disabled = UpdateConfig { disabled: true, ..cfg.clone() };
        assert_eq!(apply_pending_update(&disabled), ApplyOutcome::NothingPending);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_removes_a_stale_exe_old_when_nothing_is_pending() {
        let root = scratch("old");
        let exe = root.join("wftui.exe");
        std::fs::write(&exe, b"cur").unwrap();
        let old = root.join("wftui.exe.old");
        std::fs::write(&old, b"prev").unwrap();
        let cfg = cfg_at(&root.join("update"), &exe, "http://127.0.0.1:1/");
        std::fs::create_dir_all(&cfg.root).unwrap();
        assert_eq!(apply_pending_update(&cfg), ApplyOutcome::NothingPending);
        assert!(!old.exists());
        assert_eq!(std::fs::read(&exe).unwrap(), b"cur");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_refuses_a_cargo_target_binary_and_discards_the_generation() {
        let root = scratch("refuse");
        let checkout = root.join("checkout");
        let exe = checkout.join("target").join("release").join(host().binary_name());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(&exe, b"dev").unwrap();
        let cfg = cfg_at(&root.join("update"), &exe, "http://127.0.0.1:1/");
        let body = synthetic_binary(host(), 1);
        let p = stage_pending_in(&cfg.root, "0.0.2", host(), &sha256_hex(&body), &body).unwrap();
        assert_eq!(
            apply_pending_update(&cfg),
            ApplyOutcome::Refused { version: "0.0.2".into(), why: Refusal::CargoTarget }
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"dev");
        assert!(!p.dir.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manual_install_command_is_the_install_then_rename_swap() {
        let cmd = manual_install_command(Path::new("/cfg/update/pending-1/wftui"), Path::new("/usr/local/bin/wftui"));
        if cfg!(windows) {
            assert!(cmd.contains("move /y") && cmd.contains("copy /y"));
        } else {
            assert_eq!(
                cmd,
                "sudo install -m755 '/cfg/update/pending-1/wftui' '/usr/local/bin/wftui.new' && sudo mv -f '/usr/local/bin/wftui.new' '/usr/local/bin/wftui'"
            );
        }
    }

    #[test]
    fn last_check_throttle_blocks_auto_checks_but_not_forced_ones() {
        let root = scratch("throttle");
        assert!(since_last_check(&root).is_none());
        note_check(&root);
        let age = since_last_check(&root).unwrap();
        assert!(age < MIN_CHECK_INTERVAL);
        let exe = root.join("wftui");
        std::fs::write(&exe, b"x").unwrap();
        let cfg = cfg_at(&root, &exe, "http://127.0.0.1:1/latest");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert_eq!(rt.block_on(check_and_stage(&cfg, false)).unwrap(), Outcome::Throttled);
        // Forced: the throttle is skipped, so the (unreachable) feed is actually tried.
        assert!(rt.block_on(check_and_stage(&cfg, true)).is_err());
        let disabled = UpdateConfig { disabled: true, ..cfg.clone() };
        assert!(matches!(rt.block_on(check_and_stage(&disabled, true)).unwrap(), Outcome::Skipped(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------ wiremock

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    struct Feed {
        server: MockServer,
        root: PathBuf,
        exe: PathBuf,
        body: Vec<u8>,
    }

    /// A mock release at `tag` whose binary is `body`, with `sums` as the
    /// checksum file's text (so a test can publish a wrong one).
    async fn feed(tag: &str, body: Vec<u8>, sums: Option<String>, tweak: impl FnOnce(&mut serde_json::Value)) -> Feed {
        let server = MockServer::start().await;
        let root = scratch("feed");
        let exe = root.join("bin").join(host().binary_name());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, synthetic_binary(host(), 0)).unwrap();
        let name = host().asset_name(version_of_tag(tag));
        let sums = sums.unwrap_or_else(|| format!("{}  {name}\n", sha256_hex(&body)));
        let mut release: serde_json::Value =
            serde_json::from_str(include_str!("testdata/github_release_latest.json")).unwrap();
        release["tag_name"] = tag.into();
        release["html_url"] = format!("{}/releases/tag/{tag}", server.uri()).into();
        release["assets"] = serde_json::json!([
            {"name": name, "browser_download_url": format!("{}/dl/{name}", server.uri()), "size": body.len()},
            {"name": SUMS_ASSET, "browser_download_url": format!("{}/dl/{SUMS_ASSET}", server.uri()), "size": sums.len()},
        ]);
        tweak(&mut release);
        Mock::given(method("GET"))
            .and(path("/latest"))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/dl/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/dl/{SUMS_ASSET}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(&server)
            .await;
        Feed { server, root, exe, body }
    }

    impl Feed {
        fn cfg(&self) -> UpdateConfig {
            cfg_at(&self.root.join("update"), &self.exe, &format!("{}/latest", self.server.uri()))
        }
        async fn requests(&self) -> Vec<Request> {
            self.server.received_requests().await.unwrap_or_default()
        }
    }

    #[tokio::test]
    async fn check_and_stage_downloads_verifies_and_stages() {
        let f = feed("v0.0.2", synthetic_binary(host(), 5), None, |_| {}).await;
        let cfg = f.cfg();
        let out = check_and_stage(&cfg, false).await.unwrap();
        let Outcome::Staged { version, path } = out else {
            panic!("{out:?}");
        };
        assert_eq!(version, "0.0.2");
        assert_eq!(std::fs::read(&path).unwrap(), f.body);
        assert!(path.starts_with(&cfg.root));
        assert_eq!(std::fs::read(&f.exe).unwrap(), synthetic_binary(host(), 0), "nothing is swapped until the next start");
        assert!(cfg.root.join(LAST_CHECK_NAME).exists());

        // A second check reuses the staged generation without another download.
        let before = f.requests().await.len();
        assert!(matches!(check_and_stage(&cfg, true).await.unwrap(), Outcome::Staged { .. }));
        assert_eq!(f.requests().await.len(), before + 1, "only the feed was asked again");

        // And the next start applies it.
        assert_eq!(apply_pending_update(&cfg), ApplyOutcome::Applied("0.0.2".into()));
        assert_eq!(std::fs::read(&f.exe).unwrap(), f.body);
        let _ = std::fs::remove_dir_all(&f.root);
    }

    /// The feed is not our origin: no bearer, ever — and the house UA, which
    /// GitHub accepts and which hard rule 6 says never changes.
    #[tokio::test]
    async fn check_never_sends_authorization_and_uses_the_house_ua() {
        let f = feed("v0.0.2", synthetic_binary(host(), 5), None, |_| {}).await;
        check_and_stage(&f.cfg(), true).await.unwrap();
        let reqs = f.requests().await;
        assert_eq!(reqs.len(), 3, "feed, sums, binary");
        for r in &reqs {
            assert!(r.headers.get("authorization").is_none(), "{} carried a token", r.url);
            let ua = r.headers.get("user-agent").expect("ua").to_str().unwrap().to_string();
            assert!(ua.starts_with("wftui/"), "{ua}");
        }
        let _ = std::fs::remove_dir_all(&f.root);
    }

    #[tokio::test]
    async fn check_rejects_sha_mismatch_and_leaves_no_generation() {
        let body = synthetic_binary(host(), 5);
        let bad = format!("{}  {}\n", "0".repeat(64), host().asset_name("0.0.2"));
        let f = feed("v0.0.2", body, Some(bad), |_| {}).await;
        let cfg = f.cfg();
        assert!(matches!(check_and_stage(&cfg, true).await, Err(Error::FetchRejected(_))));
        assert!(load_pending(&cfg.root, host(), "0.0.1").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&f.root);
    }

    #[tokio::test]
    async fn check_treats_403_and_429_as_quiet_rate_limits() {
        for status in [403u16, 429] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/latest"))
                .respond_with(ResponseTemplate::new(status).insert_header("retry-after", "120"))
                .mount(&server)
                .await;
            let root = scratch("ratelimit");
            let exe = root.join("wftui");
            std::fs::write(&exe, b"x").unwrap();
            let cfg = cfg_at(&root.join("update"), &exe, &format!("{}/latest", server.uri()));
            match check_and_stage(&cfg, true).await {
                Err(Error::RateLimited { retry_after }) => assert_eq!(retry_after, Some(Duration::from_secs(120))),
                other => panic!("{status}: {other:?}"),
            }
            assert!(cfg.root.join(LAST_CHECK_NAME).exists(), "a refused request still spent quota");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[tokio::test]
    async fn check_refuses_prerelease_draft_and_older_tags() {
        let body = synthetic_binary(host(), 5);
        let f = feed("v0.0.2", body.clone(), None, |r| r["prerelease"] = true.into()).await;
        assert!(matches!(check_and_stage(&f.cfg(), true).await.unwrap(), Outcome::UpToDate { .. }));
        let f = feed("v0.0.2", body.clone(), None, |r| r["draft"] = true.into()).await;
        assert!(matches!(check_and_stage(&f.cfg(), true).await.unwrap(), Outcome::UpToDate { .. }));
        let f = feed("v0.0.1", body.clone(), None, |_| {}).await;
        assert_eq!(check_and_stage(&f.cfg(), true).await.unwrap(), Outcome::UpToDate { latest: "0.0.1".into() });
        let f = feed("v0.0.0", body, None, |_| {}).await;
        assert_eq!(check_and_stage(&f.cfg(), true).await.unwrap(), Outcome::UpToDate { latest: "0.0.0".into() });
        assert_eq!(f.requests().await.len(), 1, "an older release downloads nothing");
        let _ = std::fs::remove_dir_all(&f.root);
    }

    #[tokio::test]
    async fn check_caps_the_download_size() {
        let body = synthetic_binary(host(), 5);
        let f = feed("v0.0.2", body, None, |r| {
            let url = r["assets"][0]["browser_download_url"].as_str().unwrap();
            let (prefix, _name) = url.rsplit_once('/').unwrap();
            r["assets"][0]["browser_download_url"] = format!("{prefix}/huge").into();
        })
        .await;
        Mock::given(method("GET"))
            .and(path("/dl/huge"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; MAX_BINARY_BYTES + 1]))
            .mount(&f.server)
            .await;
        assert!(matches!(check_and_stage(&f.cfg(), true).await, Err(Error::FetchRejected(_))));
        let _ = std::fs::remove_dir_all(&f.root);
    }

    #[tokio::test]
    async fn check_reports_manual_install_when_the_binary_dir_is_read_only() {
        if cfg!(windows) {
            return; // no mode bits to take away
        }
        let f = feed("v0.0.2", synthetic_binary(host(), 5), None, |_| {}).await;
        let cfg = f.cfg();
        let dir = f.exe.parent().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        if install_dir_writable(&f.exe) {
            // root ignores mode bits; there is no read-only dir to test with.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            return;
        }
        let out = check_and_stage(&cfg, true).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let Outcome::ManualInstall { version, staged, command } = out else {
            panic!("{out:?}");
        };
        assert_eq!(version, "0.0.2");
        assert!(staged.exists());
        assert!(command.contains(&f.exe.display().to_string()));
        let _ = std::fs::remove_dir_all(&f.root);
    }
}
