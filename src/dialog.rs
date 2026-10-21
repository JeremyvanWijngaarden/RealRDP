//! mstsc-style connection dialog. Shown when the program is started without arguments; the
//! credentials-only variant is shown when a `.rdp` file is given but no password.
//!
//! On "Connect" the settings are written to `Default.rdp` (like mstsc, without the password) and the
//! process re-executes itself with `--rdp-file`, because winit allows only one event loop per process.

use std::path::PathBuf;

use anyhow::Context as _;
use eframe::egui;
use ironrdp_cfg::{AudioCaptureMode, AudioMode, GatewayCredentialsSource, GatewayUsageMethod, PropertySetExt as _, TargetAddr};
use ironrdp_propertyset::PropertySet;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    General,
    Display,
    LocalResources,
    Experience,
    Advanced,
}

const TABS: [(Tab, &str); 5] = [
    (Tab::General, "General"),
    (Tab::Display, "Display"),
    (Tab::LocalResources, "Local Resources"),
    (Tab::Experience, "Experience"),
    (Tab::Advanced, "Advanced"),
];

/// Desktop sizes offered on the Display tab; `None` means the local screen size (full screen).
const SIZES: [Option<(u16, u16)>; 8] = [
    Some((800, 600)),
    Some((1024, 768)),
    Some((1280, 720)),
    Some((1366, 768)),
    Some((1600, 900)),
    Some((1920, 1080)),
    Some((2560, 1440)),
    None,
];

struct Dialog {
    tab: Tab,
    show_options: bool,
    computer: String,
    username: String,
    domain: String,
    password: String,
    /// "Allow me to save credentials": keep the passwords in the system keyring.
    save_credentials: bool,
    size: Option<(u16, u16)>,
    clipboard: bool,
    /// Local folder redirected as a drive; empty for none.
    drives: String,
    printers: bool,
    smartcard: bool,
    microphone: bool,
    audio: AudioMode,
    gateway: String,
    gateway_usage: GatewayUsageMethod,
    /// "Use my RD Gateway credentials for the remote computer".
    gateway_same_credentials: bool,
    gateway_username: String,
    gateway_password: String,
    remote_app: bool,
    program: String,
    error: Option<String>,
    /// Only ask for credentials for this `.rdp` file instead of the full dialog.
    credentials_for: Option<PathBuf>,
    /// Set when the user pressed Connect; read back after the dialog closes.
    connect: Option<PathBuf>,
}

/// What the user chose: the `.rdp` file to connect with and the credentials to use.
pub struct Outcome {
    pub rdp_file: PathBuf,
    pub username: String,
    pub domain: String,
    pub password: String,
    pub gateway_username: String,
    pub gateway_password: String,
}

impl Default for Dialog {
    fn default() -> Self {
        Self {
            tab: Tab::General,
            show_options: true,
            computer: String::new(),
            username: String::new(),
            domain: String::new(),
            password: String::new(),
            save_credentials: false,
            size: None,
            clipboard: true,
            drives: String::new(),
            printers: false,
            smartcard: false,
            microphone: false,
            audio: AudioMode::RedirectToClient,
            gateway: String::new(),
            gateway_usage: GatewayUsageMethod::Direct,
            gateway_same_credentials: true,
            gateway_username: String::new(),
            gateway_password: String::new(),
            remote_app: false,
            program: String::new(),
            error: None,
            credentials_for: None,
            connect: None,
        }
    }
}

