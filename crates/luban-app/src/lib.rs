//! Luban Notepad — a cross-platform, GPUI-based reimplementation of the
//! Notepad++ editing experience.

rust_i18n::i18n!("locales", fallback = "en");

mod app;
mod cli;
mod column_editor;
mod commands;
mod dialogs;
mod document_map;
mod explorer;
mod file_picker;
mod file_watch;
mod find;
mod highlighter;
mod huge_file;
mod huge_gutter;
mod huge_scrollbar;
mod i18n;
mod icons;
mod ipc;
mod large_file;
mod logging;
mod macro_ui;
mod navigation;
#[cfg(target_env = "ohos")]
pub mod ohos_file_save;
mod outline;
mod palette;
mod path_prompt;
mod plugin_host;
mod plugin_manager;
mod plugin_store;
mod preferences;
mod quick_open;
mod settings_search;
mod snippet_ui;
mod syntax_theme;
mod tab_groups;
mod theme;
mod util;
mod window_registry;
mod workspace_replace;

use std::path::PathBuf;
use std::sync::Arc;

use app::LaunchOptions;
use gpui_kit::*;
use luban_core::settings::Settings;

use crate::i18n::tr_args;

use crate::cli::{parse_goto, GotoTarget};
use crate::ipc::{IpcBridge, IpcQueue};
use luban_core::disk_engine::RecoveryInfo;

struct ParsedArgs {
    files: Vec<PathBuf>,
    new_window: bool,
    smoke_test: bool,
    safe_mode: bool,
    wait: bool,
    goto: Option<GotoTarget>,
    diff: Option<(PathBuf, PathBuf)>,
    stdin: bool,
    command: Option<String>,
    skip_session: bool,
}

fn parse_args() -> ParsedArgs {
    let mut files = Vec::new();
    let mut new_window = false;
    let mut smoke_test = false;
    let mut safe_mode = false;
    let mut wait = false;
    let mut goto = None;
    let mut diff = None;
    let mut stdin = false;
    let mut command = None;
    let mut skip_session = false;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "{}",
                    tr_args(
                        "cli.help",
                        &[("version", env!("CARGO_PKG_VERSION").to_string())]
                    )
                );
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("luban-notepad {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--new" | "-n" => skip_session = true,
            "--new-window" => new_window = true,
            "--wait" => wait = true,
            "--safe-mode" => safe_mode = true,
            "--stdin" => stdin = true,
            "--smoke-test" => smoke_test = true,
            "--goto" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("--goto requires path:line[:column]");
                    std::process::exit(2);
                };
                match parse_goto(value) {
                    Ok(target) => goto = Some(target),
                    Err(error) => {
                        eprintln!("{error}");
                        std::process::exit(2);
                    }
                }
            }
            "--diff" => {
                let (Some(left), Some(right)) = (args.get(index + 1), args.get(index + 2)) else {
                    eprintln!("--diff requires two file paths");
                    std::process::exit(2);
                };
                diff = Some((PathBuf::from(left), PathBuf::from(right)));
                index += 2;
            }
            "--command" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("--command requires a command id");
                    std::process::exit(2);
                };
                command = Some(value.clone());
            }
            "--" => {
                files.extend(args[index + 1..].iter().map(PathBuf::from));
                break;
            }
            "-" => stdin = true,
            other if other.starts_with('-') => {
                eprintln!("unknown option {other}; use --help for usage");
                std::process::exit(2);
            }
            other => files.push(PathBuf::from(other)),
        }
        index += 1;
    }
    ParsedArgs {
        files,
        new_window,
        smoke_test,
        safe_mode,
        wait,
        goto,
        diff,
        stdin,
        command,
        skip_session,
    }
}

