use std::collections::HashMap;

use kodade_cli_proto::{LayoutSnapshot, LayoutTree, PaneId};
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::input::{tab_spans, TabSpan};

pub const TAB_PREFIX: &str = " Ködade · ";

pub fn render(frame: &mut Frame, layout: &LayoutSnapshot, prefix: bool, rename: bool, name: &str) {
    let areas = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let workspace = layout
        .workspaces
        .iter()
        .find(|item| item.active)
        .map(|item| item.name.as_str())
        .unwrap_or("workspace");
    let tabs = layout
        .tabs
        .iter()
        .map(|tab| {
            if tab.active {
                format!("[{}]", tab.name)
            } else {
                format!(" {} ", tab.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    frame.render_widget(
        Paragraph::new(format!("{TAB_PREFIX}{workspace}  {tabs}"))
            .style(Style::default().fg(Color::Cyan)),
        areas[0],
    );

    let mut rects = HashMap::new();
    rects_for(&layout.tree, areas[1], &mut rects);
    for pane in &layout.panes {
        if let Some(rect) = rects.get(&pane.id) {
            let title = if pane.scroll_offset > 0 {
                format!("{} [scroll]", pane.title)
            } else {
                pane.title.clone()
            };
            frame.render_widget(
                Paragraph::new(pane.screen.contents.as_str()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(if pane.focused {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        })),
                ),
                *rect,
            );
        }
    }
    let status = if rename {
        format!(" rename pane: {name}")
    } else if prefix {
        " prefix: % \" hjkl c n p w W x z d r".into()
    } else {
        format!(" session · {workspace}")
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        areas[2],
    );
}

pub fn tab_spans_for(layout: &LayoutSnapshot) -> Vec<TabSpan> {
    let workspace_len = layout
        .workspaces
        .iter()
        .find(|item| item.active)
        .map(|item| item.name.chars().count())
        .unwrap_or("workspace".len());
    tab_spans(
        (TAB_PREFIX.chars().count() + workspace_len + 2) as u16,
        &layout.tabs,
    )
}

pub fn pane_rects_for(layout: &LayoutSnapshot, area: Rect) -> Vec<(PaneId, Rect)> {
    let areas = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let mut rects = HashMap::new();
    rects_for(&layout.tree, areas[1], &mut rects);
    let mut pairs: Vec<_> = rects.into_iter().collect();
    pairs.sort_by_key(|(id, _)| id.0);
    pairs
}

fn rects_for(tree: &LayoutTree, rect: Rect, out: &mut HashMap<PaneId, Rect>) {
    match tree {
        LayoutTree::Leaf { pane } => {
            out.insert(*pane, rect);
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let direction = match axis {
                kodade_cli_proto::SplitAxis::Horizontal => LayoutDirection::Horizontal,
                kodade_cli_proto::SplitAxis::Vertical => LayoutDirection::Vertical,
            };
            let span = if direction == LayoutDirection::Horizontal {
                rect.width
            } else {
                rect.height
            };
            let areas = Layout::default()
                .direction(direction)
                .constraints([
                    Constraint::Length(((span as f32 * ratio) as u16).max(1)),
                    Constraint::Min(1),
                ])
                .split(rect);
            rects_for(first, areas[0], out);
            rects_for(second, areas[1], out);
        }
    }
}
