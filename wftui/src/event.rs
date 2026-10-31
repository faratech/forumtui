//! Terminal input: a dedicated blocking reader thread feeding the UI loop.
//!
//! A thread doing blocking `event::read()` is the reliable pattern — the UI
//! loop's poll-with-timeout raced and dropped events under load.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use ratatui::crossterm::event::{
    self, Event as TEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
};

#[derive(Debug, Clone)]
pub enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize,
    /// The reader thread's `event::read()` failed — the tty went away or
    /// errored. The loop respawns the reader a bounded number of times and
    /// exits cleanly past that, instead of ticking forever with no way to
    /// type (#653).
    ReaderDied,
}

/// How many times a dead reader may be replaced before the client concludes
/// its input is gone for good and quits through the normal (terminal-
/// restoring) teardown.
pub const MAX_READER_RESPAWNS: u32 = 3;

/// The input-death policy (#653): `deaths` is how many readers have died so
/// far. True = spawn another reader; false = exit cleanly.
pub fn reader_should_restart(deaths: u32) -> bool {
    deaths <= MAX_READER_RESPAWNS
}

/// Spawn the reader thread; returns the receiving side for the UI loop.
pub fn spawn_reader() -> Receiver<Input> {
    spawn_reader_with_prefix(Vec::new())
}

pub fn spawn_reader_with_prefix(prefix: Vec<u8>) -> Receiver<Input> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("wftui-reader".into())
        .spawn(move || {
        #[cfg(all(feature = "images", unix))]
        for event in startup_events(prefix) {
            if tx.send(event).is_err() { return; }
        }
        #[cfg(not(all(feature = "images", unix)))]
        let _ = prefix;
        loop {
            match event::read() {
                Ok(TEvent::Key(k)) => {
                    if !should_process_key(&k) {
                        continue;
                    }
                    if tx.send(Input::Key(k)).is_err() {
                        break;
                    }
                }
                Ok(TEvent::Mouse(m)) => {
                    if tx.send(Input::Mouse(m)).is_err() {
                        break;
                    }
                }
                Ok(TEvent::Paste(p)) => {
                    if tx.send(Input::Paste(p)).is_err() {
                        break;
                    }
                }
                Ok(TEvent::Resize(..)) => {
                    if tx.send(Input::Resize).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    // Tell the UI loop the reader is gone — it respawns or
                    // quits; a silent break used to disable all input
                    // forever while the frame kept redrawing (#653).
                    let _ = tx.send(Input::ReaderDied);
                    break;
                }
            }
        }
    })
    .expect("spawn reader");
    rx
}

/// How long the sole reader may wait for the rest of a sequence the probe
/// deadline cut (#137). A terminal finishing its own escape sequence sends
/// the remaining bytes within milliseconds; past this the tail is decoded as
/// it stands, and the next byte — a real keypress — is left for
/// `event::read` to deliver as itself.
#[cfg(all(feature = "images", unix))]
const COMPLETION_WINDOW: Duration = Duration::from_millis(250);

/// Replay user input consumed before the normal terminal reader existed.
/// This runs on that sole reader, after the application entered raw mode.
#[cfg(all(feature = "images", unix))]
fn startup_events(bytes: Vec<u8>) -> Vec<Input> {
    let deadline = std::time::Instant::now() + COMPLETION_WINDOW;
    complete_and_decode(bytes, || read_byte_before(libc::STDIN_FILENO, deadline))
}

/// Complete a UTF-8 character, escape sequence or bracketed paste split by
/// the probe deadline (#41), taking bytes from `next_byte` until it has no
/// more to give, then decode. `next_byte` is bounded by a deadline (#137):
/// the old blocking `read_exact` waited forever on a peer that never sent
/// the rest, with every key dead until some unrelated byte arrived — and
/// that byte was then eaten into the old sequence.
#[cfg(all(feature = "images", unix))]
fn complete_and_decode(mut bytes: Vec<u8>, mut next_byte: impl FnMut() -> Option<u8>) -> Vec<Input> {
    while let Err(error) = std::str::from_utf8(&bytes) {
        if error.error_len().is_some() { break; }
        let Some(byte) = next_byte() else { break };
        bytes.push(byte);
    }
    // A CSI truncated before its final byte otherwise decoded as Esc plus
    // the remaining bytes as literal character keys — junk keystrokes
    // injected into whatever screen came up first. Bounded in bytes too.
    let mut completed = 0usize;
    while completed < 64 * 1024 && split_sequence_tail(&bytes) {
        let Some(byte) = next_byte() else { break };
        bytes.push(byte);
        completed += 1;
    }
    decode_startup_events(&bytes)
}