/// Whether the application removes the window-manager titlebar itself.
///
/// On X11, gpui-component's root view reserves a 20px transparent margin for a
/// client-side shadow. Only a compositor can blend that margin; without one it
/// is painted black. The window therefore uses the vendored gpui-pre-linux
/// patch semantics: the window is opened with a `Client` decorations request,
/// which the patched X11 backend interprets as "the application draws its own
/// chrome" — it writes the borderless `_MOTIF_WM_HINTS` *before* the window is
/// mapped (so no native titlebar ever appears) while still reporting server
/// decorations (so no shadow margin is reserved). Wayland always composites,
/// so it keeps plain client decorations.
fn manual_window_chrome() -> bool {
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        std::env::var_os("WAYLAND_DISPLAY").is_none_or(|display| display.is_empty())
    }
    #[cfg(not(all(target_os = "linux", not(target_env = "ohos"))))]
    {
        false
    }
}

/// Re-apply the borderless `_MOTIF_WM_HINTS` (`[2, 0, 0, 0, 0]`) on the X11
/// window, leaving the client area to the application-drawn titlebar. The
/// vendored gpui-pre-linux patch already writes this hint before the window is
/// mapped, so this normally finds the property unchanged and writes nothing;
/// it is kept as a fallback for window-manager restarts and unusual recovery
/// paths. Returns whether the hint had to be written.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn apply_borderless_hint(window: &Window) -> anyhow::Result<bool> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};

    let xid = match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Xlib(handle)) => handle.window as u32,
        Ok(RawWindowHandle::Xcb(handle)) => handle.window.get(),
        Ok(other) => anyhow::bail!("cannot remove window decorations from {other:?}"),
        Err(error) => anyhow::bail!("cannot read the X11 window handle: {error}"),
    };

    let (connection, _) = x11rb::connect(None)?;
    let motif = connection
        .intern_atom(false, b"_MOTIF_WM_HINTS")?
        .reply()?
        .atom;
    let current = connection
        .get_property(false, xid, motif, AtomEnum::ANY, 0, 5)?
        .reply()?;
    let desired = [2u32, 0, 0, 0, 0];
    if current
        .value32()
        .is_some_and(|values| values.take(5).collect::<Vec<_>>() == desired)
    {
        return Ok(false);
    }

    // [flags, functions, decorations, input_mode, status]: remove the window
    // manager decorations while leaving everything else alone.
    let data: Vec<u8> = desired
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    connection.change_property(PropMode::REPLACE, xid, motif, motif, 32, 5, &data)?;
    connection.flush()?;
    Ok(true)
}

/// Re-apply the borderless hint right after the window opens and keep it
/// applied while the window manager settles. With the vendored gpui-pre-linux
/// patch the hint is already in place before mapping, so this is a pure
/// fallback (e.g. after a window-manager restart) and does not participate in
/// the first-show timing.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
pub(crate) fn remove_window_decorations(handle: AnyWindowHandle, cx: &mut App) {
    let _ = handle.update(cx, |_, window, _| match apply_borderless_hint(window) {
        Ok(true) => log::info!("removed the window manager titlebar"),
        Ok(false) => {}
        Err(error) => log::warn!("failed to remove the window manager titlebar: {error:#}"),
    });

    cx.spawn(async move |cx| {
        for delay in [120u64, 350, 800] {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = handle.update(cx, |_, window, _| {
                let _ = apply_borderless_hint(window);
            });
        }
    })
    .detach();
}

/// Decode the icon embedded in the binary so the taskbar entry is correct even
/// when the desktop entry and icon theme are not installed (X11 only).
pub(crate) fn window_icon() -> Option<Arc<image::RgbaImage>> {
    const ICON_PNG: &[u8] =
        include_bytes!("../../../packaging/linux/icons/hicolor/256x256/apps/luban-notepad.png");
    match image::load_from_memory(ICON_PNG) {
        Ok(icon) => Some(Arc::new(icon.to_rgba8())),
        Err(error) => {
            log::warn!("failed to decode the embedded window icon: {error}");
            None
        }
    }
}

fn readable_file(path: &std::path::Path) -> bool {
    path.is_file() && std::fs::File::open(path).is_ok()
}

