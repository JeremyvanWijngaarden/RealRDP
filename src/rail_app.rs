//! RemoteApp (RAIL) window manager: one local X11 window per remote window.
//!
//! The server renders every remote window into the shared desktop framebuffer; each local window
//! blits its own rectangle out of that framebuffer. Popups (menus, dropdowns, tooltips) become
//! override-redirect windows so the X11 window manager leaves their position alone.
//!
//! shortcut: local move/size is not advertised (no `ALLOW_LOCAL_MOVE_SIZE`), so the server moves
//! windows itself and we only follow its offsets; visibility rects, icons and z-order are ignored.
//! Upgrade once `ironrdp_client::rail::RailInputEvent` exposes `WindowMove`.

use core::num::NonZeroU32;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use ironrdp::client::rail::RailInputEvent;
use ironrdp::client::rdp::{AutoReconnectDecision, RdpInputEvent, RdpInputSender, RdpOutputEvent};
use ironrdp::input::{MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_rail::pdu::{ActivatePdu, SystemCommand, SystemCommandPdu};
use raw_window_handle::{DisplayHandle, HasDisplayHandle as _};

use crate::hyprland::Hyprland;

/// How long a new window keeps the title its Hyprland window rule matches on. Hyprland records
/// the initial title when the surface first commits, which happens after our first draw, and
/// a title change sent right after creation would overtake it.
const TITLE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
use tracing::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::platform::x11::{WindowAttributesExtX11 as _, WindowType};
use winit::window::{CursorIcon, CustomCursor, Icon, Window, WindowAttributes, WindowId};

use crate::keymap::{is_modifier, map_key_code};
use crate::window_order::{self, WindowInfo, WindowOrder};

const WS_POPUP: u32 = 0x8000_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const SW_HIDE: u8 = 0;
const SW_MINIMIZED: u8 = 2;

type Surface = softbuffer::Surface<DisplayHandle<'static>, Arc<Window>>;

struct RemoteWindow {
    info: WindowInfo,
    local: Option<(Arc<Window>, Surface)>,
    /// Under Hyprland: the title the window was mapped with, which identifies it in Hyprland's
    /// window rules and dispatchers, and the monitor Hyprland last assigned to it.
    initial_title: Option<String>,
    monitor: Option<String>,
    /// When the real title replaces `initial_title` (see [`TITLE_DELAY`]).
    title_due: Option<std::time::Instant>,
}

impl RemoteWindow {
    /// Menus, tooltips and dropdowns: anything without a caption that is a popup, an owned window, a
    /// tool window, or a child of the desktop (ComboLBox dropdowns are WS_CHILD with owner 0).
    fn is_popup(&self) -> bool {
        let (style, ex_style) = self.info.style.unwrap_or_default();
        style & WS_CAPTION != WS_CAPTION
            && (style & (WS_POPUP | WS_CHILD) != 0
                || ex_style & WS_EX_TOOLWINDOW != 0
                || self.info.owner.is_some_and(|o| o != 0))
    }
}

pub struct RailApp {
    input: RdpInputSender,
    context: softbuffer::Context<DisplayHandle<'static>>,
    buffer: Vec<u32>,
    buffer_size: (u16, u16),
    windows: HashMap<u32, RemoteWindow>,
    by_local: HashMap<WindowId, u32>,
    had_windows: bool,
    /// Window that lost focus in this event batch; dropped if focus moved to another window of ours.
    pending_deactivate: Option<u32>,
    cursor: Option<CustomCursor>,
    icon: Option<Icon>,
    cursor_visible: bool,
    input_database: ironrdp::input::Database,
    hyprland: Option<Hyprland>,
}

impl RailApp {
    pub fn new(
        event_loop: &EventLoop<RdpOutputEvent>,
        input: RdpInputSender,
        icon: Option<Icon>,
        hyprland: Option<Hyprland>,
    ) -> anyhow::Result<Self> {
        if let Some(hyprland) = &hyprland {
            hyprland.install_rules();
        }
        // SAFETY: the context lives inside the app, which is dropped before the event loop ends.
        // Same approach as the upstream ironrdp-viewer.
        let display_handle = unsafe {
            core::mem::transmute::<DisplayHandle<'_>, DisplayHandle<'static>>(
                event_loop.display_handle().context("get display handle")?,
            )
        };
        let context =
            softbuffer::Context::new(display_handle).map_err(|e| anyhow::anyhow!("softbuffer context: {e}"))?;
        Ok(Self {
            input,
            context,
            buffer: Vec::new(),
            buffer_size: (0, 0),
            windows: HashMap::new(),
            by_local: HashMap::new(),
            had_windows: false,
            pending_deactivate: None,
            cursor: None,
            icon,
            cursor_visible: true,
            input_database: ironrdp::input::Database::new(),
            hyprland,
        })
    }

    fn send(&mut self, operations: impl IntoIterator<Item = Operation>) {
        let Ok(permit) = self.input.try_reserve() else {
            return;
        };
        let events = self.input_database.apply(operations);
        if !events.is_empty() {
            permit.send(RdpInputEvent::FastPath(events));
        }
    }

    fn send_rail(&self, event: RailInputEvent) {
        debug!(?event, "RAIL input");
        if let Err(error) = self.input.try_send_rail_input(event) {
            warn!(%error, "dropped RAIL input event");
        }
    }

    fn apply_orders(&mut self, event_loop: &ActiveEventLoop, update: &[u8]) {
        let Some(orders) = window_order::decode(update) else {
            warn!("undecodable windowing orders update");
            return;
        };
        for order in orders {
            debug!(?order, "window order");
            match order {
                WindowOrder::Window(info) => self.update_window(event_loop, info),
                WindowOrder::WindowDeleted(id) => {
                    if let Some(w) = self.windows.remove(&id) {
                        if let Some((win, _)) = w.local {
                            self.by_local.remove(&win.id());
                        }
                    }
                }
                WindowOrder::Desktop { non_monitored, active_window, .. } => {
                    if non_monitored {
                        self.windows.clear();
                        self.by_local.clear();
                    }
                    if let Some((win, _)) = active_window.and_then(|id| self.windows.get(&id)?.local.as_ref()) {
                        win.focus_window();
                    }
                }
                WindowOrder::Ignored => {}
            }
        }
        if self.had_windows && self.windows.is_empty() {
            info!("last RemoteApp window closed");
            self.input.request_graceful_close();
        }
    }

    fn update_window(&mut self, event_loop: &ActiveEventLoop, info: WindowInfo) {
        let id = info.id;
        let w = self.windows.entry(id).or_insert_with(|| RemoteWindow {
            info: WindowInfo { id: info.id, ..Default::default() },
            local: None,
            initial_title: None,
            monitor: None,
            title_due: None,
        });
        let i = &mut w.info;
        let (old_offset, old_size) = (i.offset, i.size);
        i.owner = info.owner.or(i.owner);
        i.style = info.style.or(i.style);
        i.show = info.show.or(i.show);
        i.offset = info.offset.or(i.offset);
        i.size = info.size.or(i.size);
        if info.title.is_some() {
            i.title = info.title;
        }

        let (Some((x, y)), Some((width, height))) = (i.offset, i.size) else {
            return;
        };
        let (Some(width), Some(height)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        let show = i.show.unwrap_or(SW_HIDE);
        let visible = show != SW_HIDE;
        let title = i.title.clone().unwrap_or_default();
        let position = PhysicalPosition::new(x, y);
        let size = PhysicalSize::new(width.get(), height.get());

        match &w.local {
            Some((win, _)) => {
                // Only real changes become X11 configure requests: Hyprland converts every request
                // relative to the window's monitor, so a redundant one can move a window off-screen.
                let moved = old_offset != Some((x, y));
                if moved {
                    win.set_outer_position(position);
                }
                if old_size != Some((width.get(), height.get())) {
                    let _ = win.request_inner_size(size);
                }
                if w.title_due.is_none() {
                    win.set_title(&title);
                }
                win.set_visible(visible);
                win.set_minimized(show == SW_MINIMIZED);
                // The framebuffer rectangle behind the window changed, even if its size did not.
                win.request_redraw();
                if moved
                    && let Some(hyprland) = &self.hyprland
                    && let Some(initial_title) = &w.initial_title
                    && let Some(monitor) = hyprland.monitor_at(x, y)
                    && w.monitor.as_deref() != Some(monitor)
                {
                    hyprland.move_to_monitor(initial_title, monitor);
                    w.monitor = Some(monitor.to_owned());
                }
            }
            None => {
                if !visible {
                    return;
                }
                let popup = w.is_popup();
                // Under Hyprland the window maps with a title that its per-monitor window rule
                // matches; the real title follows once Hyprland has recorded it (`TITLE_DELAY`).
                let monitor = self
                    .hyprland
                    .as_ref()
                    .and_then(|hyprland| hyprland.monitor_at(x, y))
                    .map(str::to_owned);
                let initial_title = match (&self.hyprland, &monitor) {
                    (Some(hyprland), Some(monitor)) => Some(hyprland.initial_title(monitor, id)),
                    _ => None,
                };
                let attributes = WindowAttributes::default()
                    .with_title(initial_title.as_deref().unwrap_or(&title))
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_position(position)
                    .with_inner_size(size)
                    .with_override_redirect(popup)
                    .with_window_icon(self.icon.clone())
                    .with_name(crate::WM_CLASS, crate::WM_CLASS)
                    .with_x11_window_type(vec![if popup { WindowType::PopupMenu } else { WindowType::Normal }]);
                let win = match event_loop.create_window(attributes) {
                    Ok(win) => Arc::new(win),
                    Err(error) => {
                        error!(%error, "failed to create window");
                        return;
                    }
                };
                if initial_title.is_some() {
                    w.title_due = Some(std::time::Instant::now() + TITLE_DELAY);
                }
                w.initial_title = initial_title;
                w.monitor = monitor;
                debug!(id, popup, ?position, ?size, %title, "created RemoteApp window");
                let mut surface = softbuffer::Surface::new(&self.context, Arc::clone(&win)).expect("surface");
                surface.resize(width, height).expect("surface resize");
                if let Some(cursor) = &self.cursor {
                    win.set_cursor(cursor.clone());
                }
                win.set_cursor_visible(self.cursor_visible);
                // The server usually paints a popup before announcing it, so no later desktop
                // update may touch this area: draw what is already in the framebuffer.
                win.request_redraw();
                self.by_local.insert(win.id(), id);
                w.local = Some((win, surface));
                self.had_windows = true;
            }
        }
    }

    fn draw(&mut self, id: u32) {
        let (bw, bh) = (usize::from(self.buffer_size.0), usize::from(self.buffer_size.1));
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let (Some((win, surface)), Some((ox, oy))) = (w.local.as_mut(), w.info.offset) else {
            return;
        };
        let size = win.inner_size();
        let (Some(width), Some(height)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        surface.resize(width, height).expect("surface resize");
        let mut out = surface.buffer_mut().expect("surface buffer");
        let (width, height) = (width.get() as usize, height.get() as usize);
        // Columns [start, end) of each local row lie inside the framebuffer; the rest is black.
        let start = (-(ox as isize)).clamp(0, width as isize) as usize;
        let end = (bw as isize - ox as isize).clamp(0, width as isize) as usize;
        for row in 0..height {
            let dst = &mut out[row * width..(row + 1) * width];
            let sy = oy as isize + row as isize;
            if sy < 0 || sy as usize >= bh || start >= end {
                dst.fill(0);
                continue;
            }
            let src_row = &self.buffer[sy as usize * bw..][..bw];
            dst[..start].fill(0);
            dst[end..].fill(0);
            dst[start..end].copy_from_slice(&src_row[(ox as isize + start as isize) as usize..][..end - start]);
        }
        out.present().expect("present");
    }

    fn for_each_local(&self, f: impl Fn(&Window)) {
        self.windows.values().filter_map(|w| w.local.as_ref()).for_each(|(win, _)| f(win));
    }

    fn handle_output(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        match event {
            RdpOutputEvent::Connected => info!("RDP session connected"),
            RdpOutputEvent::LoginComplete => info!("RDP login complete"),
            RdpOutputEvent::DesktopUpdate(update) => {
                let (pixels, width, height, r) = update.into_parts();
                let (bw, bh) = (usize::from(width.get()), usize::from(height.get()));
                if self.buffer_size != (width.get(), height.get()) {
                    self.buffer_size = (width.get(), height.get());
                    self.buffer = vec![0; bw * bh];
                }
                let rw = usize::from(r.right - r.left) + 1;
                for (i, row) in pixels.chunks_exact(rw).enumerate() {
                    self.buffer[(usize::from(r.top) + i) * bw + usize::from(r.left)..][..rw].copy_from_slice(row);
                }
                for w in self.windows.values() {
                    let (Some((win, _)), Some((ox, oy))) = (&w.local, w.info.offset) else {
                        continue;
                    };
                    let s = win.inner_size();
                    if ox <= i32::from(r.right)
                        && oy <= i32::from(r.bottom)
                        && ox.saturating_add(s.width as i32) > i32::from(r.left)
                        && oy.saturating_add(s.height as i32) > i32::from(r.top)
                    {
                        win.request_redraw();
                    }
                }
            }
            RdpOutputEvent::WindowingOrders(update) => self.apply_orders(event_loop, &update),
            RdpOutputEvent::ConnectionFailure(error) => {
                eprintln!("Connection error: {}", error.report().with_locations());
                crate::error_dialog(&format!("Connection failed:\n{}", error.report()));
                event_loop.exit();
            }
            RdpOutputEvent::Terminated(result) => {
                match result {
                    Ok(reason) => println!("Terminated gracefully: {reason}"),
                    Err(error) => eprintln!("Active session error: {}", error.report().with_locations()),
                }
                event_loop.exit();
            }
            RdpOutputEvent::PointerHidden => {
                self.cursor_visible = false;
                self.for_each_local(|win| win.set_cursor_visible(false));
            }
            RdpOutputEvent::PointerDefault => {
                self.cursor = None;
                self.cursor_visible = true;
                self.for_each_local(|win| {
                    win.set_cursor(CursorIcon::default());
                    win.set_cursor_visible(true);
                });
            }
            RdpOutputEvent::PointerBitmap(pointer) => {
                match CustomCursor::from_rgba(
                    pointer.bitmap_data.clone(),
                    pointer.width,
                    pointer.height,
                    pointer.hotspot_x,
                    pointer.hotspot_y,
                ) {
                    Ok(cursor) => {
                        let cursor = event_loop.create_custom_cursor(cursor);
                        self.for_each_local(|win| {
                            win.set_cursor(cursor.clone());
                            win.set_cursor_visible(true);
                        });
                        self.cursor = Some(cursor);
                        self.cursor_visible = true;
                    }
                    Err(error) => error!(?error, "failed to set cursor bitmap"),
                }
            }
            RdpOutputEvent::AutoReconnecting { response, .. } => {
                let _ = response.send(AutoReconnectDecision::Stop);
            }
            RdpOutputEvent::RailExecuteResult(result) => info!(?result, "RemoteApp launch result"),
            RdpOutputEvent::RailExecuteFailed { executable, reason, .. } => {
                eprintln!("RemoteApp launch of {executable} failed: {reason:?}");
                crate::error_dialog(&format!("RemoteApp launch of {executable} failed: {reason:?}"));
                event_loop.exit();
            }
            other => debug!(?other, "ignored output event"),
        }
    }

    fn handle_window_event(&mut self, _event_loop: &ActiveEventLoop, local_id: WindowId, event: WindowEvent) {
        let Some(&id) = self.by_local.get(&local_id) else {
            return;
        };
        let offset = self.windows.get(&id).and_then(|w| w.info.offset).unwrap_or_default();
        match event {
            WindowEvent::RedrawRequested => self.draw(id),
            WindowEvent::CloseRequested => {
                self.send_rail(RailInputEvent::SystemCommand(SystemCommandPdu {
                    window_id: id,
                    command: SystemCommand::Close,
                }));
            }
            WindowEvent::Focused(enabled) => {
                debug!(id, enabled, "local focus change");
                // Menus and dropdowns are never activated on Windows; telling the server so closes them.
                if self.windows.get(&id).is_some_and(RemoteWindow::is_popup) {
                    return;
                }
                if enabled {
                    self.pending_deactivate = None;
                    self.send_rail(RailInputEvent::Activate(ActivatePdu { window_id: id, enabled: true }));
                } else {
                    self.pending_deactivate = Some(id);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let x = (offset.0 as f64 + position.x).clamp(0.0, f64::from(u16::MAX)) as u16;
                let y = (offset.1 as f64 + position.y).clamp(0.0, f64::from(u16::MAX)) as u16;
                self.send([Operation::MouseMove(MousePosition { x, y })]);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::X1,
                    winit::event::MouseButton::Forward => MouseButton::X2,
                    winit::event::MouseButton::Other(native) => match MouseButton::from_native_button(native) {
                        Some(button) => button,
                        None => return,
                    },
                };
                self.send([match state {
                    ElementState::Pressed => Operation::MouseButtonPressed(button),
                    ElementState::Released => Operation::MouseButtonReleased(button),
                }]);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (f64::from(x) * 100.0, f64::from(y) * 100.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                };
                let mut ops = Vec::new();
                for (is_vertical, units) in [(false, dx), (true, dy)] {
                    if units.abs() > 0.001 {
                        ops.push(Operation::WheelRotations(WheelRotations {
                            is_vertical,
                            rotation_units: units as i16,
                        }));
                    }
                }
                self.send(ops);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(key_code) = event.physical_key else {
                    return;
                };
                if is_modifier(key_code) {
                    return;
                }
                let Some((scancode, release_only)) = map_key_code(key_code) else {
                    return;
                };
                let ops: Vec<Operation> = match event.state {
                    ElementState::Pressed => vec![Operation::KeyPressed(scancode)],
                    ElementState::Released if release_only => {
                        vec![Operation::KeyPressed(scancode), Operation::KeyReleased(scancode)]
                    }
                    ElementState::Released => vec![Operation::KeyReleased(scancode)],
                };
                self.send(ops);
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                let keys = [
                    (state.shift_key(), Scancode::from_u8(false, 0x2A)),
                    (state.control_key(), Scancode::from_u8(false, 0x1D)),
                    (state.alt_key(), Scancode::from_u8(false, 0x38)),
                    (state.super_key(), Scancode::from_u8(true, 0x5B)),
                ];
                self.send(keys.into_iter().map(|(pressed, sc)| {
                    if pressed { Operation::KeyPressed(sc) } else { Operation::KeyReleased(sc) }
                }));
            }
            _ => {}
        }
    }
}

impl ApplicationHandler<RdpOutputEvent> for RailApp {
    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        self.handle_window_event(event_loop, window_id, event);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        self.handle_output(event_loop, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // X11 delivers FocusOut and the sibling's FocusIn in one batch, so a switch between two of
        // our own windows never reaches here as a deactivation.
        if let Some(id) = self.pending_deactivate.take() {
            self.send_rail(RailInputEvent::Activate(ActivatePdu { window_id: id, enabled: false }));
        }
        // Give new windows their real title once Hyprland has seen the initial one.
        let now = std::time::Instant::now();
        let mut next_due: Option<std::time::Instant> = None;
        for w in self.windows.values_mut() {
            let Some(due) = w.title_due else {
                continue;
            };
            if due <= now {
                if let Some((win, _)) = &w.local {
                    win.set_title(w.info.title.as_deref().unwrap_or_default());
                }
                w.title_due = None;
            } else {
                next_due = Some(next_due.map_or(due, |n| n.min(due)));
            }
        }
        event_loop.set_control_flow(match next_due {
            Some(due) => ControlFlow::WaitUntil(due),
            None => ControlFlow::Wait,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(style: u32, ex_style: u32, owner: u32) -> RemoteWindow {
        RemoteWindow {
            info: WindowInfo { style: Some((style, ex_style)), owner: Some(owner), ..Default::default() },
            local: None,
            initial_title: None,
            monitor: None,
            title_due: None,
        }
    }

    #[test]
    fn classifies_windows_seen_on_the_wire() {
        // ComboLBox dropdown: WS_CHILD | WS_VISIBLE | WS_BORDER | WS_VSCROLL, WS_EX_TOOLWINDOW | TOPMOST.
        assert!(window(0x54A0_8041, 0x88, 0).is_popup());
        // Main window with a caption, and a captioned splash with a dialog frame.
        assert!(!window(0x17CF_0000, 0x0005_0100, 0).is_popup());
        assert!(!window(0x16C8_0000, 0x100, 0).is_popup());
        // Owned popup menu.
        assert!(window(WS_POPUP, 0, 459174).is_popup());
    }
}
