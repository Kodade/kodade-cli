//! Pure pane-tree operations. Keeping these PTY-free makes the core testable.

use kodade_cli_proto::{Direction, LayoutTree, PaneId, SplitAxis};

const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

pub fn split(tree: &mut LayoutTree, target: PaneId, axis: SplitAxis, new_pane: PaneId) -> bool {
    match tree {
        LayoutTree::Leaf { pane } if *pane == target => {
            *tree = LayoutTree::Split {
                axis,
                ratio: 0.5,
                first: Box::new(LayoutTree::Leaf { pane: target }),
                second: Box::new(LayoutTree::Leaf { pane: new_pane }),
            };
            true
        }
        LayoutTree::Split { first, second, .. } => {
            split(first, target, axis, new_pane) || split(second, target, axis, new_pane)
        }
        _ => false,
    }
}

/// Removes a leaf and collapses its parent; callers handle an empty root.
pub fn close(tree: LayoutTree, target: PaneId) -> Option<LayoutTree> {
    match tree {
        LayoutTree::Leaf { pane } => (pane != target).then_some(LayoutTree::Leaf { pane }),
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => match (close(*first, target), close(*second, target)) {
            (Some(first), Some(second)) => Some(LayoutTree::Split {
                axis,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(child), None) | (None, Some(child)) => Some(child),
            (None, None) => None,
        },
    }
}

pub fn leaves(tree: &LayoutTree, output: &mut Vec<PaneId>) {
    match tree {
        LayoutTree::Leaf { pane } => output.push(*pane),
        LayoutTree::Split { first, second, .. } => {
            leaves(first, output);
            leaves(second, output);
        }
    }
}

pub fn contains(tree: &LayoutTree, target: PaneId) -> bool {
    match tree {
        LayoutTree::Leaf { pane } => *pane == target,
        LayoutTree::Split { first, second, .. } => {
            contains(first, target) || contains(second, target)
        }
    }
}

/// Exchanges two leaves in place, wherever they sit in the tree.
pub fn swap(tree: &mut LayoutTree, a: PaneId, b: PaneId) -> bool {
    if a == b || !contains(tree, a) || !contains(tree, b) {
        return false;
    }
    swap_inner(tree, a, b);
    true
}
fn swap_inner(tree: &mut LayoutTree, a: PaneId, b: PaneId) {
    match tree {
        LayoutTree::Leaf { pane } => {
            if *pane == a {
                *pane = b;
            } else if *pane == b {
                *pane = a;
            }
        }
        LayoutTree::Split { first, second, .. } => {
            swap_inner(first, a, b);
            swap_inner(second, a, b);
        }
    }
}

/// Resets every split ratio so sibling panes get equal space.
pub fn equalize(tree: &mut LayoutTree) {
    if let LayoutTree::Split {
        ratio,
        first,
        second,
        ..
    } = tree
    {
        *ratio = 0.5;
        equalize(first);
        equalize(second);
    }
}

/// Next / previous leaf in tree order, wrapping at the ends.
pub fn cycle(tree: &LayoutTree, current: PaneId, forward: bool) -> Option<PaneId> {
    let mut panes = Vec::new();
    leaves(tree, &mut panes);
    let index = panes.iter().position(|pane| *pane == current)?;
    let count = panes.len();
    let next = if forward {
        (index + 1) % count
    } else {
        (index + count - 1) % count
    };
    Some(panes[next])
}

pub fn resize(tree: &mut LayoutTree, target: PaneId, direction: Direction, delta: f32) -> bool {
    resize_inner(tree, target, direction, delta).is_some()
}

fn resize_inner(
    tree: &mut LayoutTree,
    target: PaneId,
    direction: Direction,
    delta: f32,
) -> Option<bool> {
    match tree {
        LayoutTree::Leaf { pane } => (*pane == target).then_some(false),
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let first_has = resize_inner(first, target, direction, delta);
            if let Some(changed) = first_has {
                if !changed && axis_matches(*axis, direction) {
                    *ratio =
                        (*ratio + signed_delta(true, direction, delta)).clamp(MIN_RATIO, MAX_RATIO);
                    return Some(true);
                }
                return Some(changed);
            }
            let second_has = resize_inner(second, target, direction, delta);
            if let Some(changed) = second_has {
                if !changed && axis_matches(*axis, direction) {
                    *ratio = (*ratio + signed_delta(false, direction, delta))
                        .clamp(MIN_RATIO, MAX_RATIO);
                    return Some(true);
                }
                return Some(changed);
            }
            None
        }
    }
}

