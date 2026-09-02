//! Attached-client state and event handling.
//!
//! `App` owns everything the TUI needs between frames so `main.rs` stays a
//! clap dispatcher. New modes hook in by adding a field plus a branch in
//! `handle_key` / `handle_mouse`.

use anyhow::Result;
use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use kodade_cli_proto::{
    AgentStateKind, ClientMessage, Direction, LayoutSnapshot, PaneId, Screen, SidebarTabInfo,
    WorkspaceInfo,
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Frame, Terminal};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, net::unix::OwnedWriteHalf, sync::mpsc};

use crate::{
    config, input, mode,
    overlay::{self, Overlay, OverlayEvent, OverlayTarget},
    paste, render,
    selection::{self, Selection, SelectionMode},
    settings,
};

/// Two left clicks on the same cell inside this window are a double click (#12).
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// How long a status-bar note stays up.
const NOTE_TTL: Duration = Duration::from_secs(5);
/// Pace a multi-chunk paste so the socket writer does not flood the daemon.
const PASTE_CHUNK_GAP: Duration = Duration::from_millis(5);

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Server messages the attached client acts on; the reader task drops the rest.
pub enum Update {
    Layout(LayoutSnapshot),
    Session(String),
}

/// What the event loop should do after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Detach,
}

/// A yes/no prompt shown in the status bar; `y` sends `on_yes`, anything else cancels.
struct Confirm {
    message: String,
    on_yes: ClientMessage,
}

/// An in-progress border drag started by a left mouse press.
struct DragState {
    direction: Direction,
    vertical: bool,
    last: u16,
}

pub struct App {
    layout: Option<LayoutSnapshot>,
    prefix: bool,
    rename: bool,
    /// The `prefix W` workspace prompt reuses the rename text buffer (`name`).
    new_workspace: bool,
    name: String,
    rename_target: Option<mode::MenuTarget>,
    drag: Option<DragState>,
    sidebar: bool,
    navigate: Option<usize>,
    copy: Option<mode::CopyMode>,
    menu: Option<mode::Menu>,
    confirm: Option<Confirm>,
    /// Persistent resize mode (`prefix alt+r`): hjkl 1 cell, HJKL 5, esc exits.
    resize: bool,
    /// Focused pane in the newest snapshot and the one before it (`last_pane`).
    focused_pane: Option<PaneId>,
    last_pane: Option<PaneId>,
    /// Settings menu (`prefix s`), drawn over everything else.
    settings: Option<Overlay>,
    /// Status-bar note and the instant it stops being shown.
    note: Option<(String, Instant)>,
    /// Last sanitized paste, copy-mode yank, or mouse selection; re-sent by
    /// the `paste_buffer` action (#21).
    paste_buffer: String,
    /// Session reported by the daemon's `Welcome`; shown in the status bar (#11).
    session_name: String,
    /// `prefix q` pane-id flash expiry (#11).
    flash_until: Option<Instant>,
    /// When the sidebar was last hidden, for the timed gutter hint (#24).
    sidebar_hidden_at: Option<Instant>,
    /// Last OSC-0 title written, so we only re-emit on change (#11).
    last_title: String,
    /// Live mouse selection and whether the button is still held (#12).
    selection: Option<Selection>,
    selecting: bool,
    /// Last left click (when, column, row, count) for double/triple clicks (#12).
    last_click: Option<(Instant, u16, u16, u8)>,
    /// Runtime mouse capture; `prefix m` toggles it without touching the
    /// config so the host terminal can take the mouse back (#12).
    mouse_capture: bool,
    config: config::Config,
    theme: config::Theme,
}

/// How long the `prefix q` pane-id flash stays up.
const FLASH: Duration = Duration::from_secs(1);
/// How long the `prefix b · sidebar` hint lingers after hiding the sidebar.
const SIDEBAR_HINT: Duration = Duration::from_secs(3);

impl App {
    pub fn new(config: &config::Config, session: &str) -> Self {
        Self {
            layout: None,
            prefix: false,
            rename: false,
            new_workspace: false,
            name: String::new(),
            rename_target: None,
            drag: None,
            sidebar: config.sidebar,
            navigate: None,
            copy: None,
            menu: None,
            confirm: None,
            resize: false,
            focused_pane: None,
            last_pane: None,
            settings: None,
            note: None,
            paste_buffer: String::new(),
            session_name: session.to_string(),
            flash_until: None,
            sidebar_hidden_at: None,
            last_title: String::new(),
            selection: None,
            selecting: false,
            last_click: None,
            mouse_capture: config.mouse,
            theme: config.resolve_theme(),
            config: config.clone(),
        }
    }

    /// Sets the status-bar note; it clears itself after `NOTE_TTL`.
    fn set_note(&mut self, text: impl Into<String>) {
        self.note = Some((text.into(), Instant::now() + NOTE_TTL));
    }

    // The note, unless it has expired.
    fn note(&self) -> Option<&str> {
        self.note
            .as_ref()
            .filter(|(_, expiry)| *expiry > Instant::now())
            .map(|(text, _)| text.as_str())
    }

    /// Pane width for the current sidebar state, used by `Hello` and `Resize`.
    pub fn pane_cols(&self, cols: u16) -> u16 {
        pane_cols(cols, self.sidebar)
    }

    /// Stores a new snapshot and keeps copy mode pointed at fresh screen text.
    pub fn handle_layout(&mut self, layout: LayoutSnapshot) {
        if let Some(copy_mode) = &mut self.copy {
            if let Some(pane) = layout.panes.iter().find(|pane| pane.id == copy_mode.pane) {
                copy_mode.refresh(pane.screen.clone());
            }
        }
        // A selection belongs to one pane's current output: drop it when focus
        // moves, when its pane goes away, or (opt-in) when the pane redraws.
        if let Some(selection) = &self.selection {
            let pane = layout.panes.iter().find(|pane| pane.id == selection.pane);
            let changed = self
                .pane_screen(selection.pane)
                .zip(pane)
                .is_some_and(|(old, new)| old.contents != new.screen.contents);
            if pane.is_none() || (changed && self.config.clear_on_output) {
                self.clear_selection();
            }
        }
        // Remember the previously focused pane so `last_pane` can jump back.
        let focused = layout.panes.iter().find(|pane| pane.focused).map(|p| p.id);
        if focused != self.focused_pane {
            if let Some(previous) = self.focused_pane {
                self.last_pane = Some(previous);
            }
            self.focused_pane = focused;
            self.clear_selection();
        }
        self.layout = Some(layout);
    }

