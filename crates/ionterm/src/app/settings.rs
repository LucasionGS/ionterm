//! The settings tab: opening it, carrying out what the page asks for, and
//! running `$EDITOR` in the tab for config files, back to the page when it
//! exits.

use std::path::{Path, PathBuf};

use ionterm_render::ChromeItem;
use winit::event::KeyEvent;
use winit::window::CursorIcon;

use super::{App, TermWindow};
use crate::config::{self, CONFIG_FILE, THEME_FILE, config_dir};
use crate::hints::{self, OpenFile};
use crate::pane::Pane;
use crate::settings::{self, FileKind, FontPicker, Request, SettingsPage};
use crate::tab::Tab;
use crate::theme::Theme;
use crate::files::transfer;
use crate::{ssh, zmodem};

/// What the settings tab's pane runs while the page shows: nothing much.
fn idle_command() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/K".into()]
    } else {
        vec!["sh".into(), "-c".into(), "while :; do sleep 86400; done".into()]
    }
}

/// The editor's name, for the buttons ("nvim").
fn editor_name() -> String {
    hints::editor()
        .and_then(|argv| Path::new(&argv[0]).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "your editor".into())
}

/// The file behind an edit button, created (empty but for a comment) when
/// it doesn't exist yet so the editor has something to open.
fn file_for(kind: FileKind) -> std::io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("no home folder"))?;
    let (path, header) = match kind {
        FileKind::Config => (
            config_dir().ok_or_else(|| std::io::Error::other("no config folder"))?.join(CONFIG_FILE),
            "# ionterm settings. Every option, with its default: extras/ionterm.toml in the ionterm repository.\n",
        ),
        FileKind::Theme => (
            config_dir().ok_or_else(|| std::io::Error::other("no config folder"))?.join(THEME_FILE),
            "# ionterm colours. Every option: extras/ionix-styler/ in the ionterm repository.\n",
        ),
        FileKind::SshConfig => (home.join(".ssh").join("config"), ""),
        FileKind::SshIonterm => {
            // Only used once ~/.ssh/config includes it.
            ssh::store::ensure_include()?;
            (ssh::store::managed_path().ok_or_else(|| std::io::Error::other("no home folder"))?, "")
        }
    };
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, header)?;
    }
    Ok(path)
}

impl TermWindow {
    /// The settings page of the active tab, while it's showing (not while
    /// the editor runs in it).
    pub(super) fn settings_page(&self) -> Option<&SettingsPage> {
        self.tabs[self.active_tab].settings.as_ref().filter(|page| !page.editing)
    }

    fn settings_page_mut(&mut self) -> Option<&mut SettingsPage> {
        self.tabs[self.active_tab].settings.as_mut().filter(|page| !page.editing)
    }

    fn settings_layout(&self) -> Option<settings::PageLayout> {
        let page = self.settings_page()?;
        let area = self.tabs[self.active_tab].pane_area(self.content_area());
        Some(settings::layout(page, area, self.renderer.cell_metrics(), self.scale_factor as f32))
    }

    pub(super) fn draw_settings(&mut self, config: &config::Config, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let Some(layout) = self.settings_layout() else { return };
        let visible = layout.visible;
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let Some(page) = self.settings_page() else { return };
        let ctx = settings::DrawContext {
            config,
            editor_name: &editor_name(),
            opacity: config.window.opacity.unwrap_or(theme.background_opacity),
        };
        settings::draw(page, &layout, &ctx, theme, m, scale, out);
        if let Some(page) = self.settings_page_mut() {
            page.visible = visible;
        }
    }

    /// Hover over the page. Returns whether the page is showing.
    pub(super) fn settings_hover(&mut self, x: f32, y: f32) -> bool {
        let Some(layout) = self.settings_layout() else { return false };
        let hover = if layout.area.contains(x, y) { settings::hit(&layout, x, y) } else { None };
        if let Some(page) = self.settings_page_mut()
            && page.hover != hover
        {
            page.hover = hover;
            self.window.request_redraw();
        }
        self.set_cursor_icon(if hover.is_some() { CursorIcon::Pointer } else { CursorIcon::Default });
        true
    }

    /// The wheel over the page. Returns whether the page took it.
    pub(super) fn settings_wheel(&mut self, lines: isize) -> bool {
        let Some(page) = self.settings_page_mut() else { return false };
        page.scroll_by(-lines);
        self.window.request_redraw();
        true
    }
}

impl App {
    /// Shows the settings tab, opening it if needed.
    pub(super) fn open_settings(&mut self) {
        let Some(w) = &mut self.window else { return };
        if let Some(index) = w.tabs.iter().position(|t| t.settings.is_some()) {
            w.select_tab(index);
            w.window.request_redraw();
            return;
        }
        let metrics = w.renderer.cell_metrics();
        let index = w.tabs.len();
        let mut terminal = self.config.terminal.clone();
        terminal.command = idle_command();
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let zmodem = zmodem::Shared::new(transfer::download_dir(&self.config.files.download_dir));
        match Pane::spawn(id, &terminal, None, None, self.theme.palette(), metrics, self.proxy.clone(), zmodem) {
            Ok(pane) => {
                self.insert_tab(index, pane);
                let Some(w) = &mut self.window else { return };
                if let Some(tab) = w.tabs.get_mut(index) {
                    tab.settings = Some(SettingsPage::new());
                }
                w.window.request_redraw();
            }
            Err(error) => tracing::warn!("could not open settings: {error:#}"),
        }
    }

