//! Native Windows graphics detection. Explorer launches do not inherit the
//! terminal's environment (microsoft/terminal#13006), so ask the console.
//! Unlike Picker::from_query_stdio, this has no worker thread or blocking
//! stdio read: one timed console wait precedes each input-record read.

use super::{Policy, Tier};

#[derive(Debug, Default)]
struct Replies {
    sixel: Option<bool>,
    font: Option<(u16, u16)>,
}

impl Replies {
    /// Only recognize responses to our three queries. Unknown sequences are
    /// handed back to the input queue along with ordinary keyboard events.
    fn accept(&mut self, reply: &str) -> bool {
        if let Some(attrs) = reply
            .strip_prefix("\x1b[?")
            .and_then(|s| s.strip_suffix('c'))
            && attrs
                .split(';')
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            self.sixel = Some(attrs.split(';').skip(1).any(|s| s == "4"));
            return true;
        }
        if let Some(size) = reply
            .strip_prefix("\x1b[6;")
            .and_then(|s| s.strip_suffix('t'))
            && let Some((h, w)) = size.split_once(';')
            && let (Ok(h), Ok(w)) = (h.parse::<u16>(), w.parse::<u16>())
        {
            self.font = (w > 0 && h > 0).then_some((w, h));
            return true;
        }
        reply == "\x1b[0n"
    }

    fn policy(&self, hint: Option<Tier>) -> Policy {
        // DA1 cannot advertise iTerm2 or Kitty; keep those terminals' hints.
        // For Sixel, an actual negative DA1 beats a stale WT_SESSION.
        let tier = match hint {
            Some(t @ (Tier::Kitty | Tier::Iterm2)) => t,
            _ => match self.sixel {
                Some(true) => Tier::Sixel,
                Some(false) => Tier::Halfblocks,
                None => hint.unwrap_or(Tier::Halfblocks),
            },
        };
        Policy {
            tier,
            font: self.font.unwrap_or((10, 20)),
        }
    }
}

#[cfg(windows)]
pub(super) fn detect(hint: Option<Tier>) -> ratatui_image::picker::Picker {
    let replies = match native::probe() {
        Ok(replies) => replies,
        Err(e) => {
            tracing::warn!("native graphics query failed ({e}); using terminal hints");
            Replies::default()
        }
    };
    picker_for(replies.policy(hint))
}

fn picker_for(policy: Policy) -> ratatui_image::picker::Picker {
    // 11.x has no font-size setter. This is its only public constructor for
    // supplying a measured size without starting its stdio reader thread.
    #[allow(deprecated)]
    let mut picker = ratatui_image::picker::Picker::from_fontsize(policy.font.into());
    picker.set_protocol_type(super::protocol_of(policy.tier).expect("inline graphics tier"));
    picker
}

#[cfg(windows)]
mod native {
    use std::io::{self, Write};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, INPUT_RECORD, KEY_EVENT,
        ReadConsoleInputW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode, WriteConsoleInputW,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::Replies;

    const TIMEOUT: Duration = Duration::from_millis(500);
    const QUERY: &[u8] = b"\x1b[c\x1b[16t\x1b[5n";

    struct Console {
        input: HANDLE,
        output: HANDLE,
        input_mode: u32,
        output_mode: u32,
        deferred: Vec<INPUT_RECORD>,
        pending: Vec<INPUT_RECORD>,
    }

    impl Console {
        fn open() -> io::Result<Self> {
            // SAFETY: borrowed standard handles, validated by GetConsoleMode;
            // all mode outputs point to initialized storage.
            unsafe {
                let input = GetStdHandle(STD_INPUT_HANDLE);
                let output = GetStdHandle(STD_OUTPUT_HANDLE);
                let (mut input_mode, mut output_mode) = (0, 0);
                if GetConsoleMode(input, &mut input_mode) == 0
                    || GetConsoleMode(output, &mut output_mode) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let console = Self {
                    input,
                    output,
                    input_mode,
                    output_mode,
                    deferred: Vec::new(),
                    pending: Vec::new(),
                };
                // Construct the guard FIRST so partial setup failure restores
                // both modes, just as timeout, normal return and unwind do.
                if SetConsoleMode(
                    input,
                    input_mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT),
                ) == 0
                    || SetConsoleMode(
                        output,
                        output_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(console)
            }
        }

