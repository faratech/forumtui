//! wftui — WindowsForum Terminal, the terminal client for windowsforum.com.
//!
//! Panic contract: the run loop is wrapped in catch_unwind and the terminal
//! guard restores raw mode / the alternate screen on every path (Drop runs
//! during unwind). This is why the workspace profile pins panic = "unwind".

mod app;
pub mod chrome;
pub mod editor;
mod event;
pub mod glyph;
pub mod hit;
pub mod images;
mod overlay;
mod screens;
mod theme;
pub mod tty;

/// What the command line asked for. Hand-parsed: four flags and one
/// positional do not need a parser crate.
struct Cli {
    site: Option<String>,
    print_config: bool,
    init_config: bool,
}

fn parse_cli() -> Result<Cli, String> {
    let mut cli = Cli { site: None, print_config: false, init_config: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--site" | "-s" => {
                cli.site = Some(args.next().ok_or("--site needs a name")?);
            }
            "--print-config" => cli.print_config = true,
            "--init-config" => cli.init_config = true,
            "--version" | "-V" => {
                println!("wftui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "wftui — {}, a terminal client for XenForo forums\n\n\
                     usage: wftui [<site> | --site <name>] [--print-config] [--init-config] [--version]\n\n\
                     Sites are read from {}; with no file, {}.\n\
                     --init-config writes an annotated example there; --print-config shows the\n\
                     resolved site. WFTUI_SITE selects a site when no argument does.",
                    common::site::PRODUCT_NAME,
                    common::site::config_path().display(),
                    if common::site::HAS_BUILTIN_SITE {
                        "windowsforum.com"
                    } else {
                        "the client asks for a forum on first run"
                    }
                );
                std::process::exit(0);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            name => {
                if cli.site.is_some() {
                    return Err(format!("only one site may be named; saw {name} too"));
                }
                cli.site = Some(name.to_string());
            }
        }
    }
    Ok(cli)
}

fn main() -> std::process::ExitCode {
    common::logging::init();

    // Which forum, before anything else: a bad config file or an unknown
    // site name is reported on the normal screen and nothing is started.
    let cli = match parse_cli() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wftui: {e} (try --help)");
            return std::process::ExitCode::from(2);
        }
    };
    let config_path = common::site::config_path();
    if cli.init_config {
        if config_path.exists() {
            eprintln!("wftui: {} already exists; not overwriting it", config_path.display());
            return std::process::ExitCode::from(2);
        }
        if let Some(dir) = config_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        return match std::fs::write(&config_path, common::site::Config::example_json()) {
            Ok(()) => {
                println!("wrote {}", config_path.display());
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("wftui: cannot write {}: {e}", config_path.display());
                std::process::ExitCode::from(2)
            }
        };
    }
    // `None` is the generic edition's first run: nothing names a site yet,
    // and the client answers with its Setup screen rather than an error.
    let (site, needs_setup) = match common::site::Config::load(&config_path)
        .and_then(|cfg| common::site::resolve_or_setup(&cfg, cli.site.as_deref(), &common::config::config_dir()))
    {
        Ok(Some(site)) => (std::sync::Arc::new(site), false),
        Ok(None) if cli.print_config => {
            eprintln!(
                "wftui: no site configured — run wftui once to set one up, or `wftui --init-config` and edit {}",
                config_path.display()
            );
            return std::process::ExitCode::from(2);
        }
        Ok(None) => (std::sync::Arc::new(common::site::SiteConfig::placeholder()), true),
        Err(e) => {
            eprintln!("wftui: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    if cli.print_config {
        println!("{site:#?}");
        println!("token store: {}", common::config::token_path_for(&site.name).display());
        println!("user agent:  {}", site.user_agent());
        println!("scopes:      {}", site.effective_scopes().join(" "));
        return std::process::ExitCode::SUCCESS;
    }

    // A staged update is applied before anything else touches the terminal
    // (#722): the swap is plain file renames, and its one line of stderr
    // must land on the normal screen, not the alternate one (issue #546).
    // This process is still the old image afterwards; `run` is told so.
    let update_cfg = common::update::UpdateConfig::from_env(env!("CARGO_PKG_VERSION"));
    let just_applied = match common::update::apply_pending_update(&update_cfg) {
        common::update::ApplyOutcome::Applied(version) => {
            eprintln!("wftui: update to v{version} applied; this session still runs the old build");
            Some(version)
        }
        common::update::ApplyOutcome::Incomplete(why) => {
            eprintln!("wftui: update not applied ({why}); will check again this session");
            None
        }
        common::update::ApplyOutcome::Refused { version, why } => {
            eprintln!("wftui: update v{version} is staged but not applied: {why}");
            None
        }
        common::update::ApplyOutcome::NothingPending => None,
    };

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
    // state always returns to the original terminal settings on exit.
    tty::snapshot();
    let mut images = images::Images::detect();
    // The sign-in logo: the embedded one for the built-in site, the
    // configured PNG for another (unreadable or absent = the text mark).
    images.set_logo(site_logo(&site));
    // Undo anything the query left behind BEFORE crossterm's first
    // `enable_raw_mode()` snapshots the mode it finds.
    tty::restore();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(app::run(images, update_cfg, just_applied, site, needs_setup))
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

/// The bytes the sign-in screen may draw as a picture. The built-in site's
/// come with the binary; another site's are read from its `brand.logo`,
/// capped like any other image fetch, and a failure is a log line, never a
/// reason not to start.
fn site_logo(site: &common::site::SiteConfig) -> Option<std::sync::Arc<[u8]>> {
    match &site.brand.logo {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) if bytes.len() <= images::MAX_IMAGE_BYTES => Some(bytes.into()),
            Ok(bytes) => {
                tracing::warn!("logo {} is {} bytes, over the {} cap; using the text mark", path.display(), bytes.len(), images::MAX_IMAGE_BYTES);
                None
            }
            Err(e) => {
                tracing::warn!("logo {} unreadable ({e}); using the text mark", path.display());
                None
            }
        },
        None if site.is_builtin() => images::embedded_logo(),
        None => None,
    }
}