/// One byte from `fd` if it arrives before `deadline`. `poll` first, then a
/// one-byte `read(2)` — never `std::io::stdin()`, whose internal buffer
/// would keep bytes past the sequence (the user's next keys) where the
/// terminal reader can never see them.
#[cfg(all(feature = "images", unix))]
fn read_byte_before(fd: std::os::fd::RawFd, deadline: std::time::Instant) -> Option<u8> {
    loop {
        let left = deadline.checked_duration_since(std::time::Instant::now())?;
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one initialized entry.
        let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis().clamp(1, i32::MAX as u128) as i32) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if ready == 0 || poll.revents & libc::POLLIN == 0 {
            return None;
        }
        let mut byte = 0u8;
        // SAFETY: one-byte writable buffer; the descriptor reported readable.
        let read = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        match read {
            1 => return Some(byte),
            0 => return None,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => continue,
            _ => return None,
        }
    }
}

/// True while `bytes` ends inside an escape sequence the decoder would
/// otherwise mangle: an unterminated CSI/SS3/OSC tail, or a bracketed paste
/// whose end marker has not arrived yet. A lone trailing ESC is not one —
/// that is a real Esc press, which the decoder handles.
#[cfg(all(feature = "images", unix))]
fn split_sequence_tail(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    if let Some(start) = text.find("\x1b[200~")
        && !text[start..].contains("\x1b[201~")
    {
        return true;
    }
    let Some(pos) = text.rfind('\x1b') else { return false };
    let tail = &text[pos..];
    if !tail.starts_with("\x1b[")
        && !tail.starts_with("\x1bO")
        && !tail.starts_with("\x1b]")
        && !tail.starts_with("\x1bP")
    {
        return false;
    }
    crate::images::unix::sequence_len(tail.as_bytes()).is_none()
}

#[cfg(all(feature = "images", unix))]
fn decode_startup_events(bytes: &[u8]) -> Vec<Input> {
    let text = String::from_utf8_lossy(bytes);
    let mut remaining = text.as_ref();
    let mut events = Vec::new();
    while !remaining.is_empty() {
        if let Some(paste) = remaining.strip_prefix("\x1b[200~") {
            // A paste whose end marker never came is still a paste: decoded
            // as keystrokes, its text would drive the first screen (#138).
            let end = paste.find("\x1b[201~");
            events.push(Input::Paste(paste[..end.unwrap_or(paste.len())].to_owned()));
            remaining = end.map_or("", |end| &paste[end + 6..]);
            continue;
        }
        let mut modifiers = KeyModifiers::NONE;
        if remaining.starts_with('\x1b') && remaining.len() > 1 {
            // A complete OSC, DCS or APC string is a terminal reply the probe
            // did not ask for, never a key: drop it whole (#138) instead of
            // reading it as Alt+`]` and a run of literal keys.
            if (remaining.starts_with("\x1b]") || remaining.starts_with("\x1bP") || remaining.starts_with("\x1b_"))
                && let Some(len) = crate::images::unix::sequence_len(remaining.as_bytes())
                && len > 2
            {
                remaining = &remaining[len..];
                continue;
            }
            if let Some(len) = crate::images::unix::sequence_len(remaining.as_bytes())
                && (remaining.starts_with("\x1b[") || remaining.starts_with("\x1bO"))
            {
                let sequence = &remaining[2..len];
                let code = match sequence.as_bytes().last() {
                    Some(b'A') => Some(KeyCode::Up), Some(b'B') => Some(KeyCode::Down),
                    Some(b'C') => Some(KeyCode::Right), Some(b'D') => Some(KeyCode::Left),
                    Some(b'H') => Some(KeyCode::Home), Some(b'F') => Some(KeyCode::End),
                    Some(b'P') => Some(KeyCode::F(1)), Some(b'Q') => Some(KeyCode::F(2)),
                    Some(b'R') => Some(KeyCode::F(3)), Some(b'S') => Some(KeyCode::F(4)),
                    Some(b'Z') => { modifiers |= KeyModifiers::SHIFT; Some(KeyCode::BackTab) },
                    Some(b'~') => match sequence.split([';', '~']).next().unwrap_or("") {
                        "1" | "7" => Some(KeyCode::Home), "2" => Some(KeyCode::Insert),
                        "3" => Some(KeyCode::Delete), "4" | "8" => Some(KeyCode::End),
                        "5" => Some(KeyCode::PageUp), "6" => Some(KeyCode::PageDown),
                        "11" => Some(KeyCode::F(1)), "12" => Some(KeyCode::F(2)),
                        "13" => Some(KeyCode::F(3)), "14" => Some(KeyCode::F(4)),
                        "15" => Some(KeyCode::F(5)), "17" => Some(KeyCode::F(6)),
                        "18" => Some(KeyCode::F(7)), "19" => Some(KeyCode::F(8)),
                        "20" => Some(KeyCode::F(9)), "21" => Some(KeyCode::F(10)),
                        "23" => Some(KeyCode::F(11)), "24" => Some(KeyCode::F(12)), _ => None,
                    },
                    _ => None,
                };
                if let Some(code) = code {
                    if let Some((_, value)) = sequence[..sequence.len()-1].rsplit_once(';')
                        && let Ok(value) = value.parse::<u8>()
                    {
                        let bits = value.saturating_sub(1);
                        if bits & 1 != 0 { modifiers |= KeyModifiers::SHIFT; }
                        if bits & 2 != 0 { modifiers |= KeyModifiers::ALT; }
                        if bits & 4 != 0 { modifiers |= KeyModifiers::CONTROL; }
                    }
                    events.push(Input::Key(KeyEvent::new(code, modifiers)));
                    remaining = &remaining[len..];
                    continue;
                }
                // A complete sequence this table does not name — an SGR
                // mouse report, focus in/out, a CSI-u key, some reply — is
                // dropped whole (#138). Falling through exploded it into Esc,
                // `[` and a run of digits and letters, which could fire quick
                // destinations or screen keys before the user typed a thing.
                remaining = &remaining[len..];
                continue;
            } else if !remaining.starts_with("\x1b[") && !remaining.starts_with("\x1bO") {
                modifiers |= KeyModifiers::ALT;
                remaining = &remaining[1..];
            }
        }
        let ch = remaining.chars().next().expect("nonempty input");
        remaining = &remaining[ch.len_utf8()..];
        let code = match ch {
            '\x1b' => KeyCode::Esc, '\r' | '\n' => KeyCode::Enter,
            '\t' => KeyCode::Tab, '\x7f' | '\x08' => KeyCode::Backspace,
            '\0' => { modifiers |= KeyModifiers::CONTROL; KeyCode::Char(' ') },
            '\x01'..='\x1a' => { modifiers |= KeyModifiers::CONTROL; KeyCode::Char((ch as u8 + b'a' - 1) as char) },
            _ => KeyCode::Char(ch),
        };
        events.push(Input::Key(KeyEvent::new(code, modifiers)));
    }
    events
}

