//! RealRDP: mstsc-compatible RDP client built on IronRDP.
//!
//! Desktop sessions reuse the upstream `ironrdp-viewer` App. RemoteApp sessions
//! (`remoteapplicationmode:i:1`) use [`rail_app::RailApp`], which maps every remote window to a
//! local X11 window; desktop sessions run natively on Wayland when available.

mod dialog;
mod hyprland;
mod keymap;
mod rail_app;
mod window_order;

use anyhow::Context as _;
use ironrdp::client::output_channel::output_channel;
use ironrdp::client::rdp::{RdpClient, RdpOutputEvent};
use ironrdp::pdu::gcc::{ClientMonitorData, MONITOR_COUNT_MAX, Monitor, MonitorFlags};
use ironrdp_cfg::PropertySetExt as _;
use ironrdp_rdpdr_native::backend::NixRdpdrBackendFactory;
use ironrdp_rdpdr_native::printer::PrintTarget;
use ironrdp_viewer::app::App;
use ironrdp_viewer::cli::ViewerConfig;
use std::os::unix::process::CommandExt as _;
use tokio::runtime;
use winit::dpi::PhysicalSize;
use winit::event_loop::EventLoop;
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::platform::x11::{EventLoopBuilderExtX11 as _, WindowAttributesExtX11};
use winit::window::{Icon, WindowAttributes};

/// `StartupWMClass` in `realrdp.desktop`: lets the desktop match our windows to its icon.
pub const WM_CLASS: &str = "realrdp";
pub const ICON_PNG: &[u8] = include_bytes!("../realrdp-icons/hicolor/64x64/apps/realrdp.png");

