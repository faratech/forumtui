//! A snapshot of the terminal's line discipline, taken before anything in the
//! process has had a chance to touch it, and re-applied on every exit path.
//!
//! Why this exists (issue #532): the graphics capability query in
//! `images::detect` hands stdin to `ratatui-image`, which clears `ICANON`/
//! `ECHO` in a helper thread and — when the terminal never answers — leaves
//! that thread blocked in `read()` with the modified attributes still in
//! force. crossterm's `enable_raw_mode()` then snapshots *those* attributes as
//! "the original", so `disable_raw_mode()` on exit hands the shell back a
//! `-icanon -echo` terminal. Taking our own `tcgetattr` before `detect()` and
//! re-applying it after `disable_raw_mode()` makes the exit state independent
//! of anything the leaked thread did or did not do.
//!
//! Unix only, and cfg-gated to nothing on Windows so the Windows build stays
//! clean (there is no termios there; the native probe in `images/windows.rs`
//! restores its own console modes and never leaves a reader thread behind).

/// Capture the current terminal attributes. Call once, as early as possible,
/// and before `images::detect()`. A second call is a no-op.
pub fn snapshot() {
    #[cfg(unix)]
    imp::snapshot();
}

/// Re-apply the captured attributes, if any. Safe to call any number of times
/// and from a panic hook.
pub fn restore() {
    #[cfg(unix)]
    imp::restore();
}

#[cfg(unix)]
mod imp {
    use std::sync::OnceLock;

    /// `libc::termios` is a plain POD struct (integers plus a `c_cc` array),
    /// so it is `Send`/`Sync` on its own and needs no wrapper lock.
    static ORIGINAL: OnceLock<Option<libc::termios>> = OnceLock::new();

    pub fn snapshot() {
        ORIGINAL.get_or_init(|| {
            // SAFETY: `isatty`/`tcgetattr` on a borrowed fd; the struct is
            // fully written by `tcgetattr` before it is read.
            unsafe {
                if libc::isatty(libc::STDIN_FILENO) != 1 {
                    return None;
                }
                let mut attrs = std::mem::zeroed::<libc::termios>();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut attrs) == 0 {
                    Some(attrs)
                } else {
                    None
                }
            }
        });
    }

    pub fn restore() {
        if let Some(Some(attrs)) = ORIGINAL.get() {
            // SAFETY: `attrs` came from `tcgetattr` on this same fd.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, attrs);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The public surface is a pair of idempotent no-ops when there is no tty
    /// (which is where tests run), on every platform.
    #[test]
    fn snapshot_and_restore_are_safe_without_a_terminal() {
        super::snapshot();
        super::snapshot();
        super::restore();
        super::restore();
    }
}
