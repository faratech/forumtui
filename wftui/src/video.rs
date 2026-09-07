//! Playing a post's video in the terminal (#710).
//!
//! The client does not decode anything: it hands the URL to `mpv`, which
//! already knows how to resolve a YouTube page through `yt-dlp` and how to
//! paint frames with kitty's graphics protocol, sixel, or true-colour text
//! blocks. What this module owns is the argument list and the reasons a
//! playback cannot happen — both testable without a terminal, unlike the
//! playback itself.
//!
//! The suspend/restore dance around it lives in `app::run`, because that is
//! what owns the terminal and the reader thread.

use crate::images::Tier as GraphicsTier;
use crate::theme::Tier as ColorTier;

/// Is `name` on PATH?
fn have(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// True when sound can be played alongside the picture (#711).
///
/// ffmpeg pipes the video here and cannot also play audio, so sound is a
/// second process. Its absence is not a reason to refuse a video.
pub fn audio_available() -> bool {
    have("ffplay")
}

/// Why a video cannot play, in words a reader can act on — or `None`.
pub fn unavailable_reason(colors: ColorTier, graphics: GraphicsTier) -> Option<String> {
    if !have("ffmpeg") {
        return Some(
            "ffmpeg is not installed — `sudo apt install ffmpeg` (yt-dlp is already here)."
                .to_string(),
        );
    }
    if matches!(colors, ColorTier::Mono) {
        return Some(
            "This terminal reports no colour, so there is nothing to draw video with — press o to watch it in a browser."
                .to_string(),
        );
    }
    if !graphics.inline() {
        return Some(
            "This terminal has no inline graphics — press o to watch it in a browser."
                .to_string(),
        );
    }
    None
}



// ================= in-app playback (#711) =================

/// A video playing *inside* the client.
///
/// Handing the terminal to `mpv` (#710) worked but was not the app: the
/// screen went away and came back. This keeps the frame in a pane, which
/// means the client owns three things it did not before — decoding, pacing,
/// and audio — and each is delegated to the tool that already does it well:
///
/// * **Decode** is `ffmpeg`, reading the stream `yt-dlp` resolved and writing
///   raw RGB to a pipe. Measured on this box: 5 s of 360p at 12 fps decodes
///   in 0.15 s, about 32x realtime, so decoding is never the constraint.
/// * **Pacing** is this module's thread, presenting frames against a wall
///   clock rather than as fast as the pipe delivers them. Only the newest
///   frame is kept; a slow terminal drops frames instead of falling behind.
/// * **Audio** is a second process (`ffplay -nodisp`), because ffmpeg cannot
///   both pipe video here and play sound. Both children start together and
///   the video is clocked to real time, which keeps them close; pausing
///   stops both with SIGSTOP so they cannot drift apart while paused.
pub struct Playback {
    children: Children,
    /// The newest decoded frame, and how many have been shown. The reader
    /// thread writes; the renderer reads.
    latest: std::sync::Arc<std::sync::Mutex<Option<Frame>>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    paused: bool,
}

/// One decoded frame, in the size it was decoded at.
pub struct Frame {
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub index: u64,
}

/// Frames a second. 12 is a deliberate floor, not a limit of the machine:
/// it is enough for talking heads and screen recordings — which is what a
/// Windows support forum links — and it keeps a slow SSH link viable.
pub const FPS: u32 = 12;
/// Decode size. Fixed on BOTH axes on purpose: raw video is a byte stream
/// with no frame boundaries, so the reader can only split it if it knows
/// exactly how big a frame is. `scale=…:force_original_aspect_ratio=decrease`
/// plus `pad` keeps the aspect and letterboxes into this box, which is what
/// a video player does anyway — and it means any source, 16:9 or 4:3 or
/// vertical phone video, produces frames of one known length.
pub const DECODE_WIDTH: u32 = 640;
pub const DECODE_HEIGHT: u32 = 360;

impl Playback {
    /// Begin playing `url`. Returns at once: resolving a YouTube page takes
    /// a second or two of network, and blocking the UI thread on it is
    /// exactly the un-fluid thing this design exists to avoid. The pane says
    /// "buffering…" until the first frame lands.
    pub fn start(url: &str, want_audio: bool) -> Result<Playback, String> {
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let children: Children = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let (slot, finished, err, kids) =
            (latest.clone(), done.clone(), error.clone(), children.clone());
        let url = url.to_string();
        std::thread::Builder::new()
            .name("wftui-video".into())
            .spawn(move || {
                if let Err(e) = run_playback(&url, want_audio, &slot, &kids)
                    && let Ok(mut guard) = err.lock()
                {
                    *guard = Some(e);
                }
                finished.store(true, std::sync::atomic::Ordering::Relaxed);
            })
            .map_err(|e| format!("video thread: {e}"))?;

        Ok(Playback {
            children,
            latest,
            done,
            error,
            paused: false,
        })
    }

    /// The newest frame, if one has arrived since the last look.
    pub fn take_frame(&mut self) -> Option<Frame> {
        self.latest.lock().ok()?.take()
    }

    pub fn finished(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Why playback stopped, when it stopped for a reason worth showing.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok()?.clone()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Pause or resume the picture and the sound together.
    ///
    /// SIGSTOP rather than any protocol of our own: it stops decode and
    /// sound at the same instant and they cannot drift apart while stopped,
    /// which no amount of buffering on our side would guarantee.
    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        #[cfg(unix)]
        {
            let sig = if self.paused { libc::SIGSTOP } else { libc::SIGCONT };
            if let Ok(kids) = self.children.lock() {
                for child in kids.iter() {
                    // SAFETY: a pid this process owns; a failure means the
                    // child has already exited, which is nothing to act on.
                    unsafe { libc::kill(child.id() as libc::pid_t, sig) };
                }
            }
        }
    }

    /// Stop everything. Called on close and from `Drop`, so a pane that goes
    /// away never leaves ffmpeg decoding into a pipe nobody reads.
    pub fn stop(&mut self) {
        // Resume first: a stopped child cannot act on being killed.
        if self.paused {
            self.toggle_pause();
        }
        if let Ok(mut kids) = self.children.lock() {
            for child in kids.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            kids.clear();
        }
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The children a playback owns, shared so the UI thread can pause and kill
/// them while the decode thread is inside a blocking read.
type Children = std::sync::Arc<std::sync::Mutex<Vec<std::process::Child>>>;

/// Resolve, spawn and decode. Runs on the playback thread.
fn run_playback(
    url: &str,
    want_audio: bool,
    slot: &std::sync::Arc<std::sync::Mutex<Option<Frame>>>,
    children: &Children,
) -> Result<(), String> {
    use std::process::{Command, Stdio};

    // A forum link is a *page*; ffmpeg needs the media behind it. mpv hid
    // this by calling yt-dlp itself — playing in-app means doing it here.
    let (media, audio_media) = resolve_streams(url)?;
    let (width, height) = (DECODE_WIDTH, DECODE_HEIGHT);
    let filter = format!(
        "fps={FPS},scale={width}:{height}:force_original_aspect_ratio=decrease,\
         pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
    );
    // A generated source (`testsrc=…`) is lavfi, not a file — tests only.
    let lavfi: &[&str] = if media.contains('=') && !media.contains("://") {
        &["-f", "lavfi"]
    } else {
        &[]
    };

    let mut video = Command::new("ffmpeg")
        .args(lavfi)
        .args([
            "-loglevel", "error",
            "-i", &media,
            "-vf", &filter,
            "-f", "rawvideo",
            "-pix_fmt", "rgb24",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let stdout = video.stdout.take().ok_or("ffmpeg produced no output")?;

    // Sound is a second process because ffmpeg cannot both pipe frames here
    // and play audio. Best-effort: a server over SSH has no sound device,
    // and a silent video is still the thing the reader asked to see.
    let audio = want_audio
        .then(|| {
            Command::new("ffplay")
                // Its own stream when the source is split, the same one when
                // it is muxed.
                .args([
                    "-nodisp",
                    "-autoexit",
                    "-loglevel",
                    "quiet",
                    "-i",
                    audio_media.as_deref().unwrap_or(&media),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        })
        .flatten();

    if let Ok(mut kids) = children.lock() {
        kids.push(video);
        if let Some(a) = audio {
            kids.push(a);
        }
    }

    decode_loop(stdout, width, height, slot);
    Ok(())
}

/// The media behind a page, via `yt-dlp`: `(video, audio)`.
///
/// Two URLs, not one. YouTube has largely stopped offering *progressive*
/// streams — video and audio muxed into a single file — so asking for one
/// fails outright on most videos ("Requested format is not available"),
/// which is what the first draft of this did. The selector therefore asks
/// for the best video plus the best audio, capped at 480p because the frames
/// are scaled to a terminal pane anyway, and falls back to whatever single
/// stream exists for the sites that still have one.
///
/// A URL that is already media (or a test source) passes through untouched.
fn resolve_streams(url: &str) -> Result<(String, Option<String>), String> {
    if common::bbcode::video_site(url).is_none() {
        return Ok((url.to_string(), None));
    }
    if !have("yt-dlp") {
        return Err("yt-dlp is not installed — needed to resolve video links.".into());
    }
    let out = std::process::Command::new("yt-dlp")
        .args([
            "--no-warnings",
            "--quiet",
            "-f",
            "bv*[height<=480]+ba/b[height<=480]/bv*+ba/b",
            "-g",
            url,
        ])
        .output()
        .map_err(|e| format!("yt-dlp: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut urls = stdout.lines().filter(|l| l.starts_with("http"));
    let Some(video) = urls.next().map(str::to_string) else {
        let err = String::from_utf8_lossy(&out.stderr);
        let reason = err.lines().last().unwrap_or("no stream found").trim();
        return Err(format!("could not resolve the video ({reason})"));
    };
    // A second line is the audio-only stream; one line means it was already
    // muxed and the same URL carries both.
    Ok((video, urls.next().map(str::to_string)))
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read fixed-size frames off ffmpeg's pipe and present them on a wall
/// clock, keeping only the newest.
///
/// Pacing here rather than in the UI is what makes playback smooth: ffmpeg
/// delivers far faster than realtime, so without a clock the whole video
/// would flash past in a second.
fn decode_loop(
    mut stdout: std::process::ChildStdout,
    width: u32,
    height: u32,
    slot: &std::sync::Arc<std::sync::Mutex<Option<Frame>>>,
) {
    use std::io::Read;
    // Exactly one frame, because the filter letterboxes every source into
    // this box. Guessing the height from the aspect (which an earlier draft
    // did) tears the stream the moment a video is not 16:9.
    let mut buf = vec![0u8; (width * height * 3) as usize];
    let start = std::time::Instant::now();
    let mut index: u64 = 0;

    // A short read is the end of the stream (or a killed child), which is
    // how playback ends normally.
    while stdout.read_exact(&mut buf).is_ok() {
        // Present this frame when its time comes. Late frames are shown at
        // once rather than skipped: dropping is the renderer's business, and
        // it already only ever paints the newest.
        let due = std::time::Duration::from_micros(index * 1_000_000 / FPS as u64);
        let elapsed = start.elapsed();
        if due > elapsed {
            std::thread::sleep(due - elapsed);
        }
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(Frame {
                rgb: buf.clone(),
                width,
                height,
                index,
            });
        } else {
            break;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #711: the refusals name the thing to do about them, and only refuse
    /// for reasons that are actually true here.
    #[test]
    fn refusals_say_what_to_do() {
        // A terminal that can paint and has colour is not refused (ffmpeg is
        // installed on this machine; if it were not, that message wins,
        // which is also right).
        let ok = unavailable_reason(ColorTier::TrueColor, GraphicsTier::Kitty);
        if have("ffmpeg") {
            assert!(ok.is_none(), "a capable terminal must not be refused: {ok:?}");

            // Mono means the reader asked for no colour: there is nothing
            // honest to paint with, and the message points at the browser.
            let mono = unavailable_reason(ColorTier::Mono, GraphicsTier::Kitty);
            assert!(mono.is_some_and(|m| m.contains("browser")));

            // The text tier can draw no picture at all.
            let text = unavailable_reason(ColorTier::TrueColor, GraphicsTier::Text);
            assert!(text.is_some_and(|m| m.contains("browser")));
        } else {
            assert!(ok.is_some_and(|m| m.contains("ffmpeg")));
        }
    }

    /// #711: the decoder really produces frames, paced to a wall clock.
    ///
    /// Against a locally generated stream rather than the network, so it
    /// tests this module and not YouTube's availability: `ffmpeg`'s own test
    /// pattern, decoded through exactly the path a video takes.
    #[test]
    fn playback_delivers_paced_frames_of_a_known_size() {
        if !have("ffmpeg") {
            return; // nothing to test against on this machine
        }
        // A 2-second synthetic source, deliberately NOT 16:9 — an earlier
        // draft guessed the frame height from the aspect and tore on
        // anything else.
        let mut p = Playback::start("testsrc=size=400x400:rate=30:duration=2", false)
            .expect("start");

        let want = (DECODE_WIDTH * DECODE_HEIGHT * 3) as usize;
        let mut frames = 0u32;
        let mut first_at = None;
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(4) {
            if let Some(f) = p.take_frame() {
                assert_eq!(f.rgb.len(), want, "a frame is exactly one frame");
                assert_eq!((f.width, f.height), (DECODE_WIDTH, DECODE_HEIGHT));
                frames += 1;
                first_at.get_or_insert(start.elapsed());
            }
            if p.finished() && frames > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert!(frames >= 5, "decoding produced {frames} frames");
        // Paced, not dumped: 2 seconds of video cannot all arrive at once,
        // even though ffmpeg decodes it ~32x faster than realtime.
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(700),
            "frames arrived far too fast to be paced"
        );
        p.stop();
        assert!(p.finished());
    }

    /// Pausing stops the decode, and resuming starts it again — the frames
    /// prove it, not the flag.
    #[test]
    fn pausing_stops_the_frames() {
        if !have("ffmpeg") {
            return;
        }
        let mut p = Playback::start("testsrc=size=320x240:rate=30:duration=10", false)
            .expect("start");
        // Let it get going.
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(800) {
            let _ = p.take_frame();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        p.toggle_pause();
        assert!(p.paused());
        // Drain whatever was already in flight, then nothing new may appear.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = p.take_frame();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            p.take_frame().is_none(),
            "a paused video must not keep producing frames"
        );
        p.toggle_pause();
        assert!(!p.paused());
        p.stop();
    }

    /// #711 end to end, against the real thing: a YouTube *page* URL —
    /// which is what a post carries and what ffmpeg cannot open — resolved,
    /// decoded, and delivered as frames.
    ///
    /// Ignored by default because it needs the network and YouTube's
    /// cooperation; run it deliberately with
    /// `cargo test -- --ignored real_youtube`.
    #[test]
    #[ignore = "needs network and yt-dlp against YouTube"]
    fn real_youtube_page_resolves_and_decodes() {
        let page = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        assert!(
            common::bbcode::video_site(page).is_some(),
            "the client must recognise a page URL as a video"
        );
        let (media, audio) = resolve_streams(page).expect("resolve");
        assert!(media.starts_with("http") && media != page, "resolved: {media:.60}");
        // YouTube serves video and audio separately now; the resolver has to
        // come back with both or the sound is silently lost.
        assert!(audio.is_some(), "expected a separate audio stream");

        let mut p = Playback::start(page, false).expect("start");
        let start = std::time::Instant::now();
        let mut frames = 0;
        while start.elapsed() < std::time::Duration::from_secs(30) && frames < 3 {
            if p.take_frame().is_some() {
                frames += 1;
            }
            if let Some(e) = p.error() {
                panic!("playback failed: {e}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        p.stop();
        assert!(frames >= 3, "decoded {frames} frames from YouTube");
    }

    /// Audio is optional and its absence never blocks a video.
    #[test]
    fn audio_is_optional() {
        // Whatever this machine has, the answer is a bool and asking is
        // free — the point is that `unavailable_reason` never consults it.
        let _ = audio_available();
        if have("ffmpeg") {
            assert!(unavailable_reason(ColorTier::TrueColor, GraphicsTier::Sixel).is_none());
        }
    }
}