fn axis_matches(axis: SplitAxis, direction: Direction) -> bool {
    matches!(
        (axis, direction),
        (SplitAxis::Horizontal, Direction::Left | Direction::Right)
            | (SplitAxis::Vertical, Direction::Up | Direction::Down)
    )
}
fn signed_delta(first: bool, direction: Direction, delta: f32) -> f32 {
    match (first, direction) {
        (true, Direction::Left | Direction::Up) | (false, Direction::Right | Direction::Down) => {
            delta
        }
        _ => -delta,
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}
fn rects(tree: &LayoutTree, rect: Rect, output: &mut Vec<(PaneId, Rect)>) {
    match tree {
        LayoutTree::Leaf { pane } => output.push((*pane, rect)),
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let first_size = match axis {
                SplitAxis::Horizontal => (rect.width as f32 * ratio) as u16,
                SplitAxis::Vertical => (rect.height as f32 * ratio) as u16,
            };
            let (one, two) = match axis {
                SplitAxis::Horizontal => (
                    Rect {
                        width: first_size,
                        ..rect
                    },
                    Rect {
                        x: rect.x + first_size,
                        width: rect.width - first_size,
                        ..rect
                    },
                ),
                SplitAxis::Vertical => (
                    Rect {
                        height: first_size,
                        ..rect
                    },
                    Rect {
                        y: rect.y + first_size,
                        height: rect.height - first_size,
                        ..rect
                    },
                ),
            };
            rects(first, one, output);
            rects(second, two, output);
        }
    }
}

