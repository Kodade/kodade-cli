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
use kodade_cli_proto::{ClientMessage, Direction, LayoutSnapshot};
use ratatui::{backend::CrosstermBackend, layout::Rect, Frame, Terminal};
use std::{io::Write, time::Duration};
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
    name: String,
    rename_target: Option<mode::MenuTarget>,
    drag: Option<DragState>,
    sidebar: bool,
    navigate: Option<usize>,
    copy: Option<mode::CopyMode>,
    menu: Option<mode::Menu>,
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
            name: String::new(),
            rename_target: None,
            drag: None,
            sidebar: config.sidebar,
            navigate: None,
            copy: None,
            menu: None,
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
                name: &self.name,
                navigate: self.navigate,
                copy: self.copy.as_ref(),
                menu: self.menu.as_ref(),
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
        if self.rename {
            self.handle_rename_key(key, writer).await?;
        } else if self.copy.is_some() {
            self.handle_copy_key(key, writer, term).await?;
        } else if self.menu.is_some() {
            self.handle_menu_key(key, writer).await?;
        } else if let Some(current) = self.navigate {
            self.handle_navigate_key(key, current, writer).await?;
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
                    Some(mode::MenuTarget::Tab(id)) => {
                        write(writer, &ClientMessage::SelectTab { id }).await?;
                        ClientMessage::RenameTab { name }
                    }
                    Some(mode::MenuTarget::Workspace(id)) => {
                        write(writer, &ClientMessage::SelectWorkspace { id }).await?;
                        ClientMessage::RenameWorkspace { name }
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
            config::Action::Detach => return Ok(Flow::Detach),
            config::Action::WorkspaceNext => {
                if let Some(current) = &self.layout {
                    let index = current
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.active)
                        .unwrap_or(0);
                    let next = (index + 1) % current.workspaces.len();
                    let id = current.workspaces[next].id;
                    write(writer, &ClientMessage::SelectWorkspace { id }).await?
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