    pub fn handle_session(&mut self, session: String) {
        self.session_name = session;
    }

    pub fn draw(&self, frame: &mut Frame) {
        let Some(layout) = &self.layout else { return };
        render::render(
            frame,
            layout,
            &render::Ui {
                sidebar: self.sidebar,
                prefix: self.prefix,
                rename: self.rename,
                new_workspace: self.new_workspace,
                name: &self.name,
                navigate: self.navigate,
                copy: self.copy.as_ref(),
                menu: self.menu.as_ref(),
                resize: self.resize,
                confirm: self.confirm.as_ref().map(|c| c.message.as_str()),
                settings: self.settings.as_ref(),
                note: self.note(),
                session: &self.session_name,
                status_right: &self.config.status_right,
                flash: self.flash_active(),
                sidebar_hint: self.sidebar_hint_active(),
                selection: self.selection.as_ref(),
            },
            &self.theme,
        )
    }

    /// Whether the `prefix q` pane-id flash is still showing.
    fn flash_active(&self) -> bool {
        self.flash_until.is_some_and(|until| Instant::now() < until)
    }

    /// Whether the timed `prefix b · sidebar` hint should show (sidebar hidden).
    fn sidebar_hint_active(&self) -> bool {
        !self.sidebar
            && self
                .sidebar_hidden_at
                .is_some_and(|at| at.elapsed() < SIDEBAR_HINT)
    }

    /// Sets the host terminal title (OSC 0) when the workspace/tab changed.
    fn sync_title(&mut self, term: &mut Term) -> Result<()> {
        let Some(title) = self.window_title() else {
            return Ok(());
        };
        if title != self.last_title {
            write!(term.backend_mut(), "\x1b]0;{title}\x07")?;
            term.backend_mut().flush()?;
            self.last_title = title;
        }
        Ok(())
    }

    /// Renders the `ui.window_title` template from the active workspace/tab.
    fn window_title(&self) -> Option<String> {
        let layout = self.layout.as_ref()?;
        let workspace = layout
            .workspaces
            .iter()
            .find(|workspace| workspace.active)
            .map(|workspace| workspace.name.as_str())
            .unwrap_or("");
        let tab = self.active_tab().map(|tab| tab.name.as_str()).unwrap_or("");
        Some(
            self.config
                .window_title
                .replace("{session}", &self.session_name)
                .replace("{workspace}", workspace)
                .replace("{tab}", tab),
        )
    }

    /// The attached event loop: drain daemon updates, draw, handle input.
    pub async fn run(
        &mut self,
        term: &mut Term,
        writer: &mut OwnedWriteHalf,
        rx: &mut mpsc::Receiver<Update>,
    ) -> Result<()> {
        loop {
            while let Ok(update) = rx.try_recv() {
                match update {
                    Update::Layout(layout) => self.handle_layout(layout),
                    Update::Session(session) => self.handle_session(session),
                }
            }
            self.sync_title(term)?;
            term.draw(|frame| self.draw(frame))?;
            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            match event::read()? {
                Event::Resize(cols, rows) => {
                    let cols = self.pane_cols(cols);
                    write(writer, &ClientMessage::Resize { cols, rows }).await?
                }
                Event::Key(key) => {
                    if self.handle_key(key, writer, term).await? == Flow::Detach {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    self.handle_mouse(mouse, writer, term).await?;
                }
                Event::Paste(text) => self.handle_paste(text, writer).await?,
                _ => {}
            }
        }
    }

    /// Routes a key to the active mode, falling back to PTY passthrough.
    pub async fn handle_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<Flow> {
        // Any keystroke ends a mouse selection (#12).
        self.clear_selection();
        if self.confirm.is_some() {
            self.handle_confirm_key(key, writer).await?;
        } else if self.rename {
            self.handle_rename_key(key, writer).await?;
        } else if self.new_workspace {
            self.handle_new_workspace_key(key, writer).await?;
        } else if self.copy.is_some() {
            self.handle_copy_key(key, writer, term).await?;
        } else if self.menu.is_some() {
            self.handle_menu_key(key, writer).await?;
        } else if self.settings.is_some() {
            self.handle_settings_key(key, writer, term).await?;
        } else if let Some(current) = self.navigate {
            self.handle_navigate_key(key, current, writer).await?;
        } else if self.resize {
            self.handle_resize_key(key, writer).await?;
        } else if self.prefix {
            return self.handle_prefix_key(key, writer, term).await;
        } else if config::normalize_key(key) == self.config.prefix {
            self.prefix = true;
        } else if let Some(action) = self.config.global_action(key) {
            // Global chords (ctrl/alt, no `prefix+`) fire before the pane sees the key.
            return self.run_action(action, writer, term).await;
        } else if let Some(bytes) = bytes(key) {
            write(writer, &ClientMessage::Input { bytes }).await?;
        }
        Ok(Flow::Continue)
    }

    // Rename mode: type a name, enter commits it to the stored target.
    async fn handle_rename_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        match key.code {
            KeyCode::Enter => {
                let name = std::mem::take(&mut self.name);
                let message = match self.rename_target.take() {
                    Some(mode::MenuTarget::Pane(id)) => ClientMessage::RenamePaneId { id, name },
                    Some(mode::MenuTarget::Tab(id)) => ClientMessage::RenameTabId { id, name },
                    Some(mode::MenuTarget::Workspace(id)) => {
                        ClientMessage::RenameWorkspaceId { id, name }
                    }
                    None => ClientMessage::RenamePane { name },
                };
                write(writer, &message).await?;
                self.rename = false
            }
            KeyCode::Esc => {
                self.name.clear();
                self.rename = false
            }
            KeyCode::Backspace => {
                self.name.pop();
            }
            KeyCode::Char(c) => self.name.push(c),
            _ => {}
        }
        Ok(())
    }