impl Dialog {
    fn load(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut ps = PropertySet::new();
        let _ = ironrdp_rdpfile::load(&mut ps, &text);
        self.computer = ps.full_address().ok().flatten().map(|a| a.to_string()).unwrap_or_default();
        self.username = ps.username().unwrap_or_default().to_owned();
        self.domain = ps.domain().unwrap_or_default().to_owned();
        if let Some(password) = saved_password(&self.computer, &self.username) {
            self.password = password;
            self.save_credentials = true;
        }
        self.size = ps.desktop_width().ok().flatten().zip(ps.desktop_height().ok().flatten());
        self.clipboard = ps.redirect_clipboard().unwrap_or(true);
        self.drives = ps.get::<&str>("drivestoredirect").unwrap_or_default().to_owned();
        self.printers = ps.redirect_printers().unwrap_or(false);
        self.smartcard = ps.enable_smartcard().unwrap_or(false);
        self.microphone = ps.audio_capture_mode().ok().flatten() == Some(AudioCaptureMode::CaptureFromClient);
        self.audio = ps.audio_mode().ok().flatten().unwrap_or(AudioMode::RedirectToClient);
        self.gateway = ps.gateway_hostname().unwrap_or_default().to_owned();
        self.gateway_usage = ps.gateway_usage_method().ok().flatten().unwrap_or(GatewayUsageMethod::Direct);
        self.gateway_same_credentials = ps.gateway_credentials_source().ok().flatten()
            != Some(GatewayCredentialsSource::UseUserCredentials);
        self.gateway_username = ps.gateway_username().unwrap_or_default().to_owned();
        if let Some(password) = saved_password(&self.gateway, &self.gateway_username) {
            self.gateway_password = password;
        }
        self.remote_app = ps.remote_application_mode().unwrap_or(false);
        self.program = ps.remote_application_program().unwrap_or_default().to_owned();
        Ok(())
    }

    fn properties(&self) -> anyhow::Result<PropertySet> {
        let addr: TargetAddr = self.computer.trim().parse().map_err(|e| anyhow::anyhow!("Computer: {e}"))?;
        let mut ps = PropertySet::new();
        ps.set_full_address(&addr);
        if !self.username.is_empty() {
            ps.set_username(self.username.clone());
        }
        if !self.domain.is_empty() {
            ps.set_domain(self.domain.clone());
        }
        if let Some((w, h)) = self.size {
            ps.set_desktop_width(w);
            ps.set_desktop_height(h);
        }
        ps.set_redirect_clipboard(self.clipboard);
        ps.set_redirect_printers(self.printers);
        if !self.drives.trim().is_empty() {
            // mstsc's key, with a local folder as value; `ironrdp_rdpdr` opens the channel for it.
            ps.insert("drivestoredirect", self.drives.trim().to_owned());
            ps.set_enable_rdpdr(true);
        }
        ps.set_enable_smartcard(self.smartcard);
        ps.set_audio_capture_mode(if self.microphone {
            AudioCaptureMode::CaptureFromClient
        } else {
            AudioCaptureMode::Disabled
        });
        ps.set_audio_mode(self.audio);
        if !self.gateway.trim().is_empty() {
            ps.set_gateway_hostname(self.gateway.trim());
            ps.set_gateway_usage_method(self.gateway_usage);
            ps.set_gateway_credentials_source(if self.gateway_same_credentials {
                GatewayCredentialsSource::UseServerCredentials
            } else {
                GatewayCredentialsSource::UseUserCredentials
            });
            if !self.gateway_username.is_empty() {
                ps.set_gateway_username(self.gateway_username.clone());
            }
        }
        if self.remote_app {
            ps.set_remote_application_mode(true);
            ps.set_remote_application_program(self.program.clone());
        }
        Ok(ps)
    }