/// The application icon for `_NET_WM_ICON`.
fn window_icon() -> Option<Icon> {
    let data = eframe::icon_data::from_png_bytes(ICON_PNG).ok()?;
    Icon::from_rgba(data.rgba, data.width, data.height).ok()
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().collect();
    // `realrdp file.rdp` (and `%U` from the desktop entry) opens that connection.
    if args.len() == 2 && args[1].ends_with(".rdp") {
        let path = args[1].strip_prefix("file://").unwrap_or(&args[1]).to_owned();
        args.splice(1.., ["--rdp-file".to_owned(), path]);
    }
    let rdp_file_arg = args.iter().position(|a| a == "--rdp-file").and_then(|i| args.get(i + 1)).cloned();
    let mut has_password =
        std::env::var_os("RDP_PASSWORD").is_some() || args.iter().any(|a| a == "-p" || a == "--password");
    let rdp_properties = rdp_file_arg.as_deref().and_then(rdp_properties);
    // Saved credentials connect without asking, like mstsc.
    if !has_password
        && let Some(ps) = &rdp_properties
        && let Some(host) = ps.full_address().ok().flatten()
        && let Some(password) = dialog::saved_password(&host.to_string(), ps.username().unwrap_or_default())
    {
        args.extend(["--password".into(), password]);
        has_password = true;
    }
    let prompt = if args.len() == 1 {
        Some(dialog::run()?)
    } else if let Some(rdp_file) = &rdp_file_arg
        && !has_password
    {
        // Credentials are missing on the command line: ask in a window, not on the terminal.
        Some(dialog::credentials(std::path::Path::new(rdp_file))?)
    } else {
        None
    };
    if let Some(prompt) = prompt {
        let Some(chosen) = prompt else {
            return Ok(());
        };
        // One winit event loop per process: restart ourselves with the chosen settings. The full
        // dialog wrote its own `.rdp` file; the credentials prompt keeps the given one and arguments.
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command.arg("--rdp-file").arg(chosen.rdp_file).env("RDP_PASSWORD", chosen.password);
        if !chosen.gateway_password.is_empty() {
            command.env("RDP_GW_USERNAME", chosen.gateway_username).env("RDP_GW_PASSWORD", chosen.gateway_password);
        }
        if rdp_file_arg.is_some() {
            command.args(args.iter().skip(1).filter(|a| *a != "--rdp-file" && Some(*a) != rdp_file_arg.as_ref()));
            command.env("RDP_USERNAME", chosen.username);
            if !chosen.domain.is_empty() {
                command.arg("--domain").arg(chosen.domain);
            }
        }
        return Err(command.exec().into());
    }

    // Like mstsc, honour `redirectprinters` from the file; the upstream viewer wants the flag explicitly.
    if rdp_properties.as_ref().is_some_and(|ps| ps.redirect_printers() == Some(true))
        && !args.iter().any(|a| a == "--printer")
    {
        args.push("--printer".into());
    }
    let cli = ViewerConfig::parse_from(&args).context("CLI arguments parsing")?;
    setup_logging().context("unable to initialize logging")?;

    let dump_rdp = cli.dump_rdp().map(ToOwned::to_owned);
    let redirect_printers = cli.redirect_printers();
    let mut config = cli.into_config().context("configuration")?;

    // In RemoteApp mode the virtual desktop covers all local monitors (unless a size was given) and
    // the server gets the monitor layout, so windows maximize to one monitor instead of all of them.
    let x11_monitors = x11_monitors().unwrap_or_default();
    if config.connector().remote_application_mode
        && let Some((monitors, width, height)) = monitor_layout(&x11_monitors)
    {
        let mut args = args.clone();
        if config.properties().desktop_width().ok().flatten().is_none() {
            args.extend(["--desktop-width".into(), width.to_string()]);
            args.extend(["--desktop-height".into(), height.to_string()]);
        }
        config = ViewerConfig::parse_from(args)?
            .with_monitor_layout(ClientMonitorData { monitors })
            .into_config()
            .context("configuration")?;
    }

    if let Some(dump_path) = dump_rdp {
        std::fs::write(&dump_path, ironrdp_rdpfile::write(config.properties()))
            .with_context(|| format!("failed to write {}", dump_path.display()))?;
        return Ok(());
    }

    // RemoteApp needs windows at absolute screen positions, which only X11 (or XWayland) allows;
    // the desktop session is one window and runs natively on Wayland when available.
    let mut builder = EventLoop::<RdpOutputEvent>::with_user_event();
    if config.connector().remote_application_mode {
        builder.with_x11();
    }
    let event_loop = builder.build()?;

    let event_loop_proxy = event_loop.create_proxy();
    let (output_event_sender, mut output_event_receiver) = output_channel(64);
    let initial_window_size = PhysicalSize::new(
        u32::from(config.connector().desktop_size.width),
        u32::from(config.connector().desktop_size.height),
    );
    let remote_app = config.connector().remote_application_mode;
    let destination = config.destination().to_string();

    let rdpdr = rdpdr_factory(&config, redirect_printers);
    let mut client = RdpClient::new(config, output_event_sender);
    if remote_app {
        // RailApp blits per changed region; the upstream desktop App only paints full `Image` events.
        client = client.with_desktop_updates();
    }
    if let Some(factory) = rdpdr {
        client = client.with_rdpdr_backend_factory(Box::new(factory));
    }
    let input_event_sender = client.input_sender();
    let hyprland = remote_app.then(|| hyprland::Hyprland::detect(x11_monitors)).flatten();
    let mut rail_app = remote_app
        .then(|| rail_app::RailApp::new(&event_loop, input_event_sender.clone(), window_icon(), hyprland))
        .transpose()?;
    let mut desktop_app = (!remote_app)
        .then(|| {
            App::new(&event_loop, &input_event_sender, initial_window_size).map(|app| {
                let attributes = WindowAttributes::default()
                    .with_title(format!("{destination} - Remote Desktop Connection"))
                    .with_window_icon(window_icon());
                // WM_CLASS on X11, app_id on Wayland: both match `realrdp.desktop` for the icon.
                let attributes = WindowAttributesExtX11::with_name(attributes, WM_CLASS, WM_CLASS);
                app.with_window_attributes(WindowAttributesExtWayland::with_name(attributes, WM_CLASS, WM_CLASS))
            })
        })
        .transpose()
        .context("unable to initialize App")?;

    let rt = runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("unable to create tokio runtime")?;

    // Bridge the winit-agnostic output channel onto the GUI event loop.
    let output_forwarder = rt.spawn(async move {
        while let Some(event) = output_event_receiver.recv().await {
            if event_loop_proxy.send_event(event).is_err() {
                break;
            }
        }
    });

    std::thread::spawn(move || {
        rt.block_on(client.run());
        if let Err(error) = rt.block_on(output_forwarder) {
            tracing::warn!(%error, "RDP output forwarder failed");
        }
    });

    match (&mut rail_app, &mut desktop_app) {
        (Some(app), _) => event_loop.run_app(app)?,
        (_, Some(app)) => {
            event_loop.run_app(app)?;
            if let Some(error) = app.connection_error() {
                error_dialog(&format!("Connection failed:\n{error}"));
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Reports an error in a window: launched from a menu or a file manager, stderr is invisible.
pub fn error_dialog(text: &str) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Remote Desktop Connection")
        .set_description(text)
        .show();
}

/// The local monitors as Xwayland or the X server reports them through RandR.
fn x11_monitors() -> Option<Vec<hyprland::X11Monitor>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let found = conn.randr_get_monitors(root, true).ok()?.reply().ok()?.monitors;
    let monitors = found
        .iter()
        .map(|m| {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_default();
            hyprland::X11Monitor {
                name,
                x: i32::from(m.x),
                y: i32::from(m.y),
                width: u32::from(m.width),
                height: u32::from(m.height),
                primary: m.primary,
            }
        })
        .collect();
    Some(monitors)
}

/// Local monitors as [MS-RDPBCGR] 2.2.1.3.6 wants them (relative to the primary monitor's top-left
/// corner, at most 16), plus the size of their bounding box. winit only exposes monitors once the
/// event loop runs, hence RandR.
///
/// shortcut: assumes the primary monitor sits at the top-left of the bounding box, as RAIL window
/// offsets are then equal to local X11 positions; translate in `RailApp` once that is not the case.
fn monitor_layout(found: &[hyprland::X11Monitor]) -> Option<(Vec<Monitor>, u16, u16)> {
    // Xwayland under Hyprland (and other wlroots-style compositors) marks no monitor as primary,
    // and the server rejects a layout without exactly one PRIMARY flag (ERRINFO_BAD_MONITOR_DATA).
    // Fall back to the top-left monitor, which is where the offsets are measured from anyway.
    let primary = found
        .iter()
        .find(|m| m.primary)
        .or_else(|| found.iter().min_by_key(|m| (m.y, m.x)))?;
    let (px, py) = (primary.x, primary.y);
    let monitors: Vec<Monitor> = found
        .iter()
        .take(MONITOR_COUNT_MAX)
        .map(|m| Monitor {
            left: m.x - px,
            top: m.y - py,
            right: m.x - px + m.width as i32 - 1,
            bottom: m.y - py + m.height as i32 - 1,
            flags: if std::ptr::eq(m, primary) {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        })
        .collect();
    let width = monitors.iter().map(|m| m.right + 1).max()?;
    let height = monitors.iter().map(|m| m.bottom + 1).max()?;
    Some((monitors, u16::try_from(width).ok()?, u16::try_from(height).ok()?))
}

fn rdp_properties(path: &str) -> Option<ironrdp_propertyset::PropertySet> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut ps = ironrdp_propertyset::PropertySet::new();
    let _ = ironrdp_rdpfile::load(&mut ps, &text);
    Some(ps)
}

/// Drive and printer redirection: one local folder (`drivestoredirect`, written by the dialog) as a
/// drive, and the CUPS default printer. `ironrdp_rdpdr` must be on for the channel to be attached.
///
/// shortcut: one folder only, the IronRDP Linux backend resolves every drive against one base path;
/// upgrade when it keys paths by device ID.
fn rdpdr_factory(config: &ironrdp::client::config::Config, printers: bool) -> Option<NixRdpdrBackendFactory> {
    let folder = config
        .properties()
        .get::<&str>("drivestoredirect")
        .map(|f| f.trim_end_matches('/').to_owned())
        .filter(|f| std::path::Path::new(f).is_dir());
    if folder.is_none() && !printers {
        return None;
    }
    if !config.properties().enable_rdpdr().unwrap_or(false) {
        tracing::warn!("drive redirection needs ironrdp_rdpdr:i:1 in the .rdp file");
    }
    let mut factory = NixRdpdrBackendFactory::new(folder.clone().unwrap_or_default());
    if let Some(folder) = folder {
        let name = std::path::Path::new(&folder)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "share".to_owned());
        factory = factory.with_drive(1, name);
    }
    if printers {
        factory = factory.with_printer("Printer".to_owned(), PrintTarget::DefaultPrinter);
    }
    Some(factory)
}

fn setup_logging() -> anyhow::Result<()> {
    use tracing::metadata::LevelFilter;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::prelude::*;

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .with_env_var("IRONRDP_LOG")
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .try_init()
        .context("failed to set tracing global subscriber")
}