fn read_stdin() -> String {
    use std::io::Read as _;
    const LIMIT: u64 = 64 * 1024 * 1024;
    let mut bytes = Vec::new();
    let mut stdin = std::io::stdin().take(LIMIT + 1);
    if let Err(error) = stdin.read_to_end(&mut bytes) {
        eprintln!("cannot read stdin: {error}");
        std::process::exit(3);
    }
    if bytes.len() as u64 > LIMIT {
        eprintln!("stdin is larger than {LIMIT} bytes");
        std::process::exit(3);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn run_desktop() {
    // Remove journals left behind by terminated instances. Only directories
    // older than a day are touched so another running instance is unaffected.
    let _ = luban_core::disk_engine::cleanup_stale(std::time::Duration::from_secs(24 * 3600));
    logging::init();

    // Resolve the UI language before parsing arguments so that `--help` is
    // localized too. `LubanApp::new` loads the same settings again for its own
    // state; keeping the two loads independent avoids threading the struct
    // through the launch options.
    let settings = Settings::load();
    i18n::apply_language(&settings.ui_language);
    // Expired snapshots are cheap to remove and must disappear even when the
    // window is never opened.
    let history = luban_core::local_history::HistoryStore::system();
    if let Err(error) = history.prune(
        std::time::Duration::from_secs(settings.local_history_days.saturating_mul(24 * 3600)),
        settings.local_history_max_mb.saturating_mul(1024 * 1024),
    ) {
        log::warn!("could not prune local history: {error}");
    }

    let args = parse_args();
    let stdin = if args.stdin { Some(read_stdin()) } else { None };
    let config_dir = luban_core::settings::config_dir();
    let request = ipc::LaunchRequest::new(
        args.files.clone(),
        args.goto.as_ref().map(|target| ipc::GotoRequest {
            path: target.path.clone(),
            line: target.line,
            column: target.column,
        }),
        args.diff.clone(),
        stdin.clone(),
        args.command.clone(),
        args.new_window,
        args.wait,
    );
    let mut server = None;
    let mut bridge = None;
    let mut session_writer = !args.wait;
    if args.safe_mode || args.skip_session {
        // These flags request a distinct process; the primary keeps its own
        // session file. If nobody owns the lock, this process becomes primary.
        match ipc::start(&config_dir) {
            Ok((new_server, new_bridge)) => {
                server = Some(new_server);
                bridge = Some(new_bridge);
            }
            Err(_) => session_writer = false,
        }
    } else {
        match ipc::forward(&config_dir, &request) {
            Ok(response) => {
                if !response.ok {
                    if let Some(error) = response.error {
                        eprintln!("{error}");
                    }
                    std::process::exit(3);
                }
                std::process::exit(0);
            }
            Err(ipc::ForwardError::NoPrimary) => match ipc::start(&config_dir) {
                Ok((new_server, new_bridge)) => {
                    server = Some(new_server);
                    bridge = Some(new_bridge);
                }
                Err(error) => {
                    // Another process may have won the race; retry forwarding.
                    match ipc::forward(&config_dir, &request) {
                        Ok(response) => {
                            if !response.ok {
                                if let Some(error) = response.error {
                                    eprintln!("{error}");
                                }
                                std::process::exit(3);
                            }
                            std::process::exit(0);
                        }
                        Err(_) => {
                            eprintln!("{error}");
                            std::process::exit(3);
                        }
                    }
                }
            },
            Err(ipc::ForwardError::Failed(error)) => {
                eprintln!("{error}");
                std::process::exit(3);
            }
        }
    }
    let owns_ipc = server.is_some();
    let _server = server;
    if let Some(bridge) = &bridge {
        ipc::set_shared_queue(bridge.queue.clone());
    }
    let bridge = bridge;
    // A launch that becomes the primary has no IPC response channel, so report
    // invalid file targets before opening a window.
    if let Some(target) = &args.goto {
        if !readable_file(&target.path) {
            eprintln!("cannot open {}", target.path.display());
            if owns_ipc {
                ipc::cleanup(&config_dir);
            }
            std::process::exit(3);
        }
    }
    if let Some((left, right)) = &args.diff {
        if !readable_file(left) {
            eprintln!("cannot open {}", left.display());
            if owns_ipc {
                ipc::cleanup(&config_dir);
            }
            std::process::exit(3);
        }
        if !readable_file(right) {
            eprintln!("cannot open {}", right.display());
            if owns_ipc {
                ipc::cleanup(&config_dir);
            }
            std::process::exit(3);
        }
    }
    for file in &args.files {
        if !readable_file(file) {
            eprintln!("cannot open {}", file.display());
            if owns_ipc {
                ipc::cleanup(&config_dir);
            }
            std::process::exit(3);
        }
    }
    // Crash recoveries are collected before any window exists so the first
    // window can show the offer next to its restored tabs. Expired entries are
    // removed; entries belonging to a running instance stay within retention.
    let recoveries = luban_core::disk_engine::scan_recovery(luban_core::disk_engine::RETENTION);
    let manual_chrome = manual_window_chrome();

    // Restore one window per saved window when the session holds any. Command
    // line files take precedence and skip the session, matching the previous
    // single-window behaviour.
    let restore = settings.restore_session
        && !args.skip_session
        && !args.new_window
        && !args.wait
        && args.files.is_empty();
    let mut windows: Vec<Option<luban_core::session::WindowSession>> = if restore {
        let session = luban_core::session::Session::load();
        if session.windows.is_empty() {
            vec![None]
        } else {
            session.windows.into_iter().map(Some).collect()
        }
    } else {
        vec![None]
    };
    if windows.is_empty() {
        windows.push(None);
    }

    run_ui(
        gpui_kit::application(),
        windows,
        LaunchContext {
            files: args.files.clone(),
            smoke_test: args.smoke_test,
            safe_mode: args.safe_mode,
            wait: args.wait,
            session_writer,
            goto: args
                .goto
                .as_ref()
                .map(|target| (target.path.clone(), target.line, target.column)),
            diff: args.diff.clone(),
            stdin: stdin.clone(),
            command: args.command.clone(),
            ipc_queue: ipc::shared_queue(),
            recoveries: recoveries.clone(),
            bridge,
            manual_chrome,
            embedded: false,
        },
    );
}

/// Everything the UI needs from the launch path.
///
/// Desktop startup fills it from the command line and IPC; the OpenHarmony
/// entry point leaves the desktop-only fields empty.
struct LaunchContext {
    files: Vec<PathBuf>,
    smoke_test: bool,
    safe_mode: bool,
    wait: bool,
    session_writer: bool,
    goto: Option<(PathBuf, usize, usize)>,
    diff: Option<(PathBuf, PathBuf)>,
    stdin: Option<String>,
    command: Option<String>,
    ipc_queue: Option<IpcQueue>,
    recoveries: Vec<RecoveryInfo>,
    bridge: Option<IpcBridge>,
    manual_chrome: bool,
    /// The host owns the run loop (OpenHarmony); use `run_embedded` and keep
    /// the returned handle alive for the process lifetime.
    embedded: bool,
}

/// Runs the GPUI event loop on a caller-supplied application.
///
/// The OpenHarmony entry point builds an [`Application`] with the platform from
/// `gpui-ohos` and hands it here; the desktop entry point passes
/// [`gpui_kit::application`].
fn run_ui(
    application: Application,
    mut windows: Vec<Option<luban_core::session::WindowSession>>,
    launch: LaunchContext,
) {
    let ui = move |cx: &mut App| {
        gpui_kit::init(cx);
        if let Some(bridge) = launch.bridge {
            let wake_rx = std::sync::Arc::new(std::sync::Mutex::new(bridge.wake_rx));
            cx.spawn(async move |cx| loop {
                let rx = wake_rx.clone();
                let received = cx
                    .background_executor()
                    .spawn(async move {
                        rx.lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .recv()
                            .is_ok()
                    })
                    .await;
                if !received {
                    break;
                }
                window_registry::notify_all(cx);
            })
            .detach();
        }
        let ipc_queue = launch.ipc_queue.clone();
        let mut first = true;
        for saved in windows.drain(..) {
            let id = window_registry::next_window_id();
            let bounds = saved
                .as_ref()
                .map(|session| window_registry::restored_bounds(session, cx))
                .unwrap_or_else(|| WindowBounds::Windowed(window_registry::centered_bounds(cx)));
            let options = LaunchOptions {
                files: if first {
                    launch.files.clone()
                } else {
                    Vec::new()
                },
                smoke_test: first && launch.smoke_test,
                safe_mode: launch.safe_mode,
                session_writer: launch.session_writer,
                wait: launch.wait,
                goto: if first { launch.goto.clone() } else { None },
                diff: if first { launch.diff.clone() } else { None },
                stdin: if first { launch.stdin.clone() } else { None },
                command: if first { launch.command.clone() } else { None },
                exit_on_command_failure: first && launch.command.is_some(),
                command_result: None,
                ipc_queue: ipc_queue.clone(),
                manual_window_chrome: launch.manual_chrome,
                recoveries: if first {
                    launch.recoveries.clone()
                } else {
                    Vec::new()
                },
                init: saved
                    .map(|session| app::WindowInit::Session(Box::new(session)))
                    .unwrap_or(app::WindowInit::Blank),
                auto_recovery: false,
                window_id: id,
            };
            if let Err(error) =
                window_registry::open_window_with(options, bounds, launch.manual_chrome, cx)
            {
                log::error!("failed to open Luban Notepad window: {error:#}");
                if first {
                    cx.quit();
                    return;
                }
            }
            first = false;
        }
        cx.activate(true);
    };
    let application = application.with_assets(icons::AppAssets);
    if launch.embedded {
        // `run_embedded` hands the loop to the host; the handle keeps the app
        // state alive. The entry point lives as long as the process.
        std::mem::forget(application.run_embedded(ui));
    } else {
        application.run(ui);
    }
}

/// Startup data for a host that supplies its own GPUI platform.
///
/// The OpenHarmony entry point cannot use the desktop command line or
/// single-instance IPC: the ability launches the process without arguments and
/// the sandbox has no stable per-user socket directory. Files arrive from the
/// system file picker instead.
pub struct EmbeddedStartup {
    pub files: Vec<PathBuf>,
    pub restore_session: bool,
    pub smoke_test: bool,
}

impl Default for EmbeddedStartup {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            restore_session: true,
            smoke_test: false,
        }
    }
}