    fn write_rdp(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, ironrdp_rdpfile::write(&self.properties()?))
            .with_context(|| format!("write {}", path.display()))
    }

    /// Saves or forgets the passwords in the system keyring, as the checkbox says.
    fn store_credentials(&self) -> anyhow::Result<()> {
        for (host, user, password) in [
            (&self.computer, &self.username, &self.password),
            (&self.gateway, &self.gateway_username, &self.gateway_password),
        ] {
            if host.trim().is_empty() || user.is_empty() {
                continue;
            }
            let entry = keyring::Entry::new(&format!("TERMSRV/{}", host.trim()), user)?;
            if self.save_credentials && !password.is_empty() {
                entry.set_password(password)?;
            } else if !self.save_credentials {
                // Nothing stored is not an error.
                let _ = entry.delete_credential();
            }
        }
        Ok(())
    }

    fn try_or_show(&mut self, result: anyhow::Result<()>) {
        self.error = result.err().map(|e| format!("{e:#}"));
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.ctx().set_visuals(egui::Visuals::light());
        if let Some(rdp_file) = self.credentials_for.clone() {
            egui::CentralPanel::default().show(ui, |ui| {
                self.header(ui);
                ui.add_space(8.0);
                ui.label(format!("Enter your credentials for {}.", self.computer));
                ui.add_space(8.0);
                self.logon_fields(ui);
                if self.asks_gateway_credentials() {
                    ui.add_space(8.0);
                    ui.label(format!("RD Gateway {} asks for its own credentials.", self.gateway));
                    self.gateway_logon_fields(ui);
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        if let Err(e) = self.store_credentials() {
                            tracing::warn!(%e, "could not save credentials");
                        }
                        self.connect = Some(rdp_file);
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Cancel").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
            return;
        }
        egui::Panel::bottom("buttons").show(ui, |ui| {
            ui.add_space(6.0);
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::DARK_RED, error);
            }
            ui.horizontal(|ui| {
                let label = if self.show_options { "Hide Options" } else { "Show Options" };
                if ui.button(label).clicked() {
                    self.show_options = !self.show_options;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Help").clicked() {
                        self.error = Some("Fill in the computer name and press Connect.".into());
                    }
                    if ui.button("Connect").clicked() {
                        let path = default_rdp_path();
                        match self.write_rdp(&path).and_then(|()| self.store_credentials()) {
                            Ok(()) => {
                                self.connect = Some(path);
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                });
            });
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.header(ui);
            ui.add_space(8.0);
            if self.show_options {
                ui.horizontal(|ui| {
                    for (tab, name) in TABS {
                        ui.selectable_value(&mut self.tab, tab, name);
                    }
                });
                ui.separator();
                match self.tab {
                    Tab::General => self.general(ui),
                    Tab::Display => self.display(ui),
                    Tab::LocalResources => self.local_resources(ui),
                    Tab::Experience => {
                        ui.label("Connection quality is negotiated automatically.");
                    }
                    Tab::Advanced => self.advanced(ui),
                }
            } else {
                self.logon_fields(ui);
            }
        });
    }

    fn header(&self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0, 71, 171))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("🖥").size(40.0).color(egui::Color32::WHITE));
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("Remote Desktop").size(20.0).color(egui::Color32::WHITE));
                        ui.label(egui::RichText::new("Connection").size(26.0).strong().color(egui::Color32::WHITE));
                    });
                });
            });
    }

    fn logon_fields(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("logon").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Computer:");
            ui.add(egui::TextEdit::singleline(&mut self.computer).hint_text("Example: computer.fabrikam.com"));
            ui.end_row();
            ui.label("User name:");
            ui.text_edit_singleline(&mut self.username);
            ui.end_row();
            ui.label("Domain:");
            ui.text_edit_singleline(&mut self.domain);
            ui.end_row();
            ui.label("Password:");
            ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
            ui.end_row();
        });
        ui.checkbox(&mut self.save_credentials, "Allow me to save credentials");
    }

    fn uses_gateway(&self) -> bool {
        !self.gateway.trim().is_empty() && self.gateway_usage != GatewayUsageMethod::Direct
    }

    fn asks_gateway_credentials(&self) -> bool {
        self.uses_gateway() && !self.gateway_same_credentials
    }

    fn gateway_logon_fields(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("gateway_logon").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Gateway user name:");
            ui.text_edit_singleline(&mut self.gateway_username);
            ui.end_row();
            ui.label("Gateway password:");
            ui.add(egui::TextEdit::singleline(&mut self.gateway_password).password(true));
            ui.end_row();
        });
    }

    fn advanced(&mut self, ui: &mut egui::Ui) {
        Self::group(ui, "Connect from anywhere", |ui| {
            ui.label("Configure settings to connect through a Remote Desktop Gateway when working remotely.");
            ui.radio_value(&mut self.gateway_usage, GatewayUsageMethod::Detect, "Automatically detect RD Gateway server settings");
            ui.radio_value(&mut self.gateway_usage, GatewayUsageMethod::UseAlways, "Use these RD Gateway server settings");
            ui.radio_value(&mut self.gateway_usage, GatewayUsageMethod::Direct, "Do not use an RD Gateway server");
            if self.gateway_usage != GatewayUsageMethod::Direct {
                ui.horizontal(|ui| {
                    ui.label("Server name:");
                    ui.add(egui::TextEdit::singleline(&mut self.gateway).hint_text("gateway.fabrikam.com"));
                });
                ui.checkbox(&mut self.gateway_same_credentials, "Use my RD Gateway credentials for the remote computer");
                if !self.gateway_same_credentials {
                    self.gateway_logon_fields(ui);
                }
            }
        });
    }

    fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).strong());
            add(ui);
        });
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        Self::group(ui, "Logon settings", |ui| {
            ui.label("Enter the name of the remote computer.");
            self.logon_fields(ui);
        });
        Self::group(ui, "Connection settings", |ui| {
            ui.label("Save the current connection settings to an RDP file or open a saved connection.");
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    let r = self.write_rdp(&default_rdp_path());
                    self.try_or_show(r);
                }
                if ui.button("Save As…").clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter("RDP file", &["rdp"]).save_file()
                {
                    let r = self.write_rdp(&path);
                    self.try_or_show(r);
                }
                if ui.button("Open…").clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter("RDP file", &["rdp"]).pick_file()
                {
                    let r = self.load(&path);
                    self.try_or_show(r);
                }
            });
        });
        Self::group(ui, "RemoteApp", |ui| {
            ui.checkbox(&mut self.remote_app, "Start a remote program instead of a desktop");
            if self.remote_app {
                ui.horizontal(|ui| {
                    ui.label("Program:");
                    ui.add(egui::TextEdit::singleline(&mut self.program).hint_text("||Alias or C:\\path\\app.exe"));
                });
            }
        });
    }

    fn display(&mut self, ui: &mut egui::Ui) {
        Self::group(ui, "Display configuration", |ui| {
            ui.label("Choose the size of your remote desktop.");
            egui::ComboBox::from_id_salt("size")
                .selected_text(size_label(self.size))
                .show_ui(ui, |ui| {
                    for size in SIZES {
                        ui.selectable_value(&mut self.size, size, size_label(size));
                    }
                });
        });
    }

    fn local_resources(&mut self, ui: &mut egui::Ui) {
        Self::group(ui, "Remote audio", |ui| {
            ui.radio_value(&mut self.audio, AudioMode::RedirectToClient, "Play on this computer");
            ui.radio_value(&mut self.audio, AudioMode::PlayOnServer, "Play on remote computer");
            ui.radio_value(&mut self.audio, AudioMode::Disabled, "Do not play");
        });
        Self::group(ui, "Local devices and resources", |ui| {
            ui.label("Choose the devices and resources that you want to use in your remote session.");
            ui.checkbox(&mut self.clipboard, "Clipboard");
            ui.checkbox(&mut self.printers, "Printers (default printer)");
            ui.checkbox(&mut self.smartcard, "Smart cards");
            ui.checkbox(&mut self.microphone, "Microphone");
            ui.horizontal(|ui| {
                ui.label("Drive (folder):");
                ui.add(egui::TextEdit::singleline(&mut self.drives).hint_text("/home/you/share"));
                if ui.button("Browse…").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_folder()
                {
                    self.drives = path.to_string_lossy().into_owned();
                }
            });
        });
    }
}

