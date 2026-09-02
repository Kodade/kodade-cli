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
    AgentStateKind, ClientMessage, Direction, LayoutSnapshot, PaneId, SidebarTabInfo, WorkspaceInfo,
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Frame, Terminal};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, net::unix::OwnedWriteHalf, sync::mpsc};

use crate::{config, input, mode, render};

const SCROLL_STEP: i16 = 3;

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
    note: Option<String>,
    /// Session reported by the daemon's `Welcome`; the status bar uses it in #11.
    #[allow(dead_code)]
    session_name: String,
    config: config::Config,
    theme: config::Theme,
}

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
            note: None,
            session_name: session.to_string(),
            theme: config.resolve_theme(),
            config: config.clone(),
        }
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
        // Remember the previously focused pane so `last_pane` can jump back.
        let focused = layout.panes.iter().find(|pane| pane.focused).map(|p| p.id);
        if focused != self.focused_pane {
            if let Some(previous) = self.focused_pane {
                self.last_pane = Some(previous);
            }
            self.focused_pane = focused;
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
                note: self.note.as_deref(),
            },
            &self.theme,
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
        } else if let Some(current) = self.navigate {
            self.handle_navigate_key(key, current, writer).await?;
        } else if self.resize {
            self.handle_resize_key(key, writer).await?;
        } else if self.prefix {
            return self.handle_prefix_key(key, writer, term).await;
        } else if key == self.config.prefix {
            self.prefix = true;
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
                    let (payload, truncated) = mode::osc52(&text);
                    execute!(term.backend_mut(), crossterm::style::Print(payload))?;
                    term.backend_mut().flush()?;
                    self.note = Some(if truncated {
                        " copied (truncated to 100KB)".into()
                    } else {
                        " copied".into()
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
        if key == self.config.prefix {
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
        match action {
            config::Action::SidebarToggle => {
                self.sidebar = !self.sidebar;
                let size = term.size()?;
                write(
                    writer,
                    &ClientMessage::Resize {
                        cols: self.pane_cols(size.width),
                        rows: size.height,
                    },
                )
                .await?;
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
            other => {
                if let Some(message) = other.message() {
                    write(writer, &message).await?
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// Routes a mouse event; ignored entirely when mouse support is off.
    pub async fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        writer: &mut OwnedWriteHalf,
        term: &mut Term,
    ) -> Result<()> {
        if !self.config.mouse || self.layout.is_none() {
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_left_down(mouse, writer, term).await?
            }
            MouseEventKind::Down(MouseButton::Right) => self.mouse_right_down(mouse, term)?,
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
            MouseEventKind::Up(MouseButton::Left) => self.drag = None,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let area = self.content_area(term)?;
                let current = self.layout.as_ref().expect("layout present");
                let rects = render::pane_rects_for(current, area);
                if let Some(id) = input::pane_at(&rects, mouse.column, mouse.row) {
                    let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        SCROLL_STEP
                    } else {
                        -SCROLL_STEP
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
        if let Some(menu) = &mut self.menu {
            if let Some(selected) = mode::menu_hit(menu, mouse.column, mouse.row) {
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
            if let Some(id) = input::tab_at(
                &render::tab_spans_for(current, content_area.x),
                mouse.column,
            ) {
                write(writer, &ClientMessage::SelectTab { id }).await?;
            }
        } else if let Some(id) = input::pane_at(&rects, mouse.column, mouse.row) {
            write(writer, &ClientMessage::FocusPaneId { id }).await?;
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