pub fn focus_neighbor(tree: &LayoutTree, current: PaneId, direction: Direction) -> Option<PaneId> {
    let mut all = Vec::new();
    rects(
        tree,
        Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        },
        &mut all,
    );
    let current_rect = all.iter().find(|(pane, _)| *pane == current)?.1;
    all.into_iter()
        .filter(|(pane, rect)| *pane != current && is_direction(current_rect, *rect, direction))
        .max_by_key(|(_, rect)| {
            overlap(current_rect, *rect, direction) * 10_000
                - distance(current_rect, *rect, direction)
        })
        .map(|(pane, _)| pane)
}
fn is_direction(a: Rect, b: Rect, dir: Direction) -> bool {
    match dir {
        Direction::Left => b.x + b.width <= a.x,
        Direction::Right => b.x >= a.x + a.width,
        Direction::Up => b.y + b.height <= a.y,
        Direction::Down => b.y >= a.y + a.height,
    }
}
fn overlap(a: Rect, b: Rect, dir: Direction) -> i32 {
    match dir {
        Direction::Left | Direction::Right => (a.y + a.height)
            .min(b.y + b.height)
            .saturating_sub(a.y.max(b.y)) as i32,
        Direction::Up | Direction::Down => (a.x + a.width)
            .min(b.x + b.width)
            .saturating_sub(a.x.max(b.x)) as i32,
    }
}
fn distance(a: Rect, b: Rect, dir: Direction) -> i32 {
    match dir {
        Direction::Left => a.x as i32 - (b.x + b.width) as i32,
        Direction::Right => b.x as i32 - (a.x + a.width) as i32,
        Direction::Up => a.y as i32 - (b.y + b.height) as i32,
        Direction::Down => b.y as i32 - (a.y + a.height) as i32,
    }
    .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn leaf(id: u64) -> LayoutTree {
        LayoutTree::Leaf { pane: PaneId(id) }
    }
    #[test]
    fn split_close_and_zoom_tree_operations() {
        let mut tree = leaf(1);
        assert!(split(
            &mut tree,
            PaneId(1),
            SplitAxis::Horizontal,
            PaneId(2)
        ));
        assert_eq!(close(tree, PaneId(2)), Some(leaf(1)));
    }
    #[test]
    fn focus_uses_adjacent_geometry() {
        let mut tree = leaf(1);
        split(&mut tree, PaneId(1), SplitAxis::Horizontal, PaneId(2));
        assert_eq!(
            focus_neighbor(&tree, PaneId(1), Direction::Right),
            Some(PaneId(2))
        );
    }
    /// `1 | (2 / (3 | 4))` — three levels deep, uneven ratios.
    fn nested() -> LayoutTree {
        LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.3,
            first: Box::new(leaf(1)),
            second: Box::new(LayoutTree::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.8,
                first: Box::new(leaf(2)),
                second: Box::new(LayoutTree::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 0.2,
                    first: Box::new(leaf(3)),
                    second: Box::new(leaf(4)),
                }),
            }),
        }
    }

    #[test]
    fn swap_exchanges_leaves_across_nested_splits() {
        let mut tree = nested();
        assert!(swap(&mut tree, PaneId(1), PaneId(4)));
        let mut order = Vec::new();
        leaves(&tree, &mut order);
        assert_eq!(order, vec![PaneId(4), PaneId(2), PaneId(3), PaneId(1)]);
        // Unknown panes and self-swaps are no-ops.
        assert!(!swap(&mut tree, PaneId(1), PaneId(1)));
        assert!(!swap(&mut tree, PaneId(1), PaneId(99)));
    }

    #[test]
    fn swap_targets_come_from_the_focus_neighbor() {
        let mut tree = leaf(1);
        split(&mut tree, PaneId(1), SplitAxis::Horizontal, PaneId(2));
        let target =
            focus_neighbor(&tree, PaneId(1), Direction::Right).expect("right neighbor exists");
        assert!(swap(&mut tree, PaneId(1), target));
        let mut order = Vec::new();
        leaves(&tree, &mut order);
        assert_eq!(order, vec![PaneId(2), PaneId(1)]);
    }

    #[test]
    fn equalize_resets_every_ratio_in_a_deep_tree() {
        let mut tree = nested();
        equalize(&mut tree);
        let mut ratios = Vec::new();
        fn collect(tree: &LayoutTree, output: &mut Vec<f32>) {
            if let LayoutTree::Split {
                ratio,
                first,
                second,
                ..
            } = tree
            {
                output.push(*ratio);
                collect(first, output);
                collect(second, output);
            }
        }
        collect(&tree, &mut ratios);
        assert_eq!(ratios, vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn cycle_wraps_in_both_directions() {
        let tree = nested();
        assert_eq!(cycle(&tree, PaneId(1), true), Some(PaneId(2)));
        assert_eq!(cycle(&tree, PaneId(4), true), Some(PaneId(1)));
        assert_eq!(cycle(&tree, PaneId(1), false), Some(PaneId(4)));
        assert_eq!(cycle(&tree, PaneId(99), true), None);
        // A single pane cycles to itself.
        assert_eq!(cycle(&leaf(7), PaneId(7), true), Some(PaneId(7)));
    }

    #[test]
    fn resize_clamps_ratio() {
        let mut tree = LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(1)),
            second: Box::new(leaf(2)),
        };
        resize(&mut tree, PaneId(1), Direction::Left, 10.0);
        let LayoutTree::Split { ratio, .. } = tree else {
            panic!()
        };
        assert_eq!(ratio, MAX_RATIO);
    }
}
