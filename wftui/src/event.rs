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
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("wftui-reader".into())
        .spawn(move || {
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
}