/// Runs the UI on a platform provided by the caller (OpenHarmony).
pub fn run_embedded(application: Application, startup: EmbeddedStartup) {
    // Same startup preparation as the desktop entry point, minus the command
    // line, IPC and stdin paths.
    let _ = luban_core::disk_engine::cleanup_stale(std::time::Duration::from_secs(24 * 3600));
    logging::init();

    let settings = Settings::load();
    i18n::apply_language(&settings.ui_language);
    let history = luban_core::local_history::HistoryStore::system();
    if let Err(error) = history.prune(
        std::time::Duration::from_secs(settings.local_history_days.saturating_mul(24 * 3600)),
        settings.local_history_max_mb.saturating_mul(1024 * 1024),
    ) {
        log::warn!("could not prune local history: {error}");
    }

    let recoveries = luban_core::disk_engine::scan_recovery(luban_core::disk_engine::RETENTION);
    let mut windows: Vec<Option<luban_core::session::WindowSession>> =
        if startup.restore_session && startup.files.is_empty() {
            let session = luban_core::session::Session::load();
            if session.windows.is_empty() {
                vec![None]
            } else {
                session.windows.into_iter().map(Some).collect()
            }
        } else {
            vec![None]
        };
    if windows.is_empty() {
        windows.push(None);
    }

    run_ui(
        application,
        windows,
        LaunchContext {
            files: startup.files,
            smoke_test: startup.smoke_test,
            safe_mode: false,
            wait: false,
            session_writer: true,
            goto: None,
            diff: None,
            stdin: None,
            command: None,
            ipc_queue: None,
            recoveries,
            bridge: None,
            manual_chrome: manual_window_chrome(),
            embedded: true,
        },
    );
}