    /// A key on the settings page. Returns whether the page took it.
    pub(super) fn settings_key(&mut self, event: &KeyEvent) -> bool {
        let Some(w) = &mut self.window else { return false };
        let config = &self.config;
        let Some(page) = w.tabs[w.active_tab].settings.as_mut().filter(|p| !p.editing) else { return false };
        let request = page.key(&event.logical_key, event.text.as_deref(), config);
        w.window.request_redraw();
        if let Some(request) = request {
            self.settings_request(request);
        }
        true
    }

    /// A mouse press on the settings page. Returns whether the page took it.
    pub(super) fn settings_click(&mut self, x: f32, y: f32) -> bool {
        let Some(w) = &mut self.window else { return false };
        let Some(layout) = w.settings_layout() else { return false };
        if !layout.area.contains(x, y) {
            return false;
        }
        let target = settings::hit(&layout, x, y);
        let config = &self.config;
        let Some(page) = w.tabs[w.active_tab].settings.as_mut() else { return false };
        let request = page.click(target, config);
        w.window.request_redraw();
        if let Some(request) = request {
            self.settings_request(request);
        }
        true
    }

    /// Pasted text goes into the field being typed in, if any.
    pub(super) fn settings_paste(&mut self, text: &str) -> bool {
        let Some(w) = &mut self.window else { return false };
        let Some(page) = w.tabs[w.active_tab].settings.as_mut().filter(|p| !p.editing) else { return false };
        if let Some(typing) = &mut page.typing {
            typing.push_str(text.lines().next().unwrap_or_default());
        } else if let Some(picker) = &mut page.picker {
            picker.filter.push_str(text.lines().next().unwrap_or_default());
        }
        w.window.request_redraw();
        true
    }

    fn settings_request(&mut self, request: Request) {
        match request {
            Request::Set { section, key, value } => {
                if let Err(error) = config::write_setting(section, key, value) {
                    if let Some(w) = &mut self.window {
                        w.show_toast(format!("Couldn't save the setting: {error}"));
                    }
                    return;
                }
                // Applied now; the file watcher's reload then finds nothing new.
                self.reload_config();
            }
            Request::PickFont => {
                let Some(w) = &mut self.window else { return };
                let families = w.renderer.monospace_families();
                let current = self.config.font.family.clone();
                if let Some(page) = w.tabs[w.active_tab].settings.as_mut() {
                    page.picker = Some(FontPicker::new(families, &current));
                }
            }
            Request::Edit(kind) => self.edit_config_file(kind),
        }
        if let Some(w) = &self.window {
            w.window.request_redraw();
        }
    }

    /// Opens a config file in `$EDITOR` inside the settings tab.
    fn edit_config_file(&mut self, kind: FileKind) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        let path = match file_for(kind) {
            Ok(path) => path,
            Err(error) => {
                w.show_toast(format!("Couldn't open it: {error}"));
                return;
            }
        };
        match hints::open_file_command("", &path, None, None) {
            Some(OpenFile::Tab(argv)) => {
                let metrics = w.renderer.cell_metrics();
                let tab = w.active_tab;
                let dir = path.parent().map(Path::to_path_buf);
                let Some(pane) = w.tabs[tab].panes.first_mut() else { return };
                match pane.run(&self.config.terminal, &argv, dir.as_deref(), metrics, proxy) {
                    Ok(()) => {
                        if let Some(page) = &mut w.tabs[tab].settings {
                            page.editing = true;
                        }
                        w.relayout(&self.config, true);
                    }
                    Err(error) => w.show_toast(format!("Couldn't start {}: {error:#}", argv[0])),
                }
            }
            Some(OpenFile::Detached(argv)) => {
                let started = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                match started {
                    Ok(_) => w.show_toast(format!("Opened {} in {}", path.display(), argv[0])),
                    Err(error) => w.show_toast(format!("Couldn't run {}: {error}", argv[0])),
                }
            }
            None => w.show_toast("No editor found: set $EDITOR (nvim, vim, nano…)"),
        }
    }

    /// The settings tab's pane ended: the editor quit (back to the page),
    /// or an earlier program's end was reported late. Returns whether it
    /// was the settings tab's.
    pub(super) fn settings_child_exited(&mut self, id: crate::layout::PaneId) -> bool {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return false };
        let Some(tab) = w.tabs.iter().position(|t| t.settings.is_some() && t.pane(id).is_some()) else { return false };
        let metrics = w.renderer.cell_metrics();
        let Some(pane) = w.tabs[tab].pane_mut(id) else { return false };
        if pane.is_running() {
            return true;
        }
        if let Err(error) = pane.run(&self.config.terminal, &idle_command(), None, metrics, proxy) {
            tracing::warn!("settings tab: {error:#}");
        }
        if let Some(page) = &mut w.tabs[tab].settings {
            page.editing = false;
        }
        w.window.request_redraw();
        true
    }
}

/// The settings tab's look in the tab bar.
pub(super) fn tab_title(tab: &Tab) -> Option<(String, char)> {
    tab.settings.as_ref().map(|_| ("Settings".to_string(), crate::tabbar::SETTINGS_ICON))
}
