//! wftui — WindowsForum.com terminal client.
//!
//! Panic contract: the run loop is wrapped in catch_unwind and the terminal
//! guard restores raw mode / the alternate screen on every path (Drop runs
//! during unwind). This is why the workspace profile pins panic = "unwind".

mod app;
pub mod chrome;
pub mod editor;
mod event;
pub mod glyph;
pub mod images;
mod overlay;
mod screens;
mod theme;
pub mod tty;

fn main() -> std::process::ExitCode {
    common::logging::init();

    // Panic hook: ensure raw mode, mouse capture, and alternate screen are reset
    // before the default panic hook prints backtrace/errors to stdout/stderr.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Shared with `TerminalGuard::drop` — each crossterm restore command
        // is issued in its own `execute!` call so a Windows-unsupported
        // PopKeyboardEnhancementFlags can never skip the rest (issue #531).
        app::restore_terminal();
        original_hook(panic_info);
    }));

    // Graphics tier detection FIRST, while this thread is still the only
    // reader of stdin. The query writes capability escapes and blocks reading
    // the terminal's replies; CLAUDE.md hard rule 3 says input comes from the
    // one dedicated blocking reader thread (`event::spawn_reader`, started
    // inside `app::run`), and two readers racing stdin would split the reply
    // exactly the way the login-corruption incident split escape sequences.
    // It also runs before raw mode and the alternate screen: the query drives
    // termios itself, which is the ordering ratatui-image's own binary uses.
    // …and before either, snapshot the terminal's line discipline, so the exit
    // state cannot be poisoned by the query's leaked reader thread (#532).
    tty::snapshot();
    let images = images::Images::detect();
    // Undo anything the query left behind BEFORE crossterm's first
    // `enable_raw_mode()` snapshots the mode it finds.
    tty::restore();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(app::run(images))
    }));
    match result {
        Ok(code) => code.into(),
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            eprintln!("wftui crashed: {detail}");
            eprintln!("a log may exist at the config dir (wftui/wftui.log)");
            std::process::ExitCode::FAILURE
        }
    }
}