    // Confirm prompt: `y` runs the pending message, anything else cancels.
    async fn handle_confirm_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let confirm = self.confirm.take().expect("confirm exists");
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            write(writer, &confirm.on_yes).await?;
        }
        Ok(())
    }

    // Workspace prompt: type `NAME [PATH]`, enter creates it (reuses `name`).
    async fn handle_new_workspace_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        match key.code {
            KeyCode::Enter => {
                let input = std::mem::take(&mut self.name);
                let focused_cwd = self
                    .layout
                    .as_ref()
                    .and_then(|layout| layout.panes.iter().find(|pane| pane.focused))
                    .and_then(|pane| pane.cwd.clone());
                let (name, root) = parse_workspace_prompt(
                    &input,
                    focused_cwd.as_deref(),
                    dirs::home_dir().as_deref(),
                );
                write(writer, &ClientMessage::NewWorkspace { name, root }).await?;
                self.new_workspace = false;
            }
            KeyCode::Esc => {
                self.name.clear();
                self.new_workspace = false;
            }
            KeyCode::Backspace => {
                self.name.pop();
            }
            KeyCode::Char(c) => self.name.push(c),
            _ => {}
        }
        Ok(())
    }

    // Resize mode: stays active until esc/enter; hjkl move 1 cell, HJKL move 5.
    async fn handle_resize_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let (direction, cells) = match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.resize = false;
                return Ok(());
            }
            KeyCode::Char('h') | KeyCode::Left => (Direction::Left, 1),
            KeyCode::Char('j') | KeyCode::Down => (Direction::Down, 1),
            KeyCode::Char('k') | KeyCode::Up => (Direction::Up, 1),
            KeyCode::Char('l') | KeyCode::Right => (Direction::Right, 1),
            KeyCode::Char('H') => (Direction::Left, 5),
            KeyCode::Char('J') => (Direction::Down, 5),
            KeyCode::Char('K') => (Direction::Up, 5),
            KeyCode::Char('L') => (Direction::Right, 5),
            _ => return Ok(()),
        };
        write(writer, &ClientMessage::ResizePane { direction, cells }).await
    }

    // Copy mode: vi-style movement, `v` anchors, `y` copies through OSC 52.
    async fn handle_copy_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        let Some(mut copy_mode) = self.copy.take() else {
            return Ok(());
        };
        let mut keep = true;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => keep = false,
            KeyCode::Char('v') => copy_mode.anchor = Some(copy_mode.cursor),
            KeyCode::Char('y') => {
                if let Some(anchor) = copy_mode.anchor {
                    let text =
                        mode::selected_text(&copy_mode.screen.contents, anchor, copy_mode.cursor);
                    // Fill the internal paste buffer so `paste_buffer` can re-send it.
                    self.paste_buffer = text.clone();
                    let (payload, truncated) = mode::osc52(&text);
                    execute!(term.backend_mut(), crossterm::style::Print(payload))?;
                    term.backend_mut().flush()?;
                    self.set_note(if truncated {
                        " copied (truncated to 100KB)"
                    } else {
                        " copied"
                    });
                    keep = false;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => copy_mode.move_by(-1, 0),
            KeyCode::Down | KeyCode::Char('j') => copy_mode.move_by(1, 0),
            KeyCode::Left | KeyCode::Char('h') => copy_mode.move_by(0, -1),
            KeyCode::Right | KeyCode::Char('l') => copy_mode.move_by(0, 1),
            KeyCode::PageUp | KeyCode::Char('u')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || matches!(key.code, KeyCode::PageUp) =>
            {
                write(
                    writer,
                    &ClientMessage::ScrollPane {
                        id: copy_mode.pane,
                        delta: 20,
                    },
                )
                .await?
            }
            KeyCode::PageDown | KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || matches!(key.code, KeyCode::PageDown) =>
            {
                write(
                    writer,
                    &ClientMessage::ScrollPane {
                        id: copy_mode.pane,
                        delta: -20,
                    },
                )
                .await?
            }
            _ => {}
        }
        if keep {
            self.copy = Some(copy_mode);
        }
        Ok(())
    }

    // Context menu: move the selection or run the highlighted action.
    async fn handle_menu_key(&mut self, key: KeyEvent, writer: &mut OwnedWriteHalf) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.menu = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(menu) = &mut self.menu {
                    menu.move_by(-1)
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(menu) = &mut self.menu {
                    menu.move_by(1)
                }
            }
            KeyCode::Enter => self.execute_menu(writer).await?,
            _ => {}
        }
        Ok(())
    }

    // Navigate mode: move through sidebar rows and activate one.
    async fn handle_navigate_key(
        &mut self,
        key: KeyEvent,
        current: usize,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let rows = render::sidebar_rows(self.layout.as_ref().expect("navigate has layout"));
        let targets = rows
            .iter()
            .map(|row| row.target.clone())
            .collect::<Vec<_>>();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.navigate = None;
                if !self.config.sidebar {
                    self.sidebar = false;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.navigate = mode::navigate(&targets, Some(current), -1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.navigate = mode::navigate(&targets, Some(current), 1)
            }
            KeyCode::Enter => {
                self.activate_sidebar(writer, rows[current].target.clone())
                    .await?;
                self.navigate = None;
            }
            _ => {}
        }
        Ok(())
    }

    // Prefix mode: the key after the prefix selects a bound action.
    async fn handle_prefix_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<Flow> {
        self.prefix = false;
        if config::normalize_key(key) == self.config.prefix {
            write(
                writer,
                &ClientMessage::Input {
                    bytes: bytes(key).unwrap_or_default(),
                },
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let Some(action) = self.config.action(key) else {
            return Ok(Flow::Continue);
        };
        self.run_action(action, writer, term).await
    }

    // Runs a bound action, whether it came from the prefix table or a global chord.
    async fn run_action(
        &mut self,
        action: config::Action,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<Flow> {
        match action {
            config::Action::DisplayPanes => {
                self.flash_until = Some(Instant::now() + FLASH);
            }
            config::Action::MouseToggle => {
                self.mouse_capture = !self.mouse_capture;
                set_mouse_capture(term, self.mouse_capture)?;
                self.set_note(if self.mouse_capture {
                    " mouse capture on"
                } else {
                    " mouse capture off · prefix m to re-enable"
                });
            }
            config::Action::SidebarToggle => {
                self.sidebar = !self.sidebar;
                // Start the timed gutter hint when the sidebar just went away.
                if !self.sidebar {
                    self.sidebar_hidden_at = Some(Instant::now());
                }
                self.send_resize(writer, term).await?;
            }
            config::Action::ReloadConfig => self.reload_config(term)?,
            config::Action::Settings => {
                self.settings = Some(settings::overlay(&self.config, 0));
            }
            config::Action::Navigate => {
                self.sidebar = true;
                self.navigate = Some(0);
            }
            config::Action::CopyMode => {
                if let Some(current) = &self.layout {
                    if let Some(pane) = current.panes.iter().find(|pane| pane.focused) {
                        self.copy = Some(mode::CopyMode::new(pane.id, pane.screen.clone()));
                    }
                }
            }
            config::Action::Rename => self.rename = true,
            config::Action::NewWorkspace => self.new_workspace = true,
            config::Action::Detach => return Ok(Flow::Detach),
            config::Action::ResizeMode => self.resize = true,
            config::Action::RenameTab => {
                if let Some(id) = self.active_tab().map(|tab| tab.id) {
                    self.rename = true;
                    self.rename_target = Some(mode::MenuTarget::Tab(id));
                }
            }
            config::Action::RenameWorkspace => {
                if let Some(id) = self.active_workspace().map(|workspace| workspace.id) {
                    self.rename = true;
                    self.rename_target = Some(mode::MenuTarget::Workspace(id));
                }
            }
            config::Action::CloseTab => {
                if let Some(prompt) = self.active_tab().map(|tab| {
                    (
                        ClientMessage::CloseTab { id: tab.id },
                        close_tab_prompt(tab),
                    )
                }) {
                    let (message, prompt) = prompt;
                    match prompt {
                        Some(prompt) => {
                            self.confirm = Some(Confirm {
                                message: prompt,
                                on_yes: message,
                            })
                        }
                        None => write(writer, &message).await?,
                    }
                }
            }
            config::Action::CloseWorkspace => {
                if let Some(prompt) = self.active_workspace().map(|workspace| {
                    (
                        ClientMessage::CloseWorkspace { id: workspace.id },
                        close_workspace_prompt(workspace),
                    )
                }) {
                    let (message, prompt) = prompt;
                    match prompt {
                        Some(prompt) => {
                            self.confirm = Some(Confirm {
                                message: prompt,
                                on_yes: message,
                            })
                        }
                        None => write(writer, &message).await?,
                    }
                }
            }
            config::Action::LastPane => {
                if let Some(id) = self.last_pane {
                    write(writer, &ClientMessage::FocusPaneId { id }).await?
                }
            }
            config::Action::PasteBuffer => {
                if self.paste_buffer.is_empty() {
                    self.set_note(" paste buffer empty");
                } else {
                    let text = self.paste_buffer.clone();
                    self.send_paste(&text, writer).await?;
                }
            }
            other => {
                if let Some(message) = other.message() {
                    write(writer, &message).await?
                }
            }
        }
        Ok(Flow::Continue)
    }

    // Settings menu: move the selection, toggle a setting, or close.
    async fn handle_settings_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        let Some(mut menu) = self.settings.take() else {
            return Ok(());
        };
        match overlay::overlay_key(&mut menu, key) {
            // Dropping the overlay closes it.
            OverlayEvent::Cancel => return Ok(()),
            OverlayEvent::Select => {
                let Some(OverlayTarget::Index(index)) =
                    menu.current().map(|row| row.target.clone())
                else {
                    self.settings = Some(menu);
                    return Ok(());
                };
                let setting = settings::SETTINGS[index];
                settings::toggle(&mut self.config, setting);
                self.apply_setting(setting, writer, term).await?;
                if let Err(error) = settings::write(&config::config_path(), &self.config) {
                    self.set_note(format!(" config write failed: {error}"));
                }
                // Rebuild so the row shows the new value.
                self.settings = Some(settings::overlay(&self.config, menu.selected));
            }
            _ => self.settings = Some(menu),
        }
        Ok(())
    }

    // Applies a just-toggled setting to the running client.
    async fn apply_setting(
        &mut self,
        setting: settings::Setting,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        match setting {
            settings::Setting::Theme => {
                let config = self.config.clone();
                self.apply_theme(&config);
                if config.theme == config::ThemeChoice::Auto {
                    self.set_note(" auto resolves on next start");
                }
            }
            settings::Setting::Mouse => {
                self.mouse_capture = self.config.mouse;
                set_mouse_capture(term, self.mouse_capture)?;
            }
            settings::Setting::Sidebar => {
                self.sidebar = self.config.sidebar;
                self.send_resize(writer, term).await?;
            }
            settings::Setting::CopyOnSelect | settings::Setting::Notify => {}
        }
        Ok(())
    }

    // `prefix R`: re-read config.toml and the theme without detaching. A broken
    // file keeps the previous config and reports it in the status bar.
    fn reload_config(&mut self, term: &mut Term) -> Result<()> {
        match config::Config::load_checked() {
            Ok(config) => {
                self.apply_theme(&config);
                if config.mouse != self.mouse_capture {
                    self.mouse_capture = config.mouse;
                    set_mouse_capture(term, self.mouse_capture)?;
                }
                let note = match config.warnings.first() {
                    Some(warning) => format!(" config reloaded · {warning}"),
                    None => " config reloaded".into(),
                };
                self.config = config;
                self.set_note(note);
            }
            Err(error) => self.set_note(format!(" config error: {error} · previous config kept")),
        }
        Ok(())
    }

    // Re-resolves the theme. `auto` keeps the current theme because its OSC 11
    // query cannot run while the TUI owns the terminal.
    fn apply_theme(&mut self, config: &config::Config) {
        if config.theme != config::ThemeChoice::Auto {
            self.theme = config.resolve_theme();
        }
    }

    // Tells the daemon the pane area after the sidebar changed.
    async fn send_resize(&self, writer: &mut OwnedWriteHalf, term: &mut Term) -> Result<()> {
        let size = term.size()?;
        write(
            writer,
            &ClientMessage::Resize {
                cols: self.pane_cols(size.width),
                rows: size.height,
            },
        )
        .await
    }

    // A bracketed-paste event: sanitize (when enabled), remember it, and send it.
    async fn handle_paste(&mut self, text: String, writer: &mut OwnedWriteHalf) -> Result<()> {
        let text = if self.config.paste_sanitize {
            paste::sanitize(&text)
        } else {
            text
        };
        self.paste_buffer = text.clone();
        self.send_paste(&text, writer).await
    }

    // Frames text for the focused pane and sends it, pacing multi-chunk pastes.
    // Bracketed panes get the paste markers; the daemon writes the bytes as-is.
    async fn send_paste(&self, text: &str, writer: &mut OwnedWriteHalf) -> Result<()> {
        let bracketed = self
            .layout
            .as_ref()
            .and_then(|layout| layout.panes.iter().find(|pane| pane.focused))
            .map(|pane| pane.screen.bracketed_paste)
            .unwrap_or(false);
        let bytes = paste::wrap(text, bracketed);
        let chunks = paste::chunks(&bytes, paste::CHUNK_SIZE);
        let many = chunks.len() > 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            if many && index > 0 {
                tokio::time::sleep(PASTE_CHUNK_GAP).await;
            }
            write(writer, &ClientMessage::Input { bytes: chunk }).await?;
        }
        Ok(())
    }

    /// Routes a mouse event; ignored entirely when mouse support is off.
    pub async fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        if !self.mouse_capture || self.layout.is_none() {
            return Ok(());
        }
        // The settings overlay owns the mouse while it is up.
        if self.settings.is_some() {
            return self.settings_mouse(mouse, writer, term).await;
        }
        // A pane app that turned mouse reporting on gets the event verbatim.
        if self.passthrough(mouse, writer, term).await? {
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_left_down(mouse, writer, term).await?
            }
            MouseEventKind::Down(MouseButton::Right) => self.mouse_right_down(mouse, term)?,
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                self.drag_selection(mouse, term)?;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(dragging) = &mut self.drag {
                    let now = if dragging.vertical {
                        mouse.column
                    } else {
                        mouse.row
                    };
                    let cells = input::drag_delta(dragging.last, now);
                    if cells != 0 {
                        let direction = dragging.direction;
                        dragging.last = now;
                        write(writer, &ClientMessage::ResizePane { direction, cells }).await?;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                self.finish_selection(term)?;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let area = self.content_area(term)?;
                let current = self.layout.as_ref().expect("layout present");
                let rects = render::pane_rects_for(current, area);
                if let Some(id) = input::pane_at(&rects, mouse.column, mouse.row) {
                    let step = self.config.scroll_lines;
                    let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        step
                    } else {
                        -step
                    };
                    write(writer, &ClientMessage::ScrollPane { id, delta }).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // Left click: menu selection, sidebar rows, pane borders, tabs, then panes.
    async fn mouse_left_down(
        &mut self,
        mouse: MouseEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        let area_width = term.size()?.width;
        if let Some(menu) = &mut self.menu {
            if let Some(selected) = mode::menu_hit(menu, mouse.column, mouse.row, area_width) {
                menu.selected = selected;
                self.execute_menu(writer).await?;
            } else {
                self.menu = None;
            }
            return Ok(());
        }
        let size = term.size()?;
        let frame_area = Rect::new(0, 0, size.width, size.height);
        let content_area = render::content_area(frame_area, self.sidebar);
        let current = self.layout.as_ref().expect("layout present");
        if mouse.column < content_area.x {
            if self.sidebar {
                if let Some(row) = render::sidebar_row_at(&render::sidebar_rows(current), mouse.row)
                {
                    let message = sidebar_message(row.target.clone());
                    write(writer, &message).await?;
                }
            } else {
                self.sidebar = true;
                let cols = self.pane_cols(size.width);
                write(
                    writer,
                    &ClientMessage::Resize {
                        cols,
                        rows: size.height,
                    },
                )
                .await?;
            }
            return Ok(());
        }
        let rects = render::pane_rects_for(current, content_area);
        if let Some(border) = input::border_at(&rects, mouse.column, mouse.row) {
            write(writer, &ClientMessage::FocusPaneId { id: border.pane }).await?;
            self.drag = Some(DragState {
                direction: border.direction,
                vertical: border.vertical,
                last: if border.vertical {
                    mouse.column
                } else {
                    mouse.row
                },
            });
        } else if mouse.row == 0 {
            if let Some(id) =
                input::tab_at(&render::tab_spans_for(current, content_area), mouse.column)
            {
                write(writer, &ClientMessage::SelectTab { id }).await?;
            }
        } else if let Some((id, (col, row))) = pane_cell_at(&rects, mouse.column, mouse.row) {
            write(writer, &ClientMessage::FocusPaneId { id }).await?;
            // Ctrl/cmd-click opens a link instead of starting a selection.
            if mouse
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
            {
                self.clear_selection();
                self.open_link(id, row as usize, col as usize);
                return Ok(());
            }
            let mode = match self.click_count(mouse.column, mouse.row) {
                2 => SelectionMode::Word,
                3 => SelectionMode::Line,
                _ => SelectionMode::Char,
            };
            let started = self
                .pane_screen(id)
                .map(|screen| Selection::new(id, (row as usize, col as usize), mode, screen));
            self.selection = started;
            self.selecting = self.selection.is_some();
        } else if input::pane_at(&rects, mouse.column, mouse.row).is_none() {
            self.clear_selection();
        }
        Ok(())
    }

    // Forwards a mouse event to a pane app that enabled mouse reporting, as
    // SGR (1006) bytes. Returns true when the pane consumed it. The tab bar,
    // sidebar, borders, the context menu, and ctrl/cmd-clicks stay local.
    async fn passthrough(
        &mut self,
        mouse: MouseEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<bool> {
        if !self.config.passthrough
            || self.menu.is_some()
            || self.copy.is_some()
            || matches!(mouse.kind, MouseEventKind::Moved)
            || mouse
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return Ok(false);
        }
        let content = self.content_area(term)?;
        let current = self.layout.as_ref().expect("layout present");
        let rects = render::pane_rects_for(current, content);
        let Some((id, (col, row))) = pane_cell_at(&rects, mouse.column, mouse.row) else {
            return Ok(false);
        };
        if !current
            .panes
            .iter()
            .any(|pane| pane.id == id && pane.screen.mouse_reporting)
        {
            return Ok(false);
        }
        let bytes = selection::sgr_mouse(mouse.kind, mouse.modifiers, col, row);
        write(writer, &ClientMessage::SendToPane { id, bytes }).await?;
        Ok(true)
    }

    // Extends the active selection to the cell under the pointer, clamped to
    // the pane so dragging past an edge keeps selecting.
    fn drag_selection(&mut self, mouse: MouseEvent, term: &mut Term) -> Result<()> {
        let content = self.content_area(term)?;
        let Some(mut selection) = self.selection.take() else {
            self.selecting = false;
            return Ok(());
        };
        let current = self.layout.as_ref().expect("layout present");
        let rect = render::pane_rects_for(current, content)
            .into_iter()
            .find(|(id, _)| *id == selection.pane)
            .map(|(_, rect)| rect);
        if let Some((col, row)) = rect.map(|rect| clamped_cell(rect, mouse.column, mouse.row)) {
            if let Some(screen) = self.pane_screen(selection.pane) {
                selection.set_head((row as usize, col as usize), screen);
            }
        }
        self.selection = Some(selection);
        Ok(())
    }

    // Mouse up: a plain click clears, a real drag copies when
    // `mouse.copy_on_select` is on and always fills the paste buffer.
    fn finish_selection(&mut self, term: &mut Term) -> Result<()> {
        self.selecting = false;
        let Some(selection) = self.selection.clone() else {
            return Ok(());
        };
        if selection.is_click() {
            self.selection = None;
            return Ok(());
        }
        let text = match self.pane_screen(selection.pane) {
            Some(screen) => selection.text(screen),
            None => String::new(),
        };
        if text.is_empty() {
            self.selection = None;
            return Ok(());
        }
        // Always fill the internal buffer so `prefix ]` can re-paste it (#21).
        self.paste_buffer = text.clone();
        if self.config.copy_on_select {
            let (payload, truncated) = mode::osc52(&text);
            execute!(term.backend_mut(), crossterm::style::Print(payload))?;
            term.backend_mut().flush()?;
            self.set_note(if truncated {
                " copied (truncated to 100KB)"
            } else {
                " copied"
            });
        }
        Ok(())
    }

    // Click count for double/triple clicks: same cell inside `MULTI_CLICK`.
    fn click_count(&mut self, column: u16, row: u16) -> u8 {
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, last_column, last_row, count))
                if last_column == column
                    && last_row == row
                    && now.duration_since(at) < MULTI_CLICK =>
            {
                (count + 1).min(3)
            }
            _ => 1,
        };
        self.last_click = Some((now, column, row, count));
        count
    }

    // Ctrl/cmd-click: open the URL under the pointer with `ui.link_command`.
    fn open_link(&mut self, pane: PaneId, row: usize, col: usize) {
        let url = self
            .pane_screen(pane)
            .and_then(|screen| selection::link_at(screen, row, col));
        let Some(url) = url else {
            self.set_note(" no link here");
            return;
        };
        // Detached: the opener owns the URL from here, we never wait on it.
        let spawned = Command::new(&self.config.link_command)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                drop(child);
                self.set_note(format!(" opened {url}"));
            }
            Err(error) => self.set_note(format!(" open failed: {error}")),
        }
    }

    // The newest screen for a pane, if it is still in the layout.
    fn pane_screen(&self, pane: PaneId) -> Option<&Screen> {
        self.layout
            .as_ref()?
            .panes
            .iter()
            .find(|candidate| candidate.id == pane)
            .map(|candidate| &candidate.screen)
    }

    // Drops any mouse selection and its drag state.
    fn clear_selection(&mut self) {
        self.selection = None;
        self.selecting = false;
    }

    // Clicks while the settings overlay is open: a row activates, anything
    // else closes it. Nothing falls through to panes, tabs, or the sidebar.
    async fn settings_mouse(
        &mut self,
        mouse: MouseEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Down(MouseButton::Right)
        ) {
            return Ok(());
        }
        let size = term.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let menu = self.settings.as_ref().expect("settings overlay open");
        let row = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            .then(|| overlay::row_at(area, menu, mouse.column, mouse.row))
            .flatten();
        match row {
            Some(index) => {
                if let Some(menu) = &mut self.settings {
                    menu.selected = index;
                }
                // Reuse the enter path so a click and a keypress behave alike.
                self.handle_settings_key(KeyEvent::from(KeyCode::Enter), writer, term)
                    .await?;
            }
            None => self.settings = None,
        }
        Ok(())
    }

    // Right click: open the context menu for the pane or sidebar row under it.
    fn mouse_right_down(&mut self, mouse: MouseEvent, term: &mut Term) -> Result<()> {
        let content = self.content_area(term)?;
        let current = self.layout.as_ref().expect("layout present");
        let target = if self.sidebar && mouse.column < content.x {
            render::sidebar_row_at(&render::sidebar_rows(current), mouse.row).map(|row| {
                match row.target {
                    render::SidebarTarget::Workspace(id) => mode::MenuTarget::Workspace(id),
                    render::SidebarTarget::Tab(id) => mode::MenuTarget::Tab(id),
                    render::SidebarTarget::Pane(id) => mode::MenuTarget::Pane(id),
                }
            })
        } else {
            input::pane_at(
                &render::pane_rects_for(current, content),
                mouse.column,
                mouse.row,
            )
            .map(mode::MenuTarget::Pane)
        };
        self.menu = target.map(|target| mode::Menu {
            target,
            x: mouse.column,
            y: mouse.row,
            selected: 0,
        });
        Ok(())
    }

    // Focuses the workspace, tab, or pane behind a sidebar row.
    async fn activate_sidebar(
        &mut self,
        writer: &mut OwnedWriteHalf,
        target: render::SidebarTarget,
    ) -> Result<()> {
        write(writer, &sidebar_message(target)).await
    }

    // Runs the highlighted menu action and closes the menu.
    async fn execute_menu(&mut self, writer: &mut OwnedWriteHalf) -> Result<()> {
        let menu = self.menu.take().expect("menu exists");
        if let mode::MenuTarget::Pane(id) = menu.target {
            write(writer, &ClientMessage::FocusPaneId { id }).await?;
        }
        match menu.action() {
            mode::MenuAction::Rename => {
                self.rename = true;
                self.rename_target = Some(menu.target);
            }
            mode::MenuAction::SplitRight => write(writer, &ClientMessage::SplitRight).await?,
            mode::MenuAction::SplitDown => write(writer, &ClientMessage::SplitDown).await?,
            mode::MenuAction::Zoom => write(writer, &ClientMessage::ZoomPane).await?,
            mode::MenuAction::BreakToTab => write(writer, &ClientMessage::BreakPane).await?,
            mode::MenuAction::Equalize => write(writer, &ClientMessage::EqualizeLayout).await?,
            mode::MenuAction::MoveLeft | mode::MenuAction::MoveRight => {
                // Tab moves apply to the active tab, so select the target first.
                if let mode::MenuTarget::Tab(id) = menu.target {
                    write(writer, &ClientMessage::SelectTab { id }).await?;
                    let delta = if matches!(menu.action(), mode::MenuAction::MoveLeft) {
                        -1
                    } else {
                        1
                    };
                    write(writer, &ClientMessage::MoveTab { delta }).await?;
                }
            }
            mode::MenuAction::Close => {
                let message = match menu.target {
                    mode::MenuTarget::Pane(_) => ClientMessage::ClosePane,
                    mode::MenuTarget::Tab(id) => ClientMessage::CloseTab { id },
                    mode::MenuTarget::Workspace(id) => ClientMessage::CloseWorkspace { id },
                };
                write(writer, &message).await?;
            }
        }
        Ok(())
    }

    // Active workspace in the newest snapshot, if any.
    fn active_workspace(&self) -> Option<&WorkspaceInfo> {
        self.layout
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.active)
    }

    // Active tab of the active workspace, with its agent list.
    fn active_tab(&self) -> Option<&SidebarTabInfo> {
        let layout = self.layout.as_ref()?;
        self.active_workspace()?
            .tabs
            .iter()
            .find(|tab| tab.id == layout.active_tab)
    }

    // Terminal area left for panes once the sidebar is subtracted.
    fn content_area(&self, term: &Term) -> Result<Rect> {
        let size = term.size()?;
        Ok(render::content_area(
            Rect::new(0, 0, size.width, size.height),
            self.sidebar,
        ))
    }
}