fn size_label(size: Option<(u16, u16)>) -> String {
    match size {
        Some((w, h)) => format!("{w} by {h} pixels"),
        None => "Full screen".to_owned(),
    }
}

/// Password saved in the system keyring for `user` on `host` (the key mstsc uses: `TERMSRV/<host>`).
pub fn saved_password(host: &str, user: &str) -> Option<String> {
    if host.trim().is_empty() || user.is_empty() {
        return None;
    }
    keyring::Entry::new(&format!("TERMSRV/{}", host.trim()), user)
        .and_then(|entry| entry.get_password())
        .ok()
}

/// mstsc keeps its last settings in `Default.rdp`; we keep ours in the XDG config directory.
fn default_rdp_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default();
    base.join("realrdp").join("Default.rdp")
}

/// Runs the full dialog; returns what to connect with, or `None` when closed.
pub fn run() -> anyhow::Result<Option<Outcome>> {
    let mut dialog = Dialog::default();
    let _ = dialog.load(&default_rdp_path());
    show(dialog, [440.0, 560.0])
}

/// Asks only for credentials for `rdp_file` (like the Windows Security prompt of mstsc).
pub fn credentials(rdp_file: &std::path::Path) -> anyhow::Result<Option<Outcome>> {
    let mut dialog = Dialog::default();
    dialog.load(rdp_file)?;
    dialog.credentials_for = Some(rdp_file.to_owned());
    show(dialog, [440.0, 300.0])
}