        fn read_before(&self, deadline: Instant) -> io::Result<Option<INPUT_RECORD>> {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            let millis = left.as_millis().clamp(1, u128::from(u32::MAX - 1)) as u32;
            // SAFETY: this is the ONLY console reader until detection returns.
            // A signaled console handle guarantees at least one input record;
            // reading exactly one cannot wait for a second record. No stdio
            // lock, worker thread or outstanding read survives the deadline.
            unsafe {
                match WaitForSingleObject(self.input, millis) {
                    WAIT_TIMEOUT => return Ok(None),
                    WAIT_OBJECT_0 => {}
                    _ => return Err(io::Error::last_os_error()),
                }
                let mut record = INPUT_RECORD::default();
                let mut count = 0;
                if ReadConsoleInputW(self.input, &mut record, 1, &mut count) == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok((count == 1).then_some(record))
            }
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            self.deferred.append(&mut self.pending);
            // SAFETY: handles are borrowed and still open; records came from
            // ReadConsoleInputW. Replay only non-query input, never flush the
            // console (which would lose keys typed during startup).
            unsafe {
                let mut offset = 0;
                while offset < self.deferred.len() {
                    let mut written = 0;
                    if WriteConsoleInputW(
                        self.input,
                        self.deferred[offset..].as_ptr(),
                        (self.deferred.len() - offset) as u32,
                        &mut written,
                    ) == 0
                        || written == 0
                    {
                        break;
                    }
                    offset += written as usize;
                }
                SetConsoleMode(self.input, self.input_mode);
                SetConsoleMode(self.output, self.output_mode);
            }
        }
    }

    pub(super) fn probe() -> io::Result<Replies> {
        probe_for(QUERY, TIMEOUT)
    }

    fn probe_for(query: &[u8], timeout: Duration) -> io::Result<Replies> {
        let mut console = Console::open()?;
        let deadline = Instant::now() + timeout;
        io::stdout().write_all(query)?;
        io::stdout().flush()?;
        let mut replies = Replies::default();
        let mut sequence = String::new();
        while let Some(record) = console.read_before(deadline)? {
            // Terminal reports arrive as key-down Unicode events with VK=0.
            // Real keys, releases, resize and mouse events belong to the TUI.
            let ch = response_char(&record);
            if let Some(ch) = ch.filter(|ch| *ch == '\x1b' || !sequence.is_empty()) {
                if ch == '\x1b' {
                    console.deferred.append(&mut console.pending);
                    sequence.clear();
                }
                console.pending.push(record);
                sequence.push(ch);
                let complete = sequence.len() > 2 && ('@'..='~').contains(&ch);
                if complete || sequence.len() >= 128 {
                    let done = sequence == "\x1b[0n";
                    if replies.accept(&sequence) {
                        console.pending.clear();
                    } else {
                        console.deferred.append(&mut console.pending);
                    }
                    sequence.clear();
                    if done {
                        break;
                    }
                }
            } else {
                console.deferred.push(record);
            }
        }
        Ok(replies)
    }

