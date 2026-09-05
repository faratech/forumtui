//! File-only logging. The TUI owns the alternate screen, so tracing must
//! never touch stdout/stderr. Best-effort: if the log file cannot be opened
//! the session simply runs unlogged.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

struct FileSink(Arc<Mutex<std::fs::File>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileSink {
    type Writer = FileGuard;
    fn make_writer(&'a self) -> Self::Writer {
        FileGuard(self.0.clone())
    }
}

struct FileGuard(Arc<Mutex<std::fs::File>>);

impl Write for FileGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut f) = self.0.lock() {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install a file-backed subscriber. Level comes from `WFTUI_LOG`
/// (module-targeted syntax allowed), default `warn`.
pub fn init() {
    let path = crate::config::log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let filter = EnvFilter::try_from_env("WFTUI_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(FileSink(Arc::new(Mutex::new(file))))
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .try_init();
}

#[cfg(test)]
mod tests {
    /// `WFTUI_CONFIG_DIR` is process-global, so this test has to hold
    /// `config::ENV_LOCK` like every other env-mutating test does. It used
    /// not to, and it *removed* the variable afterwards: an `api` test that
    /// had already taken the lock and pointed the config dir at its temp
    /// fixture could then build a `WfApiClient` that resolved the machine
    /// owner's real `~/.config/wftui/token.json` and overwrite it with a
    /// test token set — which is exactly what happened on the dev box
    /// (issue #565). The variable is also restored, never removed, so no
    /// window exists in which the real config dir is what resolves.
    #[test]
    fn init_does_not_panic_without_config_dir() {
        let _lock = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("WFTUI_CONFIG_DIR").ok();
        let dir = std::env::temp_dir().join(format!("wftui-test-logging-{}", std::process::id()));
        unsafe { std::env::set_var("WFTUI_CONFIG_DIR", &dir) };
        assert!(
            crate::config::log_path().starts_with(&dir),
            "the log must be written to the scratch dir, not {:?}",
            crate::config::log_path()
        );

        super::init();

        // With no previous value the scratch dir is left in place rather
        // than unset, so no window exists in which the real config dir is
        // what `config::config_root()` resolves.
        if let Some(prev) = previous {
            unsafe { std::env::set_var("WFTUI_CONFIG_DIR", prev) };
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
