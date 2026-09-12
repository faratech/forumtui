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

/// Replay user input consumed before the normal terminal reader existed.
/// This runs on that sole reader, after the application entered raw mode.
#[cfg(all(feature = "images", unix))]
fn startup_events(mut bytes: Vec<u8>) -> Vec<Input> {
    // Complete a UTF-8 character split by the probe deadline. This is an
    // ordinary blocking input read on the sole reader, never a probe worker.
    while let Err(error) = std::str::from_utf8(&bytes) {
        if error.error_len().is_some() { break; }
        use std::io::Read;
        let mut byte = [0];
        if std::io::stdin().read_exact(&mut byte).is_err() { break; }
        bytes.push(byte[0]);
    }
    decode_startup_events(&bytes)
}

#[cfg(all(feature = "images", unix))]
fn decode_startup_events(bytes: &[u8]) -> Vec<Input> {
    let text = String::from_utf8_lossy(bytes);
    let mut remaining = text.as_ref();
    let mut events = Vec::new();
    while !remaining.is_empty() {
        if let Some(paste) = remaining.strip_prefix("\x1b[200~")
            && let Some(end) = paste.find("\x1b[201~")
        {
            events.push(Input::Paste(paste[..end].to_owned()));
            remaining = &paste[end + 6..];
            continue;
        }
        let mut modifiers = KeyModifiers::NONE;
        if remaining.starts_with('\x1b') && remaining.len() > 1 {
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

}
