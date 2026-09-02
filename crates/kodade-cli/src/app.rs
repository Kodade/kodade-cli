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
    AgentStateKind, ClientMessage, Direction, LayoutSnapshot, Notification, PaneId, Screen,
    ServerMessage, SidebarTabInfo, SplitAxis, WorkspaceId, WorkspaceInfo,
};
use ratatui::{backend::CrosstermBackend, layout::Rect, style::Color, Frame, Terminal};
use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, net::unix::OwnedWriteHalf, sync::mpsc};

use crate::{
    config, help, input, mode, notify,
    overlay::{self, Overlay, OverlayEvent, OverlayTarget},
    paste,
    picker::{self, PickTarget},
    render,
    render::SidebarMode,
    selection::{self, Selection, SelectionMode},
    settings, state,
};

/// Two left clicks on the same cell inside this window are a double click (#12).
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// The 8 preset swatch colors the right-click `Color…` menu cycles through (#19).
const WORKSPACE_COLORS: [&str; 8] = [
    "#e7a33b", "#d95b5b", "#a8c87f", "#7aa2f7", "#bb9af7", "#7dcfff", "#e0af68", "#9ece6a",
];

/// How long a status-bar note stays up.
const NOTE_TTL: Duration = Duration::from_secs(5);
/// Pace a multi-chunk paste so the socket writer does not flood the daemon.
const PASTE_CHUNK_GAP: Duration = Duration::from_millis(5);
/// Notification toasts linger a little longer than ordinary notes (#10).
const TOAST_TTL: Duration = Duration::from_secs(6);

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Server messages the attached client acts on; the reader task drops the rest.
pub enum Update {
    Layout(LayoutSnapshot),
    Session(String),
    Notification(Notification),
    /// `session rename` moved the daemon's socket; the client follows it so
    /// one-shot requests (copy mode's `ReadPane`) keep working (#16).
    SessionRenamed {
        name: String,
        socket: PathBuf,
    },
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

/// A status-bar note (or notification toast) and how it is drawn.
struct Note {
    text: String,
    expiry: Instant,
    color: Color,
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
    /// The `prefix G` worktree prompt (branch name), also reusing `name` (#22).
    worktree_new: bool,
    /// Pending `remove worktree <branch>? y/n/k(eep)` prompt: (workspace, branch).
    worktree_confirm: Option<(WorkspaceId, String)>,
    name: String,
    rename_target: Option<mode::MenuTarget>,
    drag: Option<DragState>,
    /// Full / compact rail / hidden gutter (#19).
    sidebar_mode: SidebarMode,
    /// Workspaces the user collapsed in the sidebar list (#19).
    collapsed: HashSet<WorkspaceId>,
    /// Persisted UI state (collapsed workspaces, help_seen); loaded once (#19).
    ui_state: state::State,
    /// Workspace ids already reconciled against the persisted collapse set, so a
    /// newly appearing workspace is seeded but existing ones are left alone (#19).
    seeded_ids: HashSet<WorkspaceId>,
    /// True when auto-hide collapsed the sidebar, so widening restores it (#19).
    auto_hidden: bool,
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
    /// Workspace switcher (`prefix w`) or goto palette (`prefix g`); holds its
    /// item list so the filter rebuilds without re-reading the snapshot (#17).
    picker: Option<picker::Picker>,
    /// Help overlay (`prefix ?`); holds the full row set so its filter can
    /// rebuild without re-reading the config (#6).
    help: Option<help::HelpOverlay>,
    /// Whether the help overlay has ever been opened; drives the first-attach
    /// hint and is persisted to the state file (#6).
    help_seen: bool,
    /// Status-bar note and the instant it stops being shown.
    note: Option<Note>,
    /// Last sanitized paste, copy-mode yank, or mouse selection; re-sent by
    /// the `paste_buffer` action (#21).
    paste_buffer: String,
    /// Agent notifications: unread stack and effect computation (#10).
    notifier: notify::Notifier,
    /// Session reported by the daemon's `Welcome`; shown in the status bar (#11).
    session_name: String,
    /// Socket the client is attached to (local or an SSH-forwarded one for
    /// `--remote`); used for one-shot requests like copy-mode reads (#23).
    socket: PathBuf,
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
    pub fn new(config: &config::Config, session: &str, socket: PathBuf) -> Self {
        // The toast tells the user which chord jumps to the pane, so read the
        // live binding (defaults to `N` per #14's o/O collision).
        let jump_hint = config
            .chords_for(config::Action::NotificationJump)
            .into_iter()
            .next()
            .unwrap_or_else(|| "N".to_string());
        Self {
            layout: None,
            prefix: false,
            rename: false,
            new_workspace: false,
            worktree_new: false,
            worktree_confirm: None,
            name: String::new(),
            rename_target: None,
            drag: None,
            sidebar_mode: if config.sidebar {
                SidebarMode::Full
            } else {
                config_collapsed_mode(config)
            },
            collapsed: HashSet::new(),
            ui_state: state::State::load(),
            seeded_ids: HashSet::new(),
            auto_hidden: false,
            navigate: None,
            copy: None,
            menu: None,
            confirm: None,
            resize: false,
            focused_pane: None,
            last_pane: None,
            settings: None,
            picker: None,
            help: None,
            help_seen: help::state_seen(),
            note: None,
            paste_buffer: String::new(),
            notifier: notify::Notifier::new(config, jump_hint),
            session_name: session.to_string(),
            socket,
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

    /// Sets the status-bar note in the default color; clears after `NOTE_TTL`.
    fn set_note(&mut self, text: impl Into<String>) {
        let color = self.theme.done;
        self.set_note_full(text, NOTE_TTL, color);
    }

    /// Sets a note with an explicit lifetime and color (notification toasts).
    fn set_note_full(&mut self, text: impl Into<String>, ttl: Duration, color: Color) {
        self.note = Some(Note {
            text: text.into(),
            expiry: Instant::now() + ttl,
            color,
        });
    }

    // The note text and color, unless it has expired.
    fn note(&self) -> Option<(&str, Color)> {
        self.note
            .as_ref()
            .filter(|note| note.expiry > Instant::now())
            .map(|note| (note.text.as_str(), note.color))
    }

    /// Effective sidebar width for the current mode and config.
    fn sidebar_width(&self) -> u16 {
        render::sidebar_width(self.sidebar_mode, &self.config)
    }

    /// Collapse a full sidebar under the auto-hide column threshold, and restore
    /// it once the terminal is wide enough again (#19).
    pub fn apply_auto_hide(&mut self, cols: u16) {
        let below = cols < self.config.sidebar_auto_hide_below;
        if below && self.sidebar_mode == SidebarMode::Full {
            self.sidebar_mode = config_collapsed_mode(&self.config);
            self.auto_hidden = true;
            if self.sidebar_mode == SidebarMode::Hidden {
                self.sidebar_hidden_at = Some(Instant::now());
            }
        } else if !below && self.auto_hidden {
            self.sidebar_mode = SidebarMode::Full;
            self.auto_hidden = false;
        }
    }

    /// Pane width for the current sidebar state, used by `Hello` and `Resize`.
    pub fn pane_cols(&self, cols: u16) -> u16 {
        pane_cols(cols, self.sidebar_width())
    }

    /// Stores a new snapshot. Copy mode refreshes its full-history buffer
    /// separately (throttled) in the event loop, not from the visible screen.
    pub fn handle_layout(&mut self, layout: LayoutSnapshot) {
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
        self.seed_collapsed();
    }

    /// Map persisted collapsed workspace names to ids so the sidebar reopens
    /// with the same workspaces folded. Runs every snapshot but only seeds ids
    /// it has not seen yet, so a workspace that first appears later is still
    /// reconciled while ones the user has since toggled are left alone (#19).
    fn seed_collapsed(&mut self) {
        let Some(layout) = &self.layout else { return };
        let ids: HashSet<WorkspaceId> = layout.workspaces.iter().map(|w| w.id).collect();
        let names = self.ui_state.collapsed_for(&self.session_name);
        for workspace in &layout.workspaces {
            if self.seeded_ids.insert(workspace.id) && names.contains(&workspace.name) {
                self.collapsed.insert(workspace.id);
            }
        }
        // Drop bookkeeping for workspaces that have gone away.
        self.seeded_ids.retain(|id| ids.contains(id));
        self.collapsed.retain(|id| ids.contains(id));
    }

    /// Persist the current collapsed set as workspace names for this session.
    fn persist_collapsed(&mut self) {
        let Some(layout) = &self.layout else { return };
        let names = layout
            .workspaces
            .iter()
            .filter(|workspace| self.collapsed.contains(&workspace.id))
            .map(|workspace| workspace.name.clone())
            .collect();
        // Reload first so a concurrently written `help_seen` (#6) is preserved.
        self.ui_state = state::State::load();
        self.ui_state.set_collapsed(&self.session_name, names);
    }

    pub fn handle_session(&mut self, session: String) {
        self.session_name = session;
    }

    /// Applies one agent notification: computes its effects and performs them
    /// (toast, bell, system OSC, sound). Never blocks the render loop.
    fn handle_notification(&mut self, notification: Notification, term: &mut Term) -> Result<()> {
        let effects = {
            let Some(layout) = self.layout.as_ref() else {
                return Ok(());
            };
            self.notifier.handle(&notification, layout)
        };
        let color = note_color(&self.theme, notification.state);
        for effect in effects {
            match effect {
                notify::Effect::Toast(text) => self.set_note_full(text, TOAST_TTL, color),
                notify::Effect::Bell => {
                    execute!(term.backend_mut(), crossterm::style::Print("\x07"))?;
                }
                notify::Effect::Osc777 { title, body } => {
                    // OSC 777 (rxvt/foot) and OSC 9 (iTerm2/WezTerm) cover the
                    // common desktop-notification escapes.
                    execute!(
                        term.backend_mut(),
                        crossterm::style::Print(format!("\x1b]777;notify;{title};{body}\x07")),
                        crossterm::style::Print(format!("\x1b]9;{body}\x07")),
                    )?;
                }
                notify::Effect::Sound(command) => spawn_sound(&command),
            }
        }
        Ok(())
    }

    /// `prefix N`: focus the pane of the most recent unread notification and
    /// mark it read; an empty stack just says so.
    async fn notification_jump(&mut self, writer: &mut OwnedWriteHalf) -> Result<()> {
        match self.notifier.pop_unread() {
            Some(notification) => {
                write(
                    writer,
                    &ClientMessage::FocusPaneId {
                        id: notification.pane,
                    },
                )
                .await?;
            }
            None => self.set_note(" no notifications"),
        }
        Ok(())
    }

    /// Fetch a pane's full scrollback + screen as plain lines over a one-shot
    /// daemon connection (the copy-mode buffer). Returns `None` on any error.
    async fn fetch_pane_lines(&self, pane: PaneId) -> Option<Vec<String>> {
        let reply = crate::commands::request(
            &self.socket,
            ClientMessage::ReadPane {
                id: pane,
                scrollback: true,
                lines: None,
            },
        )
        .await
        .ok()?;
        match reply {
            ServerMessage::PaneText { text, .. } => {
                Some(text.split('\n').map(str::to_string).collect())
            }
            _ => None,
        }
    }

    /// Refresh the copy buffer if enough time has passed since the last fetch.
    async fn refresh_copy(&mut self) {
        let Some(cm) = &self.copy else { return };
        if cm.refreshed_at.elapsed() < Duration::from_millis(500) {
            return;
        }
        let (pane, height) = (cm.pane, cm.height);
        if let Some(lines) = self.fetch_pane_lines(pane).await {
            if let Some(cm) = &mut self.copy {
                cm.refresh(lines, height);
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let Some(layout) = &self.layout else { return };
        // Both hints are generated from the live bindings so remaps show through.
        let prefix_hint = help::prefix_hint(&self.config);
        let attach = help::attach_hint(&self.config);
        let first_attach_hint = (!self.help_seen).then_some(attach.as_str());
        // The worktree removal prompt reuses the confirm status line (#22).
        let worktree_prompt = self
            .worktree_confirm
            .as_ref()
            .map(|(_, branch)| format!("remove worktree {branch}? y/n/k(eep)"));
        let confirm = worktree_prompt
            .as_deref()
            .or_else(|| self.confirm.as_ref().map(|c| c.message.as_str()));
        render::render(
            frame,
            layout,
            &render::Ui {
                sidebar_mode: self.sidebar_mode,
                sidebar_width: self.sidebar_width(),
                collapsed: &self.collapsed,
                agents_panel: self.config.sidebar_agents_panel,
                prefix: self.prefix,
                rename: self.rename,
                new_workspace: self.new_workspace,
                worktree_new: self.worktree_new,
                name: &self.name,
                navigate: self.navigate,
                copy: self.copy.as_ref(),
                menu: self.menu.as_ref(),
                resize: self.resize,
                confirm,
                settings: self.settings.as_ref(),
                note: self.note(),
                session: &self.session_name,
                status_right: &self.config.status_right,
                flash: self.flash_active(),
                sidebar_hint: self.sidebar_hint_active(),
                selection: self.selection.as_ref(),
                prefix_hint: &prefix_hint,
                first_attach_hint,
                help: self.help.as_ref().map(|state| &state.overlay),
                picker: self.picker.as_ref(),
            },
            &self.theme,
        )
    }

    // Opens the help overlay and records that help has been seen.
    fn open_help(&mut self) {
        self.help = Some(help::overlay(&self.config));
        if !self.help_seen {
            self.help_seen = true;
            help::mark_seen();
        }
    }

    /// Whether the `prefix q` pane-id flash is still showing.
    fn flash_active(&self) -> bool {
        self.flash_until.is_some_and(|until| Instant::now() < until)
    }

    /// Whether the timed `prefix b · sidebar` hint should show (sidebar hidden).
    fn sidebar_hint_active(&self) -> bool {
        self.sidebar_mode == SidebarMode::Hidden
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
            let mut layout_changed = false;
            while let Ok(update) = rx.try_recv() {
                match update {
                    Update::Layout(layout) => {
                        self.handle_layout(layout);
                        layout_changed = true;
                    }
                    Update::Session(session) => self.handle_session(session),
                    Update::Notification(notification) => {
                        self.handle_notification(notification, term)?
                    }
                    Update::SessionRenamed { name, socket } => {
                        self.handle_session(name);
                        self.socket = socket;
                    }
                }
            }
            // Refetch the copy-mode buffer when the pane produced new output,
            // throttled so a busy pane does not flood the socket.
            if layout_changed {
                self.refresh_copy().await;
            }
            self.sync_title(term)?;
            term.draw(|frame| self.draw(frame))?;
            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            match event::read()? {
                Event::Resize(cols, rows) => {
                    self.apply_auto_hide(cols);
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
        if self.worktree_confirm.is_some() {
            self.handle_worktree_confirm_key(key, writer).await?;
        } else if self.confirm.is_some() {
            self.handle_confirm_key(key, writer).await?;
        } else if self.rename {
            self.handle_rename_key(key, writer).await?;
        } else if self.new_workspace {
            self.handle_new_workspace_key(key, writer).await?;
        } else if self.worktree_new {
            self.handle_worktree_new_key(key, writer).await?;
        } else if self.copy.is_some() {
            self.handle_copy_key(key, writer, term).await?;
        } else if self.menu.is_some() {
            self.handle_menu_key(key, writer).await?;
        } else if self.help.is_some() {
            self.handle_help_key(key);
        } else if self.settings.is_some() {
            self.handle_settings_key(key, writer, term).await?;
        } else if self.picker.is_some() {
            self.handle_picker_key(key, writer).await?;
        } else if let Some(current) = self.navigate {
            self.handle_navigate_key(key, current, writer, term).await?;
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

    // Worktree prompt: type `BRANCH [FROM]`, enter opens a worktree workspace on
    // the active workspace's repo (#22).
    async fn handle_worktree_new_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        match key.code {
            KeyCode::Enter => {
                let input = std::mem::take(&mut self.name);
                self.worktree_new = false;
                let mut parts = input.split_whitespace();
                let Some(branch) = parts.next().map(str::to_owned) else {
                    return Ok(());
                };
                let from = parts.next().map(str::to_owned);
                // The repo comes from the active workspace's root directory.
                let repo_root = self
                    .active_workspace()
                    .and_then(|workspace| workspace.root.clone());
                match repo_root {
                    Some(repo_root) => {
                        write(
                            writer,
                            &ClientMessage::NewWorktreeWorkspace {
                                repo_root,
                                branch,
                                from,
                            },
                        )
                        .await?;
                    }
                    None => self.set_note(" active workspace has no repo to branch"),
                }
            }
            KeyCode::Esc => {
                self.name.clear();
                self.worktree_new = false;
            }
            KeyCode::Backspace => {
                self.name.pop();
            }
            KeyCode::Char(c) => self.name.push(c),
            _ => {}
        }
        Ok(())
    }

    // `remove worktree <branch>? y/n/k(eep)`: `y` removes the directory, `k`
    // keeps it, anything else cancels (#22).
    async fn handle_worktree_confirm_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let (id, _) = self
            .worktree_confirm
            .take()
            .expect("worktree confirm exists");
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                write(
                    writer,
                    &ClientMessage::RemoveWorktreeWorkspace { id, keep: false },
                )
                .await?;
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                write(
                    writer,
                    &ClientMessage::RemoveWorktreeWorkspace { id, keep: true },
                )
                .await?;
            }
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

    // Copy mode: vi motions over full scrollback, `/`?` search, `v`/`V`/`ctrl+v`
    // selection, `y` copies via OSC 52, `e` opens the buffer in an editor pane.
    async fn handle_copy_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        let Some(mut cm) = self.copy.take() else {
            return Ok(());
        };

        // A live `/`?` prompt swallows input until Enter/Esc.
        if let Some(mut prompt) = cm.prompt.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    cm.set_search(&prompt.input, prompt.forward);
                    cm.search_jump(prompt.forward);
                }
                KeyCode::Backspace => {
                    prompt.input.pop();
                    cm.prompt = Some(prompt);
                }
                KeyCode::Char(c) => {
                    prompt.input.push(c);
                    cm.prompt = Some(prompt);
                }
                _ => cm.prompt = Some(prompt),
            }
            self.copy = Some(cm);
            return Ok(());
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Any key but a second `g` cancels a pending `gg`.
        let pending_g = cm.pending_g;
        cm.pending_g = false;
        let mut keep = true;

        match key.code {
            // Esc peels off search highlights, then a selection, then exits.
            KeyCode::Esc => {
                if cm.search.is_some() {
                    cm.clear_search();
                } else if cm.anchor.is_some() {
                    cm.anchor = None;
                } else {
                    keep = false;
                }
            }
            KeyCode::Char('q') => keep = false,

            // Ctrl chords first, so plain letters below stay reachable.
            KeyCode::Char('v') if ctrl => {
                cm.select = mode::SelectKind::Block;
                cm.anchor = Some(cm.cursor);
            }
            KeyCode::Char('b') if ctrl => cm.page(false),
            KeyCode::Char('f') if ctrl => cm.page(true),
            KeyCode::Char('u') if ctrl => cm.half_page(false),
            KeyCode::Char('d') if ctrl => cm.half_page(true),

            // Selection anchors.
            KeyCode::Char('v') => {
                cm.select = mode::SelectKind::Char;
                cm.anchor = Some(cm.cursor);
            }
            KeyCode::Char('V') => {
                cm.select = mode::SelectKind::Line;
                cm.anchor = Some(cm.cursor);
            }

            // Yank the selection (or current line) through OSC 52.
            KeyCode::Char('y') => {
                let text = cm.yank_text();
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

            // Open the buffer in an editor, in a NEW pane — never the agent's.
            KeyCode::Char('e') => {
                if self.open_in_editor(&cm, writer).await? {
                    keep = false;
                }
            }

            // Search.
            KeyCode::Char('/') => {
                cm.prompt = Some(mode::Prompt {
                    forward: true,
                    input: String::new(),
                })
            }
            KeyCode::Char('?') => {
                cm.prompt = Some(mode::Prompt {
                    forward: false,
                    input: String::new(),
                })
            }
            KeyCode::Char('n') => {
                if let Some(forward) = cm.search.as_ref().map(|s| s.forward) {
                    cm.search_jump(forward);
                }
            }
            KeyCode::Char('N') => {
                if let Some(forward) = cm.search.as_ref().map(|s| s.forward) {
                    cm.search_jump(!forward);
                }
            }

            // Character / line motions.
            KeyCode::Up | KeyCode::Char('k') => cm.move_rows(-1),
            KeyCode::Down | KeyCode::Char('j') => cm.move_rows(1),
            KeyCode::Left | KeyCode::Char('h') => cm.move_cols(-1),
            KeyCode::Right | KeyCode::Char('l') => cm.move_cols(1),
            KeyCode::Char('0') => cm.goto_line_start(),
            KeyCode::Char('$') => cm.goto_line_end(),
            KeyCode::Char('^') => cm.goto_first_nonblank(),

            // Word motions (`e` is the editor, so word-end is `E`, WORD-wise).
            KeyCode::Char('w') => cm.next_word(false),
            KeyCode::Char('W') => cm.next_word(true),
            KeyCode::Char('b') => cm.prev_word(false),
            KeyCode::Char('B') => cm.prev_word(true),
            KeyCode::Char('E') => cm.end_word(true),

            // Paragraph and buffer jumps.
            KeyCode::Char('{') => cm.paragraph(false),
            KeyCode::Char('}') => cm.paragraph(true),
            KeyCode::Char('g') => {
                if pending_g {
                    cm.goto_top();
                } else {
                    cm.pending_g = true;
                }
            }
            KeyCode::Char('G') => cm.goto_bottom(),

            // Viewport-relative and paged movement.
            KeyCode::Char('H') => cm.viewport_top(),
            KeyCode::Char('M') => cm.viewport_middle(),
            KeyCode::Char('L') => cm.viewport_bottom(),
            KeyCode::PageUp => cm.page(false),
            KeyCode::PageDown => cm.page(true),
            _ => {}
        }
        if keep {
            self.copy = Some(cm);
        }
        Ok(())
    }

    /// Write the copy buffer to a temp file and open `$EDITOR` (fallback `vi`)
    /// in a new vertical split. Returns whether the editor pane was requested.
    async fn open_in_editor(
        &mut self,
        cm: &mode::CopyMode,
        writer: &mut OwnedWriteHalf,
    ) -> Result<bool> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("kodade-cli-{}-{}.txt", cm.pane.0, ts));
        if std::fs::write(&path, cm.lines.join("\n")).is_err() {
            self.set_note(" could not write copy-mode temp file");
            return Ok(false);
        }
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        write(
            writer,
            &ClientMessage::NewPane {
                workspace: None,
                tab: None,
                split: Some(SplitAxis::Vertical),
                command: Some(vec![editor, path.to_string_lossy().into_owned()]),
                name: Some("editor".into()),
            },
        )
        .await?;
        Ok(true)
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

    // Navigate mode: move through selectable sidebar rows (headings skipped),
    // toggle a workspace's collapse on enter, or activate a tab/pane.
    async fn handle_navigate_key(
        &mut self,
        key: KeyEvent,
        current: usize,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        let rows = self.sidebar_flat();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.navigate = None;
                if !self.config.sidebar {
                    // Restoring the collapsed shape shrinks the sidebar, so tell
                    // the daemon the new pane width.
                    self.sidebar_mode = config_collapsed_mode(&self.config);
                    self.send_resize(writer, term).await?;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.navigate = self.move_selectable(&rows, current, true);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.navigate = self.move_selectable(&rows, current, false);
            }
            // `*` expands every workspace.
            KeyCode::Char('*') => {
                self.collapsed.clear();
                self.persist_collapsed();
            }
            KeyCode::Enter => {
                let Some(target) = rows.get(current).and_then(|row| row.target.clone()) else {
                    return Ok(());
                };
                match target {
                    // Enter on a workspace folds/unfolds it and selects it,
                    // staying in navigate so the user can keep moving.
                    render::SidebarTarget::Workspace(id) => {
                        self.toggle_collapsed(id);
                        write(writer, &ClientMessage::SelectWorkspace { id }).await?;
                    }
                    other => {
                        self.activate_sidebar(writer, other).await?;
                        self.navigate = None;
                    }
                }
            }
            // `?` opens the help overlay from navigate mode (#6).
            KeyCode::Char('?') => {
                self.navigate = None;
                self.open_help();
            }
            _ => {}
        }
        Ok(())
    }

    /// Flat sidebar rows (workspaces then agents) for the current layout.
    fn sidebar_flat(&self) -> Vec<render::SidebarRow> {
        match &self.layout {
            Some(layout) => {
                render::sidebar_rows(layout, &self.collapsed, self.config.sidebar_agents_panel)
                    .into_flat()
            }
            None => Vec::new(),
        }
    }

    /// First selectable (non-heading) row index, if any.
    fn first_selectable_row(&self) -> Option<usize> {
        self.sidebar_flat()
            .iter()
            .position(|row| row.target.is_some())
    }

    /// Next selectable row from `current` in the given direction, skipping
    /// headings and clamping at the ends.
    fn move_selectable(
        &self,
        rows: &[render::SidebarRow],
        current: usize,
        up: bool,
    ) -> Option<usize> {
        let mut index = current;
        loop {
            index = if up {
                match index.checked_sub(1) {
                    Some(i) => i,
                    None => return Some(current),
                }
            } else {
                let next = index + 1;
                if next >= rows.len() {
                    return Some(current);
                }
                next
            };
            if rows.get(index).is_some_and(|row| row.target.is_some()) {
                return Some(index);
            }
        }
    }

    /// Fold or unfold a workspace in the sidebar and persist the change.
    fn toggle_collapsed(&mut self, id: WorkspaceId) {
        if !self.collapsed.insert(id) {
            self.collapsed.remove(&id);
        }
        self.persist_collapsed();
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
            // An unbound key after the prefix: nudge toward the help overlay.
            let chord = config::render_chord(config::normalize_key(key));
            self.set_note(format!(" unbound: {chord} · prefix ? for help"));
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
                // Cycle full → compact → hidden → full (#19). A deliberate choice
                // clears the auto-hide flag so a later widening resize does not
                // override it.
                self.sidebar_mode = self.sidebar_mode.next();
                self.auto_hidden = false;
                if self.sidebar_mode == SidebarMode::Hidden {
                    self.sidebar_hidden_at = Some(Instant::now());
                }
                self.send_resize(writer, term).await?;
            }
            config::Action::ReloadConfig => self.reload_config(term)?,
            config::Action::Settings => {
                self.settings = Some(settings::overlay(&self.config, 0));
            }
            config::Action::Help => self.open_help(),
            config::Action::WorkspacePicker => {
                if let Some(layout) = &self.layout {
                    self.picker = Some(picker::Picker::new(
                        "workspaces · type to filter · esc closes",
                        picker::workspace_items(layout),
                    ));
                }
            }
            config::Action::Goto => {
                if let Some(layout) = &self.layout {
                    self.picker = Some(picker::Picker::new(
                        "go to · type to filter · esc closes",
                        picker::goto_items(layout),
                    ));
                }
            }
            config::Action::Navigate => {
                // Opening navigate forces the full sidebar; tell the daemon the
                // new pane width when that actually widened the sidebar.
                let widened = self.sidebar_mode != SidebarMode::Full;
                self.sidebar_mode = SidebarMode::Full;
                self.auto_hidden = false;
                self.navigate = self.first_selectable_row();
                if widened {
                    self.send_resize(writer, term).await?;
                }
            }
            config::Action::CopyMode => {
                // Freeze the focused pane's full history; viewport height comes
                // from its visible row count.
                let target = self
                    .layout
                    .as_ref()
                    .and_then(|layout| layout.panes.iter().find(|pane| pane.focused))
                    .map(|pane| (pane.id, pane.screen.rows.len().max(1)));
                if let Some((id, height)) = target {
                    let lines = self.fetch_pane_lines(id).await.unwrap_or_default();
                    self.copy = Some(mode::CopyMode::new(id, lines, height));
                }
            }
            config::Action::Rename => self.rename = true,
            config::Action::NewWorkspace => self.new_workspace = true,
            config::Action::WorktreeNew => self.worktree_new = true,
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
                // A worktree workspace gets the y/n/k(eep) removal prompt instead
                // of the plain close confirmation (#22).
                let worktree = self
                    .active_workspace()
                    .filter(|workspace| workspace.parent.is_some())
                    .map(|workspace| {
                        (
                            workspace.id,
                            workspace
                                .branch
                                .clone()
                                .unwrap_or_else(|| workspace.name.clone()),
                        )
                    });
                if let Some(worktree) = worktree {
                    self.worktree_confirm = Some(worktree);
                } else if let Some(prompt) = self.active_workspace().map(|workspace| {
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
            config::Action::NotificationJump => self.notification_jump(writer).await?,
            other => {
                if let Some(message) = other.message() {
                    write(writer, &message).await?
                }
            }
        }
        Ok(Flow::Continue)
    }

    // Help overlay: esc / q / ? close; anything else filters the row list.
    fn handle_help_key(&mut self, key: KeyEvent) {
        let Some(mut state) = self.help.take() else {
            return;
        };
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        // `?` always closes, and `q` closes when the filter box is empty so the
        // muscle-memory close key still works before any typing.
        let close = matches!(key.code, KeyCode::Esc)
            || (plain && key.code == KeyCode::Char('?'))
            || (plain
                && key.code == KeyCode::Char('q')
                && state
                    .overlay
                    .filter
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty());
        if close {
            return;
        }
        if overlay::overlay_key(&mut state.overlay, key) == OverlayEvent::Filtered {
            state.apply_filter();
        }
        self.help = Some(state);
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

    // Picker overlay (`prefix w` / `prefix g`): esc closes, enter activates the
    // highlighted row, anything else filters the list.
    async fn handle_picker_key(
        &mut self,
        key: KeyEvent,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        let Some(mut picker) = self.picker.take() else {
            return Ok(());
        };
        match overlay::overlay_key(&mut picker.overlay, key) {
            OverlayEvent::Cancel => return Ok(()),
            OverlayEvent::Select => {
                if let Some(target) = picker.current_target() {
                    self.activate_pick(target, writer).await?;
                    // Selecting closes the picker.
                    return Ok(());
                }
                self.picker = Some(picker);
            }
            OverlayEvent::Filtered => {
                picker.apply_filter();
                self.picker = Some(picker);
            }
            _ => self.picker = Some(picker),
        }
        Ok(())
    }

    // Focuses the workspace, tab, or pane behind a picker row. A tab first
    // activates its owning workspace, since it may live in an inactive one.
    async fn activate_pick(
        &mut self,
        target: PickTarget,
        writer: &mut OwnedWriteHalf,
    ) -> Result<()> {
        match target {
            PickTarget::Workspace(id) => {
                write(writer, &ClientMessage::SelectWorkspace { id }).await?;
            }
            PickTarget::Tab(id) => {
                if let Some(workspace) = self.workspace_of_tab(id) {
                    write(writer, &ClientMessage::SelectWorkspace { id: workspace }).await?;
                }
                write(writer, &ClientMessage::SelectTab { id }).await?;
            }
            PickTarget::Pane(id) => {
                write(writer, &ClientMessage::FocusPaneId { id }).await?;
            }
        }
        Ok(())
    }

    // The workspace that owns `tab` in the newest snapshot, if any.
    fn workspace_of_tab(
        &self,
        tab: kodade_cli_proto::TabId,
    ) -> Option<kodade_cli_proto::WorkspaceId> {
        self.layout
            .as_ref()?
            .workspaces
            .iter()
            .find_map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|candidate| candidate.id == tab)
                    .then_some(workspace.id)
            })
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
                // A deliberate toggle clears auto-hide so a later resize keeps it.
                self.sidebar_mode = if self.config.sidebar {
                    SidebarMode::Full
                } else {
                    config_collapsed_mode(&self.config)
                };
                self.auto_hidden = false;
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
        // The picker owns the mouse the same way: a click selects a row, a
        // click outside closes it.
        if self.picker.is_some() {
            return self.picker_mouse(mouse, writer, term).await;
        }
        // The help overlay swallows mouse input: a click outside it closes it,
        // and clicks inside do nothing (its rows are not actionable).
        if self.help.is_some() {
            if let MouseEventKind::Down(_) = mouse.kind {
                let area = term.size().map(|s| Rect::new(0, 0, s.width, s.height))?;
                let inside = self.help.as_ref().is_some_and(|state| {
                    overlay::contains(area, &state.overlay, mouse.column, mouse.row)
                });
                if !inside {
                    self.help = None;
                }
            }
            return Ok(());
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
        let content_area = render::content_area(frame_area, self.sidebar_mode, &self.config);
        // A click inside the sidebar column stays local to the sidebar (#19).
        if mouse.column < content_area.x {
            match self.sidebar_mode {
                SidebarMode::Full => {
                    let layout = self.layout.as_ref().expect("layout present");
                    let model = render::sidebar_rows(
                        layout,
                        &self.collapsed,
                        self.config.sidebar_agents_panel,
                    );
                    let place = render::sidebar_layout(size.height, &model, self.navigate);
                    if let Some((_, row)) = render::sidebar_row_at(&model, &place, mouse.row) {
                        if let Some(target) = row.target.clone() {
                            write(writer, &sidebar_message(target)).await?;
                        }
                    }
                }
                SidebarMode::Compact => {
                    // A click on a rail dot selects that workspace.
                    let layout = self.layout.as_ref().expect("layout present");
                    if let Some(id) = render::rail_workspace_at(layout, mouse.row) {
                        write(writer, &ClientMessage::SelectWorkspace { id }).await?;
                    }
                }
                SidebarMode::Hidden => {
                    self.sidebar_mode = SidebarMode::Full;
                    self.send_resize(writer, term).await?;
                }
            }
            return Ok(());
        }
        let current = self.layout.as_ref().expect("layout present");
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

    // Picker clicks: a left click on a row selects it, anything else closes.
    async fn picker_mouse(
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
        let overlay = &self.picker.as_ref().expect("picker open").overlay;
        let row = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            .then(|| overlay::row_at(area, overlay, mouse.column, mouse.row))
            .flatten();
        match row {
            Some(index) => {
                if let Some(picker) = &mut self.picker {
                    picker.overlay.selected = index;
                }
                // Reuse the enter path so a click and a keypress behave alike.
                self.handle_picker_key(KeyEvent::from(KeyCode::Enter), writer)
                    .await?;
            }
            None => self.picker = None,
        }
        Ok(())
    }

    // Right click: open the context menu for the pane or sidebar row under it.
    fn mouse_right_down(&mut self, mouse: MouseEvent, term: &mut Term) -> Result<()> {
        let size = term.size()?;
        let content = render::content_area(
            Rect::new(0, 0, size.width, size.height),
            self.sidebar_mode,
            &self.config,
        );
        let in_sidebar = mouse.column < content.x;
        let target = if in_sidebar && self.sidebar_mode == SidebarMode::Full {
            let layout = self.layout.as_ref().expect("layout present");
            let model =
                render::sidebar_rows(layout, &self.collapsed, self.config.sidebar_agents_panel);
            let place = render::sidebar_layout(size.height, &model, self.navigate);
            render::sidebar_row_at(&model, &place, mouse.row)
                .and_then(|(_, row)| row.target.clone())
                .map(|target| match target {
                    render::SidebarTarget::Workspace(id) => mode::MenuTarget::Workspace(id),
                    render::SidebarTarget::Tab(id) => mode::MenuTarget::Tab(id),
                    render::SidebarTarget::Pane(id) => mode::MenuTarget::Pane(id),
                })
        } else if in_sidebar && self.sidebar_mode == SidebarMode::Compact {
            let layout = self.layout.as_ref().expect("layout present");
            render::rail_workspace_at(layout, mouse.row).map(mode::MenuTarget::Workspace)
        } else {
            let current = self.layout.as_ref().expect("layout present");
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
            mode::MenuAction::Color => {
                // Cycle the workspace swatch through the 8 presets.
                if let mode::MenuTarget::Workspace(id) = menu.target {
                    let color = self.next_workspace_color(id);
                    write(writer, &ClientMessage::SetWorkspaceColor { id, color }).await?;
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
            mode::MenuAction::Help => self.open_help(),
        }
        Ok(())
    }

    /// The next preset swatch color for a workspace, cycling from its current
    /// one (or the first preset when it has none / an off-list color).
    fn next_workspace_color(&self, id: WorkspaceId) -> Option<String> {
        let current = self
            .layout
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .and_then(|workspace| workspace.color.as_deref());
        let index = current
            .and_then(|hex| WORKSPACE_COLORS.iter().position(|preset| *preset == hex))
            .map(|position| (position + 1) % WORKSPACE_COLORS.len())
            .unwrap_or(0);
        Some(WORKSPACE_COLORS[index].to_string())
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
            self.sidebar_mode,
            &self.config,
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

/// Pane width for a terminal width and effective sidebar width.
pub fn pane_cols(cols: u16, sidebar_width: u16) -> u16 {
    cols.saturating_sub(sidebar_width).max(1)
}

/// The sidebar mode a collapsed/hidden config maps to when the sidebar is not
/// shown at startup (or after auto-hide).
fn config_collapsed_mode(config: &config::Config) -> SidebarMode {
    match config.sidebar_collapsed {
        config::CollapsedMode::Compact => SidebarMode::Compact,
        config::CollapsedMode::Hidden => SidebarMode::Hidden,
    }
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

/// Status-bar color for a notification's state, matching the pane borders.
fn note_color(theme: &config::Theme, state: AgentStateKind) -> Color {
    match state {
        AgentStateKind::Blocked => theme.blocked,
        AgentStateKind::Working => theme.working,
        AgentStateKind::Done => theme.done,
        AgentStateKind::Idle | AgentStateKind::Unknown => theme.idle,
    }
}

/// Fire-and-forget: runs the sound command through `sh -c`, fully detached with
/// null stdio so it never touches the TUI or blocks the render loop.
fn spawn_sound(command: &str) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Translates a key event into the bytes a PTY application expects.
pub fn bytes(k: KeyEvent) -> Option<Vec<u8>> {
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut b = match k.code {
        KeyCode::Char(c) if k.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![(c.to_ascii_lowercase() as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        // Every other key comes from the table `pane send-keys` also uses.
        code => crate::keys::from_code(code)?.to_vec(),
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
    fn auto_hide_collapses_at_80_columns_and_restores_when_widened() {
        let config = config::Config::default();
        let mut app = App::new(&config, "work", PathBuf::from("/tmp/kodade-test.sock"));
        // Default auto_hide_below is 100 columns; 80 is under it.
        app.apply_auto_hide(80);
        assert_eq!(app.sidebar_mode, SidebarMode::Compact);
        assert!(app.auto_hidden);
        // Widening back over the threshold restores the full sidebar.
        app.apply_auto_hide(120);
        assert_eq!(app.sidebar_mode, SidebarMode::Full);
        assert!(!app.auto_hidden);
    }

    #[test]
    fn sidebar_toggle_cycles_full_compact_hidden() {
        let config = config::Config::default();
        let mut app = App::new(&config, "work", PathBuf::from("/tmp/kodade-test.sock"));
        assert_eq!(app.sidebar_mode, SidebarMode::Full);
        app.sidebar_mode = app.sidebar_mode.next();
        assert_eq!(app.sidebar_mode, SidebarMode::Compact);
        app.sidebar_mode = app.sidebar_mode.next();
        assert_eq!(app.sidebar_mode, SidebarMode::Hidden);
        app.sidebar_mode = app.sidebar_mode.next();
        assert_eq!(app.sidebar_mode, SidebarMode::Full);
    }

    // A layout with the named workspaces; workspace i gets `WorkspaceId(i+1)`.
    fn layout_named(names: &[&str]) -> LayoutSnapshot {
        let workspaces = names
            .iter()
            .enumerate()
            .map(|(i, name)| WorkspaceInfo {
                id: kodade_cli_proto::WorkspaceId(i as u64 + 1),
                name: (*name).into(),
                active: i == 0,
                state: AgentStateKind::Idle,
                root: None,
                color: None,
                branch: None,
                parent: None,
                tabs: Vec::new(),
            })
            .collect();
        LayoutSnapshot {
            active_workspace: kodade_cli_proto::WorkspaceId(1),
            active_tab: kodade_cli_proto::TabId(1),
            workspaces,
            tabs: Vec::new(),
            tree: kodade_cli_proto::LayoutTree::Leaf { pane: PaneId(1) },
            panes: Vec::new(),
            zoomed: false,
            restored: false,
        }
    }

    #[test]
    fn seed_collapsed_reconciles_newly_appearing_workspaces() {
        use kodade_cli_proto::WorkspaceId;
        let config = config::Config::default();
        let mut app = App::new(
            &config,
            "seed-test-session",
            PathBuf::from("/tmp/kodade-test.sock"),
        );
        // Persisted collapse names a workspace that has not appeared yet.
        app.ui_state
            .collapsed
            .insert("seed-test-session".into(), vec!["beta".into()]);
        // First snapshot has only alpha; beta absent, so nothing collapses.
        app.handle_layout(layout_named(&["alpha"]));
        assert!(app.collapsed.is_empty());
        // beta appears in a later snapshot and is seeded from the persisted set.
        app.handle_layout(layout_named(&["alpha", "beta"]));
        assert!(app.collapsed.contains(&WorkspaceId(2)));
        // A workspace the user expands stays expanded across later snapshots.
        app.collapsed.remove(&WorkspaceId(2));
        app.handle_layout(layout_named(&["alpha", "beta"]));
        assert!(!app.collapsed.contains(&WorkspaceId(2)));
        // A workspace that goes away is dropped from the collapsed set.
        app.collapsed.insert(WorkspaceId(2));
        app.handle_layout(layout_named(&["alpha"]));
        assert!(!app.collapsed.contains(&WorkspaceId(2)));
    }

    #[test]
    fn pane_cols_reserve_the_sidebar() {
        let width = render::sidebar_width(SidebarMode::Full, &config::Config::default());
        assert_eq!(pane_cols(100, width), 100 - width.min(100));
        assert_eq!(pane_cols(1, width), 1);
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
            color: None,
            branch: None,
            parent: None,
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
