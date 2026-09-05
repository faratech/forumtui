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
                Err(_) => break,
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

    #[test]
    fn filters_key_release_events() {
        let press = KeyEvent {
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