fn show(dialog: Dialog, size: [f32; 2]) -> anyhow::Result<Option<Outcome>> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_app_id(crate::WM_CLASS);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(crate::ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        // X11 like the session windows: Wayland has no window icon of its own, only the
        // `.desktop` match, so the icon would otherwise stay winit's default "W".
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11 as _;
            builder.with_x11();
        })),
        ..Default::default()
    };
    // eframe hands the App back only through the closure, so smuggle the result out via a cell.
    let result = std::rc::Rc::new(std::cell::RefCell::new(None));
    let out = result.clone();
    eframe::run_native(
        "Remote Desktop Connection",
        options,
        Box::new(move |_| Ok(Box::new(ResultOnDrop { dialog, out }))),
    )
    .map_err(|e| anyhow::anyhow!("dialog: {e}"))?;
    let taken = result.borrow_mut().take();
    Ok(taken)
}

struct ResultOnDrop {
    dialog: Dialog,
    out: std::rc::Rc<std::cell::RefCell<Option<Outcome>>>,
}

impl eframe::App for ResultOnDrop {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.dialog.ui(ui);
    }
}

impl Drop for ResultOnDrop {
    fn drop(&mut self) {
        if let Some(rdp_file) = self.dialog.connect.take() {
            *self.out.borrow_mut() = Some(Outcome {
                rdp_file,
                username: std::mem::take(&mut self.dialog.username),
                domain: std::mem::take(&mut self.dialog.domain),
                password: std::mem::take(&mut self.dialog.password),
                gateway_username: std::mem::take(&mut self.dialog.gateway_username),
                gateway_password: std::mem::take(&mut self.dialog.gateway_password),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to the real system keyring, so opt in with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let d = Dialog {
            computer: "realrdp-test.invalid".into(),
            username: "tester".into(),
            password: "s3cret".into(),
            save_credentials: true,
            ..Default::default()
        };
        d.store_credentials().unwrap();
        assert_eq!(saved_password("realrdp-test.invalid", "tester").as_deref(), Some("s3cret"));
        Dialog { save_credentials: false, ..d }.store_credentials().unwrap();
        assert_eq!(saved_password("realrdp-test.invalid", "tester"), None);
    }

    #[test]
    fn rdp_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("realrdp-test-{}", std::process::id()));
        let path = dir.join("Default.rdp");
        let d = Dialog {
            computer: "srv.example.com:3390".into(),
            username: "jeremy".into(),
            domain: "CORP".into(),
            size: Some((1920, 1080)),
            clipboard: false,
            audio: AudioMode::Disabled,
            remote_app: true,
            program: "||Outlook".into(),
            drives: "/srv/share".into(),
            printers: true,
            smartcard: true,
            microphone: true,
            gateway: "gw.example.com".into(),
            gateway_usage: GatewayUsageMethod::UseAlways,
            gateway_same_credentials: false,
            gateway_username: "gwuser".into(),
            ..Default::default()
        };
        d.write_rdp(&path).unwrap();
        let mut back = Dialog::default();
        back.load(&path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(back.computer, "srv.example.com:3390");
        assert_eq!(back.username, "jeremy");
        assert_eq!(back.domain, "CORP");
        assert_eq!(back.size, Some((1920, 1080)));
        assert!(!back.clipboard);
        assert_eq!(back.audio, AudioMode::Disabled);
        assert!(back.remote_app);
        assert_eq!(back.program, "||Outlook");
        assert!(back.printers && back.smartcard && back.microphone);
        assert_eq!(back.drives, "/srv/share");
        assert_eq!(back.gateway, "gw.example.com");
        assert_eq!(back.gateway_usage, GatewayUsageMethod::UseAlways);
        assert!(!back.gateway_same_credentials);
        assert_eq!(back.gateway_username, "gwuser");
        assert!(back.asks_gateway_credentials());
        assert!(Dialog { computer: "not a host:x".into(), ..Default::default() }.properties().is_err());
    }
}
