//! wftui — WindowsForum.com terminal client.
//!
//! Panic contract: the run loop is wrapped in catch_unwind and the terminal
//! guard restores raw mode / the alternate screen on every path (Drop runs
//! during unwind). This is why the workspace profile pins panic = "unwind".

mod app;
pub mod editor;
mod event;
mod screens;
mod theme;

fn main() -> std::process::ExitCode {
    common::logging::init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(app::run())
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