    fn response_char(record: &INPUT_RECORD) -> Option<char> {
        if u32::from(record.EventType) != KEY_EVENT {
            return None;
        }
        // SAFETY: EventType identifies the active union member.
        let key = unsafe { record.Event.KeyEvent };
        if key.bKeyDown == 0 || key.wVirtualKeyCode != 0 || key.wRepeatCount != 1 {
            return None;
        }
        // SAFETY: ReadConsoleInputW populated the UnicodeChar union member.
        let ch = unsafe { key.uChar.UnicodeChar };
        (ch > 0 && ch <= 127).then(|| char::from(ch as u8))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Mutex;
        use windows_sys::Win32::System::Console::{INPUT_RECORD_0, KEY_EVENT_RECORD};

        static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

        fn modes() -> (u32, u32) {
            let (mut input, mut output) = (0, 0);
            // SAFETY: borrowed standard handles and valid mode outputs.
            unsafe {
                assert_ne!(
                    GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut input),
                    0
                );
                assert_ne!(
                    GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut output),
                    0
                );
            }
            (input, output)
        }

        #[test]
        #[ignore = "run in native Windows Terminal, with WT_* unset and --nocapture"]
        fn native_console_probe_detects_sixel_preserves_input_and_modes() {
            let _lock = CONSOLE_LOCK.lock().unwrap();
            assert!(std::env::var_os("WT_SESSION").is_none());
            assert!(std::env::var_os("WT_PROFILE_ID").is_none());
            let before = modes();
            let key = INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: 1,
                        wRepeatCount: 1,
                        wVirtualKeyCode: 0x87, // F24
                        ..Default::default()
                    },
                },
            };
            // SAFETY: initialized record and a borrowed console input handle.
            unsafe {
                let mut written = 0;
                assert_ne!(
                    WriteConsoleInputW(GetStdHandle(STD_INPUT_HANDLE), &key, 1, &mut written),
                    0
                );
                assert_eq!(written, 1);
            }
            let replies = probe().unwrap();
            assert_eq!(
                modes(),
                before,
                "query must restore exact input AND output modes"
            );
            assert_eq!(replies.sixel, Some(true));
            assert!(
                replies.font.is_some(),
                "measure real pixels instead of assuming 10x20"
            );
            let images = super::super::super::Images::detect();
            assert_eq!(images.policy().tier, super::super::Tier::Sixel);
            assert_eq!(images.policy().font, replies.font.unwrap());
            assert_eq!(modes(), before);
            let mut console = Console::open().unwrap();
            let mut found = false;
            let deadline = Instant::now() + TIMEOUT;
            while let Some(record) = console.read_before(deadline).unwrap() {
                // SAFETY: access the key union only for a key event.
                if u32::from(record.EventType) == KEY_EVENT
                    && unsafe { record.Event.KeyEvent.wVirtualKeyCode } == 0x87
                {
                    found = true;
                    break;
                }
                console.deferred.push(record);
            }
            assert!(found, "a key queued before detection must survive it");
        }

        #[test]
        #[ignore = "requires a native console; --nocapture"]
        fn native_console_probe_times_out_without_leaving_a_reader_or_changed_modes() {
            let _lock = CONSOLE_LOCK.lock().unwrap();
            let before = modes();
            let start = Instant::now();
            let replies = probe_for(b"", Duration::from_millis(30)).unwrap();
            assert!(start.elapsed() < Duration::from_secs(1));
            assert_eq!(replies.sixel, None);
            assert_eq!(modes(), before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_terminal_without_environment_uses_sixel_and_measured_cells() {
        let mut replies = Replies::default();
        // Captured from native Windows Terminal 1.24 with WT_* removed.
        assert!(replies.accept("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c"));
        assert!(replies.accept("\x1b[6;20;10t"));
        assert!(replies.accept("\x1b[0n"));
        assert_eq!(
            replies.policy(None),
            Policy {
                tier: Tier::Sixel,
                font: (10, 20)
            }
        );
        // Larger fonts / display scaling must increase the encoded pixel size.
        assert!(replies.accept("\x1b[6;28;14t"));
        assert_eq!(replies.policy(None).font, (14, 28));
    }

    #[test]
    fn legacy_console_does_not_enable_sixel_from_stale_environment() {
        let mut replies = Replies::default();
        // Captured from the legacy console on the same Windows machine.
        assert!(replies.accept("\x1b[?61;1;6;7;21;22;23;24;28;32;42;52c"));
        assert_eq!(replies.policy(Some(Tier::Sixel)).tier, Tier::Halfblocks);
        assert_eq!(Replies::default().policy(None).tier, Tier::Halfblocks);
        assert_eq!(
            Replies::default().policy(Some(Tier::Sixel)).tier,
            Tier::Sixel
        );
    }

    #[test]
    fn unrelated_responses_and_invalid_sizes_do_not_become_capabilities() {
        let mut replies = Replies::default();
        for text in [
            "abc",
            "\x1b[A",
            "\x1b[4;800;1200t",
            "\x1b[?61;;4c",
            "\x1b[6;bad;10t",
        ] {
            assert!(!replies.accept(text), "{text:?}");
        }
        assert!(replies.accept("\x1b[?64;14;24c"));
        assert_eq!(replies.policy(None).tier, Tier::Halfblocks);
        assert!(replies.accept("\x1b[6;0;10t"));
        assert_eq!(replies.policy(None).font, (10, 20));
        for tier in [Tier::Iterm2, Tier::Kitty] {
            assert_eq!(replies.policy(Some(tier)).tier, tier);
        }
    }

    #[test]
    fn windows_sixel_payload_keeps_pixels_instead_of_halfblock_resolution() {
        let mut replies = Replies::default();
        replies.accept("\x1b[?61;4c");
        replies.accept("\x1b[6;28;14t");
        let picker = picker_for(replies.policy(None));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1536, 1024)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let loaded = super::super::decode(&picker, png.get_ref(), 60, 20).unwrap();
        let ratatui_image::protocol::Protocol::Sixel(sixel) = loaded.decoded else {
            panic!("a Windows Terminal response must produce actual Sixel, not half-block cells");
        };
        assert_eq!(sixel.size, ratatui::layout::Size::new(60, 20));
        // Sixel raster attributes: 840x560 pixels, versus 60x40 half-blocks.
        assert!(sixel.data.contains("\"1;1;840;560"));
    }
}
