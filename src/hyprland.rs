//! Hyprland support for the RemoteApp windows.
//!
//! The session windows are X11 windows (Xwayland), positioned by the X11 coordinates the server
//! dictates. Hyprland converts those coordinates relative to the monitor it assigned the window to,
//! which is the focused monitor at map time, not the monitor the coordinates lie on (Hyprland
//! 0.56, `CWindow::xwaylandPositionToReal`). A window meant for another monitor therefore lands
//! off-screen. Two things fix that over Hyprland's IPC socket:
//!
//! - a window rule per monitor, matching an initial title of the form `[<monitor> <id>]`, so a
//!   new window is assigned the monitor its position lies on before it is placed;
//! - a `move` dispatch to the new monitor when the server later moves a window across monitors.
//!
//! The rules live in Hyprland's Lua state for the session only and are installed once (guarded by
//! a Lua global), together with a cosmetic rule that keeps borders, rounding, shadows, blur,
//! animations and transparency off the session windows.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tracing::{debug, warn};

/// An X11 (RandR) monitor as Xwayland reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X11Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub struct Hyprland {
    socket: PathBuf,
    monitors: Vec<X11Monitor>,
}

impl Hyprland {
    /// `Some` when running inside a Hyprland session with a reachable IPC socket.
    pub fn detect(monitors: Vec<X11Monitor>) -> Option<Self> {
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        let socket = PathBuf::from(runtime_dir)
            .join("hypr")
            .join(signature)
            .join(".socket.sock");
        if !socket.exists() {
            return None;
        }
        let this = Self { socket, monitors };
        // Probe the socket: not every compositor exporting the signature speaks this protocol.
        match this.request("repl return 1") {
            Ok(reply) if reply.trim() == "1" => Some(this),
            Ok(reply) => {
                warn!(%reply, "unexpected reply from the Hyprland IPC socket");
                None
            }
            Err(error) => {
                warn!(%error, "Hyprland IPC socket unreachable");
                None
            }
        }
    }

    /// One request per connection, as Hyprland requires.
    fn request(&self, request: &str) -> std::io::Result<String> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(request.as_bytes())?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        Ok(reply)
    }

    fn eval(&self, lua: &str) {
        match self.request(&format!("eval {lua}")) {
            Ok(reply) if reply.trim() == "ok" => {}
            Ok(reply) => warn!(%reply, %lua, "Hyprland rejected Lua"),
            Err(error) => warn!(%error, "Hyprland IPC request failed"),
        }
    }

    /// Installs the window rules for this session. Named rules are updated in place when they
    /// already exist, so a newer build replaces the rules an older one left in the Lua state.
    pub fn install_rules(&self) {
        let class = regex_literal(crate::WM_CLASS);
        let mut lua = format!(
            "hl.window_rule({{ name = \"realrdp\", match = {{ class = \"^{class}$\" }}, \
             border_size = 0, rounding = 0, no_anim = true, no_shadow = true, no_blur = true, opacity = \"1.0 override\", \
             persistent_size = false }}) "
        );
        for monitor in &self.monitors {
            let name = lua_string(&monitor.name);
            let token = regex_literal(&token_prefix(&monitor.name));
            // Hyprland matches rule regexes against the whole string.
            let window = lua_string(&format!("^{token}[0-9]+\\]$"));
            let popup = lua_string(&format!("^{token}[0-9]+ p\\]$"));
            // Popups map silently: a non-silent monitor rule focuses that monitor, which takes
            // focus from the window that owns the menu, and Windows then closes the menu.
            let silent = lua_string(&format!("{} silent", monitor.name));
            lua.push_str(&format!(
                "hl.window_rule({{ name = \"realrdp-\" .. {name}, \
                 match = {{ class = \"^{class}$\", initial_title = {window} }}, monitor = {name} }}) \
                 hl.window_rule({{ name = \"realrdp-popup-\" .. {name}, \
                 match = {{ class = \"^{class}$\", initial_title = {popup} }}, monitor = {silent} }}) "
            ));
        }
        debug!("installing Hyprland window rules");
        self.eval(&lua);
    }

    /// The monitor an X11 position lies on, or the nearest one.
    pub fn monitor_at(&self, x: i32, y: i32) -> Option<&str> {
        let distance = |m: &X11Monitor| {
            let dx = (m.x - x).max(0).max(x - (m.x + m.width as i32 - 1));
            let dy = (m.y - y).max(0).max(y - (m.y + m.height as i32 - 1));
            i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
        };
        self.monitors
            .iter()
            .min_by_key(|m| distance(m))
            .map(|m| m.name.as_str())
    }

    /// The initial title that makes the per-monitor rule assign `monitor` to a new window.
    pub fn initial_title(&self, monitor: &str, id: u32, popup: bool) -> String {
        let suffix = if popup { " p" } else { "" };
        format!("{}{id}{suffix}]", token_prefix(monitor))
    }

    /// Moves the window created with `initial_title` to `monitor`; Hyprland re-derives its
    /// position from the X11 geometry relative to that monitor.
    pub fn move_to_monitor(&self, initial_title: &str, monitor: &str) {
        let selector = lua_string(&format!("initialtitle:^{}$", regex_literal(initial_title)));
        let monitor = lua_string(monitor);
        debug!(initial_title, %monitor, "moving window to monitor");
        self.eval(&format!(
            "hl.dispatch(hl.dsp.window.move({{ window = {selector}, monitor = {monitor}, follow = false }}))"
        ));
    }
}

fn token_prefix(monitor: &str) -> String {
    format!("[{monitor} ")
}

/// Escapes `text` for literal use inside an RE2 regular expression.
fn regex_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if !c.is_alphanumeric() && c != ' ' && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A Lua string literal.
fn lua_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hyprland() -> Hyprland {
        Hyprland {
            socket: PathBuf::new(),
            monitors: vec![
                X11Monitor {
                    name: "DVI-I-1".into(),
                    x: 0,
                    y: 0,
                    width: 3440,
                    height: 1440,
                    primary: false,
                },
                X11Monitor {
                    name: "DVI-I-2".into(),
                    x: 3440,
                    y: 0,
                    width: 3440,
                    height: 1440,
                    primary: false,
                },
            ],
        }
    }

    #[test]
    fn monitor_at_contains_or_nearest() {
        let h = hyprland();
        assert_eq!(h.monitor_at(85, 78), Some("DVI-I-1"));
        assert_eq!(h.monitor_at(3440, 0), Some("DVI-I-2"));
        assert_eq!(h.monitor_at(-50, 2000), Some("DVI-I-1"));
        assert_eq!(h.monitor_at(9000, 10), Some("DVI-I-2"));
    }

    #[test]
    fn initial_title_matches_rule_prefix() {
        let h = hyprland();
        assert_eq!(h.initial_title("DVI-I-1", 65932, false), "[DVI-I-1 65932]");
        assert_eq!(h.initial_title("DVI-I-1", 65932, true), "[DVI-I-1 65932 p]");
        assert_eq!(regex_literal("[DVI-I-1 65932]"), "\\[DVI\\-I\\-1 65932\\]");
        assert_eq!(lua_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