/// Discard key release events emitted by modern terminals with enhanced keyboard support.
pub fn should_process_key(k: &KeyEvent) -> bool {
    k.kind != event::KeyEventKind::Release
}

/// Wait up to `timeout` for the next input from the reader thread.
pub fn next(rx: &Receiver<Input>, timeout: Duration) -> Option<Input> {
    match rx.recv_timeout(timeout) {
        Ok(input) => Some(input),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// True if this key press should terminate the app from anywhere
/// (Ctrl+C). Normal quit is `q` at the root screen.
pub fn is_ctrl_c(k: KeyEvent) -> bool {
    k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    /// #653: a dead reader is replaced a bounded number of times — enough
    /// to survive a transient tty error, not enough to spin forever when
    /// stdin is gone for good.
    #[test]
    fn the_reader_death_policy_restarts_a_bounded_number_of_times() {
        assert!(reader_should_restart(0));
        assert!(reader_should_restart(1));
        assert!(reader_should_restart(MAX_READER_RESPAWNS));
        assert!(
            !reader_should_restart(MAX_READER_RESPAWNS + 1),
            "past the cap the client must quit cleanly, not respawn forever"
        );
    }

    #[test]
    fn filters_key_release_events() {        let press = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        let release = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::empty(),
        };
        let repeat = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::empty(),
        };

        assert!(should_process_key(&press));
        assert!(!should_process_key(&release));
        assert!(should_process_key(&repeat));
    }
    #[cfg(all(feature = "images", unix))]
    #[test]
    fn startup_keys_preserve_unicode_arrows_modifiers_and_paste() {
        let keys = decode_startup_events("jZoë\x1b[A\x1b[1;5D\x1bx\x03\x1b[200~hello\x1b[201~".as_bytes());
        assert!(matches!(keys[3], Input::Key(k) if k.code == KeyCode::Char('ë')));
        assert!(matches!(keys[4], Input::Key(k) if k.code == KeyCode::Up));
        assert!(matches!(keys[5], Input::Key(k) if k.code == KeyCode::Left && k.modifiers == KeyModifiers::CONTROL));
        assert!(matches!(keys[6], Input::Key(k) if k.code == KeyCode::Char('x') && k.modifiers == KeyModifiers::ALT));
        assert!(matches!(keys[7], Input::Key(k) if is_ctrl_c(k)));
        assert!(matches!(&keys[8], Input::Paste(text) if text == "hello"));
    }

    /// #41: the probe's leftover bytes are an arbitrary cut, so a buffer can
    /// end mid-sequence. `startup_events` completes those the way it
    /// completes a split UTF-8 tail; this pins the detector that decides
    /// when more bytes are needed — and that a lone trailing ESC (a real
    /// Esc press) is not one.
    #[cfg(all(feature = "images", unix))]
    #[test]
    fn a_sequence_cut_by_the_probe_deadline_is_recognized_as_incomplete() {
        assert!(split_sequence_tail(b"\x1b[1;5"), "truncated CSI");
        assert!(split_sequence_tail(b"\x1b["), "bare CSI introducer");
        assert!(split_sequence_tail(b"\x1bO"), "bare SS3 introducer");
        assert!(split_sequence_tail(b"\x1b]52;c;abc"), "unterminated OSC");
        assert!(split_sequence_tail(b"hello\x1b[200~pa"), "unterminated paste");
        assert!(!split_sequence_tail(b"\x1b[1;5B"), "complete CSI");
        assert!(!split_sequence_tail(b"\x1b]52;c;abc\x1b\\"), "complete OSC");
        assert!(!split_sequence_tail(b"hi\x1b[200~pa\x1b[201~"), "complete paste");
        assert!(!split_sequence_tail(b"\x1b"), "a lone ESC is an Esc press");
        assert!(!split_sequence_tail(b"\x1bx"), "ALT+x is complete");
        assert!(!split_sequence_tail(b"plain"), "no escape at all");
        // And a completed buffer decodes as the key it names, not junk.
        let keys = decode_startup_events(b"\x1b[1;5B");
        assert!(matches!(keys[0], Input::Key(k) if k.code == KeyCode::Down && k.modifiers == KeyModifiers::CONTROL));
    }

    /// #138: a complete sequence the decoder does not name (SGR mouse,
    /// focus, CSI-u, an unrequested OSC/DCS reply) is dropped whole, never
    /// exploded into Esc plus literal keys; the keys around it survive, and
    /// a paste whose end never came is a paste, not keystrokes.
    #[cfg(all(feature = "images", unix))]
    #[test]
    fn unknown_startup_sequences_are_dropped_whole() {
        let keys = decode_startup_events(b"a\x1b[<0;12;5M\x1b[I\x1b[97;5u\x1b]11;rgb:0/0/0\x1b\\\x1bP1$r0m\x1b\\b\x1b[O\x1b[A");
        let codes: Vec<KeyCode> = keys.iter().map(|k| match k { Input::Key(k) => k.code, other => panic!("{other:?}") }).collect();
        assert_eq!(codes, vec![KeyCode::Char('a'), KeyCode::Char('b'), KeyCode::Up], "{keys:?}");
        let keys = decode_startup_events(b"x\x1b[200~rm -rf ~/j");
        assert!(matches!(&keys[0], Input::Key(k) if k.code == KeyCode::Char('x')));
        assert!(matches!(&keys[1], Input::Paste(text) if text == "rm -rf ~/j"), "{keys:?}");
        assert_eq!(keys.len(), 2);
    }

    /// #137: completing a cut sequence is bounded in time. A source with no
    /// more bytes ends the completion at once — the old blocking read
    /// waited for a byte that might never come — and the real reader gives
    /// up on an idle descriptor at its deadline instead of blocking.
    #[cfg(all(feature = "images", unix))]
    #[test]
    fn completing_a_cut_sequence_never_blocks_on_a_silent_peer() {
        let started = std::time::Instant::now();
        let keys = complete_and_decode(b"j\x1b[1;5".to_vec(), || None);
        assert!(matches!(keys.first(), Some(Input::Key(k)) if k.code == KeyCode::Char('j')), "{keys:?}");
        let mut more = b"B".to_vec().into_iter();
        let keys = complete_and_decode(b"\x1b[1;5".to_vec(), || more.next());
        assert!(matches!(keys.as_slice(), [Input::Key(k)] if k.code == KeyCode::Down && k.modifiers == KeyModifiers::CONTROL), "{keys:?}");

        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let deadline = std::time::Instant::now() + Duration::from_millis(80);
        assert_eq!(read_byte_before(fds[0], deadline), None, "an idle peer times out");
        assert_eq!(unsafe { libc::write(fds[1], b"zq".as_ptr().cast(), 2) }, 2);
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        assert_eq!(read_byte_before(fds[0], deadline), Some(b'z'));
        assert_eq!(read_byte_before(fds[0], deadline), Some(b'q'), "one byte per read: nothing is buffered away");
        unsafe { libc::close(fds[0]); libc::close(fds[1]); }
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

}