/// Pane-relative cell under the pointer, or `None` on a border or outside.
/// Panes draw a one-cell border, so the text grid starts at `rect + 1`.
fn pane_cell_at(rects: &[(PaneId, Rect)], column: u16, row: u16) -> Option<(PaneId, (u16, u16))> {
    rects.iter().find_map(|(id, rect)| {
        let inner = inner_area(*rect);
        inner
            .contains((column, row).into())
            .then(|| (*id, (column - inner.x, row - inner.y)))
    })
}

/// Pane-relative cell, clamped into the pane so a drag past an edge still
/// selects the last row or column.
fn clamped_cell(rect: Rect, column: u16, row: u16) -> (u16, u16) {
    let inner = inner_area(rect);
    let last_column = inner.width.saturating_sub(1);
    let last_row = inner.height.saturating_sub(1);
    (
        column.saturating_sub(inner.x).min(last_column),
        row.saturating_sub(inner.y).min(last_row),
    )
}

/// The text grid inside a pane's border.
fn inner_area(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    )
}

/// Turns terminal mouse reporting on or off after a settings change.
fn set_mouse_capture(term: &mut Term, enabled: bool) -> Result<()> {
    if enabled {
        execute!(term.backend_mut(), event::EnableMouseCapture)?;
    } else {
        execute!(term.backend_mut(), event::DisableMouseCapture)?;
    }
    Ok(())
}

