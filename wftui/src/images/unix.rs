//! Deadline-bound startup probe. No input worker outlives this function.
use ratatui_image::picker::{
    Picker, ProtocolType,
    cap_parser::{Parser, QueryStdioOptions, Response},
};
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}
impl RawMode {
    fn enter(fd: RawFd) -> io::Result<Self> {
        // SAFETY: valid initialized storage and a borrowed terminal descriptor.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, saved })
        }
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: descriptor remains open and saved came from tcgetattr.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

#[derive(Default)]
struct Replies {
    protocol: Option<ProtocolType>,
    font: Option<(u16, u16)>,
}
impl Replies {
    fn accept(&mut self, sequence: &[u8]) -> bool {
        let Ok(sequence) = std::str::from_utf8(sequence) else {
            return false;
        };
        // Restrict the parser to the responses we actually requested. In
        // particular, an arrow or a user's Alt chord is not a capability.
        let report = (sequence.starts_with("\x1b[?") && sequence.ends_with('c'))
            || (sequence.starts_with("\x1b[6;") && sequence.ends_with('t'))
            || sequence.starts_with("\x1b_Gi=31;")
            || sequence == "\x1b[0n";
        if !report {
            return false;
        }
        let mut parser = Parser::new();
        for ch in sequence.chars() {
            for response in parser.push(ch) {
                match response {
                    Response::Kitty => self.protocol = Some(ProtocolType::Kitty),
                    Response::Sixel if self.protocol != Some(ProtocolType::Kitty) => {
                        self.protocol = Some(ProtocolType::Sixel)
                    }
                    Response::CellSize(Some(font)) => self.font = Some(font),
                    _ => {}
                }
            }
        }
        true
    }
}

/// Sequence framing shared by the probe and startup key replay.
pub(crate) fn sequence_len(bytes: &[u8]) -> Option<usize> {
    if bytes.first() != Some(&0x1b) {
        return Some(1);
    }
    match bytes.get(1)? {
        b'[' => bytes
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, b)| (0x40..=0x7e).contains(*b))
            .map(|(i, _)| i + 1),
        b'_' | b']' | b'P' => bytes.windows(2).position(|b| b == b"\x1b\\").map(|i| i + 2),
        b'O' => (bytes.len() >= 3).then_some(3),
        _ => Some(2),
    }
}

fn probe(
    fd: RawFd,
    output: &mut impl Write,
    timeout: Duration,
    tmux: bool,
) -> io::Result<(Replies, Vec<u8>)> {
    let _mode = RawMode::enter(fd)?;
    let deadline = Instant::now() + timeout;
    let blacklist_protocols = if std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("KONSOLE_VERSION").is_some()
    {
        vec![ProtocolType::Kitty, ProtocolType::Sixel]
    } else {
        Vec::new()
    };
    let query = Parser::query(
        tmux,
        QueryStdioOptions {
            blacklist_protocols,
            ..Default::default()
        },
    );
    output.write_all(query.as_bytes())?;
    output.flush()?;
    let mut replies = Replies::default();
    let mut deferred = Vec::new();
    let mut sequence = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one initialized entry. VMIN=0 ensures even
        // a spurious readiness result cannot leave a blocking read behind.
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                left.as_millis().clamp(1, i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 || poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            break;
        }
        let mut byte = 0u8;
        // SAFETY: one-byte writable buffer, borrowed descriptor in raw mode.
        let read = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        if read <= 0 {
            continue;
        }
        sequence.push(byte);
        if let Some(len) = sequence_len(&sequence) {
            let done = sequence == b"\x1b[0n";
            if !replies.accept(&sequence[..len]) {
                deferred.extend_from_slice(&sequence[..len]);
            }
            sequence.drain(..len);
            if done {
                break;
            }
        } else if sequence.len() >= 512 {
            deferred.append(&mut sequence);
        }
    }
    deferred.append(&mut sequence);
    Ok((replies, deferred))
}

pub(super) fn detect() -> (Picker, Vec<u8>) {
    let tmux = std::env::var_os("TMUX").is_some()
        || std::env::var("TERM")
            .is_ok_and(|term| term.starts_with("screen") || term.starts_with("tmux"));
    let (replies, deferred) = probe(
        libc::STDIN_FILENO,
        &mut io::stdout(),
        Duration::from_secs(2),
        tmux,
    )
    .unwrap_or_else(|error| {
        tracing::warn!("graphics probe failed: {error}");
        (Replies::default(), Vec::new())
    });
    let fallback = crossterm::terminal::window_size().ok().and_then(|size| {
        let font = (
            size.width.checked_div(size.columns)?,
            size.height.checked_div(size.rows)?,
        );
        (font.0 > 0 && font.1 > 0).then_some(font)
    });
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(replies.font.or(fallback).unwrap_or((10, 20)).into());
    if let Some(protocol) = replies.protocol {
        picker.set_protocol_type(protocol);
    }
    (picker, deferred)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    #[test]
    fn probe_timeout_restores_mode_and_preserves_keys_without_a_worker() {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: valid output pointers; the successful descriptors are owned below.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        use std::os::fd::AsRawFd;
        let fd = slave.as_raw_fd();
        let mut before: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut before) }, 0);
        master.write_all("jZoë\x1b[A".as_bytes()).unwrap();
        let start = Instant::now();
        let (_, deferred) = probe(fd, &mut Vec::new(), Duration::from_millis(30), false).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(deferred, "jZoë\x1b[A".as_bytes());
        let mut after = before;
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut after) }, 0);
        assert_eq!(before.c_lflag, after.c_lflag);
        assert_eq!(before.c_iflag, after.c_iflag);
        assert_eq!(before.c_cc, after.c_cc);
        // No worker exists to consume this key or change the later raw mode.
        let _raw = RawMode::enter(fd).unwrap();
        master.write_all(b"k").unwrap();
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) },
            1
        );
        assert_eq!(byte, b'k');
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut after) }, 0);
        assert_eq!(after.c_lflag & (libc::ICANON | libc::ECHO), 0);
    }

    #[test]
    fn capability_parser_keeps_keys_and_selects_measured_graphics() {
        let mut replies = Replies::default();
        assert!(!replies.accept(b"\x1b[A"));
        assert!(replies.accept(b"\x1b[?62;4c"));
        assert!(replies.accept(b"\x1b[6;24;12t"));
        assert_eq!(replies.protocol, Some(ProtocolType::Sixel));
        assert_eq!(replies.font, Some((12, 24)));
        assert!(replies.accept(b"\x1b_Gi=31;OK\x1b\\"));
        assert_eq!(replies.protocol, Some(ProtocolType::Kitty));
    }
}
