use kodade_cli_proto::{Direction, PaneId, TabId, TabInfo};
use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabSpan {
    pub id: TabId,
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragBorder {
    pub pane: PaneId,
    pub direction: Direction,
    pub vertical: bool,
}

pub fn tab_spans(start: u16, tabs: &[TabInfo]) -> Vec<TabSpan> {
    let mut column = start;
    tabs.iter()
        .map(|tab| {
            let label = if tab.active {
                format!("[{}]", tab.name)
            } else {
                format!(" {} ", tab.name)
            };
            let span = TabSpan {
                id: tab.id,
                start: column,
                end: column.saturating_add(label.chars().count() as u16),
            };
            column = span.end.saturating_add(1);
            span
        })
        .collect()
}

pub fn tab_at(spans: &[TabSpan], column: u16) -> Option<TabId> {
    spans
        .iter()
        .find(|span| (span.start..span.end).contains(&column))
        .map(|span| span.id)
}

pub fn pane_at(rects: &[(PaneId, Rect)], column: u16, row: u16) -> Option<PaneId> {
    rects
        .iter()
        .find(|(_, rect)| rect.contains((column, row).into()))
        .map(|(id, _)| *id)
}

pub fn border_at(rects: &[(PaneId, Rect)], column: u16, row: u16) -> Option<DragBorder> {
    for (index, (first_id, first)) in rects.iter().enumerate() {
        for (second_id, second) in &rects[index + 1..] {
            if first.x.saturating_add(first.width) == second.x
                && overlaps(first.y, first.height, second.y, second.height, row)
                && distance(column, second.x) <= 1
            {
                return Some(DragBorder {
                    pane: *first_id,
                    direction: Direction::Left,
                    vertical: true,
                });
            }
            if second.x.saturating_add(second.width) == first.x
                && overlaps(first.y, first.height, second.y, second.height, row)
                && distance(column, first.x) <= 1
            {
                return Some(DragBorder {
                    pane: *second_id,
                    direction: Direction::Left,
                    vertical: true,
                });
            }
            if first.y.saturating_add(first.height) == second.y
                && overlaps(first.x, first.width, second.x, second.width, column)
                && distance(row, second.y) <= 1
            {
                return Some(DragBorder {
                    pane: *first_id,
                    direction: Direction::Up,
                    vertical: false,
                });
            }
            if second.y.saturating_add(second.height) == first.y
                && overlaps(first.x, first.width, second.x, second.width, column)
                && distance(row, first.y) <= 1
            {
                return Some(DragBorder {
                    pane: *second_id,
                    direction: Direction::Up,
                    vertical: false,
                });
            }
        }
    }
    None
}

pub fn drag_delta(start: u16, current: u16) -> i16 {
    (i32::from(current) - i32::from(start)).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn overlaps(start: u16, len: u16, other_start: u16, other_len: u16, point: u16) -> bool {
    let end = start.saturating_add(len);
    let other_end = other_start.saturating_add(other_len);
    point >= start.max(other_start) && point < end.min(other_end)
}

fn distance(a: u16, b: u16) -> u16 {
    a.max(b) - a.min(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_hit_testing_uses_rendered_spans() {
        let tabs = vec![
            TabInfo {
                id: TabId(1),
                name: "shell".into(),
                active: true,
            },
            TabInfo {
                id: TabId(2),
                name: "logs".into(),
                active: false,
            },
        ];
        let spans = tab_spans(10, &tabs);
        assert_eq!(
            spans[0],
            TabSpan {
                id: TabId(1),
                start: 10,
                end: 17
            }
        );
        assert_eq!(tab_at(&spans, 16), Some(TabId(1)));
        assert_eq!(tab_at(&spans, 17), None);
        assert_eq!(tab_at(&spans, 18), Some(TabId(2)));
    }

    #[test]
    fn border_hit_testing_and_drag_delta_are_cell_accurate() {
        let rects = [
            (PaneId(1), Rect::new(0, 1, 40, 10)),
            (PaneId(2), Rect::new(40, 1, 40, 10)),
        ];
        assert_eq!(
            border_at(&rects, 39, 4),
            Some(DragBorder {
                pane: PaneId(1),
                direction: Direction::Left,
                vertical: true
            })
        );
        assert_eq!(border_at(&rects, 42, 4), None);
        assert_eq!(drag_delta(40, 46), 6);
        assert_eq!(drag_delta(46, 40), -6);
    }
}