/// Pane width for a terminal width and sidebar state.
pub fn pane_cols(cols: u16, sidebar: bool) -> u16 {
    cols.saturating_sub(render::sidebar_width(sidebar)).max(1)
}

/// Prompt text for closing a tab, or `None` when no pane in it is working.
fn close_tab_prompt(tab: &SidebarTabInfo) -> Option<String> {
    let working = tab
        .agents
        .iter()
        .filter(|agent| agent.state == AgentStateKind::Working)
        .count();
    (working > 0).then(|| {
        format!(
            "close tab \"{}\" with {working} working {}? y/n",
            tab.name,
            plural(working)
        )
    })
}

/// Prompt text for closing a workspace, or `None` when nothing is active.
fn close_workspace_prompt(workspace: &WorkspaceInfo) -> Option<String> {
    let active = workspace
        .tabs
        .iter()
        .flat_map(|tab| &tab.agents)
        .filter(|agent| {
            matches!(
                agent.state,
                AgentStateKind::Working | AgentStateKind::Blocked
            )
        })
        .count();
    (active > 0).then(|| {
        format!(
            "close workspace \"{}\" with {active} active {}? y/n",
            workspace.name,
            plural(active)
        )
    })
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        "agent"
    } else {
        "agents"
    }
}

/// Parse a `prefix W` prompt of `NAME [PATH]` into a workspace name and root.
/// A lone path-like token is taken as the path; `~` expands to `home`; the path
/// defaults to the focused pane's cwd and the name to the path basename.
fn parse_workspace_prompt(
    input: &str,
    focused_cwd: Option<&Path>,
    home: Option<&Path>,
) -> (String, Option<PathBuf>) {
    let mut parts = input.split_whitespace();
    let first = parts.next();
    let second = parts.next();
    let is_pathish = |token: &str| token.starts_with('~') || token.contains('/');
    let (name_token, path_token) = match (first, second) {
        (Some(name), Some(path)) => (Some(name), Some(path)),
        (Some(one), None) if is_pathish(one) => (None, Some(one)),
        (Some(one), None) => (Some(one), None),
        (None, _) => (None, None),
    };
    let root = path_token
        .map(|token| expand_tilde(token, home))
        .or_else(|| focused_cwd.map(PathBuf::from));
    let name = name_token
        .map(str::to_owned)
        .or_else(|| {
            root.as_deref()
                .and_then(Path::file_name)
                .and_then(|base| base.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "workspace".to_owned());
    (name, root)
}

/// Expand a leading `~` to the home directory; other paths pass through.
fn expand_tilde(token: &str, home: Option<&Path>) -> PathBuf {
    match (token.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if token == "~" => home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(token)),
        _ => PathBuf::from(token),
    }
}

// Selecting a sidebar row means focusing its workspace, tab, or pane.
fn sidebar_message(target: render::SidebarTarget) -> ClientMessage {
    match target {
        render::SidebarTarget::Workspace(id) => ClientMessage::SelectWorkspace { id },
        render::SidebarTarget::Tab(id) => ClientMessage::SelectTab { id },
        render::SidebarTarget::Pane(id) => ClientMessage::FocusPaneId { id },
    }
}

// One encoded message per socket write keeps the framing newline-delimited.
async fn write(writer: &mut OwnedWriteHalf, message: &ClientMessage) -> Result<()> {
    writer
        .write_all(&kodade_cli_proto::encode(message)?)
        .await?;
    Ok(())
}

/// Translates a key event into the bytes a PTY application expects.
pub fn bytes(k: KeyEvent) -> Option<Vec<u8>> {
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut b = match k.code {
        KeyCode::Char(c) if k.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![(c.to_ascii_lowercase() as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![27],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            format!("\x1b[{code}~").into_bytes()
        }
        _ => return None,
    };
    if alt {
        b.insert(0, 27)
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_cols_reserve_the_sidebar() {
        assert_eq!(
            pane_cols(100, true),
            100 - render::sidebar_width(true).min(100)
        );
        assert_eq!(pane_cols(1, true), 1);
    }

    fn agent(state: AgentStateKind) -> kodade_cli_proto::AgentInfo {
        kodade_cli_proto::AgentInfo {
            pane: PaneId(1),
            name: "claude".into(),
            state,
            state_age_secs: 0,
        }
    }

    #[test]
    fn close_prompts_appear_only_for_busy_agents() {
        let tab = |states: &[AgentStateKind]| SidebarTabInfo {
            id: kodade_cli_proto::TabId(2),
            name: "agents".into(),
            state: AgentStateKind::Idle,
            agents: states.iter().copied().map(agent).collect(),
        };
        assert_eq!(close_tab_prompt(&tab(&[AgentStateKind::Idle])), None);
        assert_eq!(
            close_tab_prompt(&tab(&[AgentStateKind::Working, AgentStateKind::Working])),
            Some("close tab \"agents\" with 2 working agents? y/n".into())
        );
        // Blocked panes do not block a tab close, but they do a workspace close.
        let workspace = WorkspaceInfo {
            id: kodade_cli_proto::WorkspaceId(1),
            name: "main".into(),
            active: true,
            state: AgentStateKind::Blocked,
            root: None,
            tabs: vec![tab(&[AgentStateKind::Blocked, AgentStateKind::Idle])],
        };
        assert_eq!(close_tab_prompt(&workspace.tabs[0]), None);
        assert_eq!(
            close_workspace_prompt(&workspace),
            Some("close workspace \"main\" with 1 active agent? y/n".into())
        );
    }

    #[test]
    fn workspace_prompt_parses_name_and_path() {
        let home = Path::new("/Users/keith");
        let cwd = Path::new("/Users/keith/src/repo");
        // NAME PATH, with ~ expansion.
        assert_eq!(
            parse_workspace_prompt("api ~/src/api", Some(cwd), Some(home)),
            (
                "api".to_owned(),
                Some(PathBuf::from("/Users/keith/src/api"))
            )
        );
        // Lone name → path defaults to the focused cwd.
        assert_eq!(
            parse_workspace_prompt("api", Some(cwd), Some(home)),
            (
                "api".to_owned(),
                Some(PathBuf::from("/Users/keith/src/repo"))
            )
        );
        // Lone path → name defaults to its basename.
        assert_eq!(
            parse_workspace_prompt("/tmp/thing", Some(cwd), Some(home)),
            ("thing".to_owned(), Some(PathBuf::from("/tmp/thing")))
        );
        // Empty → cwd basename as the name.
        assert_eq!(
            parse_workspace_prompt("", Some(cwd), Some(home)),
            (
                "repo".to_owned(),
                Some(PathBuf::from("/Users/keith/src/repo"))
            )
        );
    }

    #[test]
    fn pane_cells_skip_the_border_and_clamp_on_overshoot() {
        let rects = [(PaneId(1), Rect::new(0, 1, 20, 10))];
        // The border ring belongs to drag-resize, not to the text grid.
        assert_eq!(pane_cell_at(&rects, 0, 1), None);
        assert_eq!(pane_cell_at(&rects, 1, 2), Some((PaneId(1), (0, 0))));
        assert_eq!(pane_cell_at(&rects, 18, 9), Some((PaneId(1), (17, 7))));
        assert_eq!(pane_cell_at(&rects, 19, 9), None);
        // Dragging past an edge keeps selecting the last cell.
        assert_eq!(clamped_cell(rects[0].1, 200, 200), (17, 7));
        assert_eq!(clamped_cell(rects[0].1, 0, 0), (0, 0));
    }

    #[test]
    fn key_bytes_encode_control_and_alt() {
        assert_eq!(
            bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            bytes(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(vec![27, b'b'])
        );
        assert_eq!(
            bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
    }
}
