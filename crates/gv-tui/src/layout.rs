//! The frame's geometry (M10.02, #457 — phase 2a): one terminal [`Rect`] in,
//! the five regions out. Pure and host-tested.
//!
//! # The shape
//!
//! lazygit's: a left column carrying three stacked panes (Repositories,
//! Working Tree, Commits) and a main pane, with a one-row status strip along the
//! bottom. The three left panes share their column equally. #457's graph
//! rows carry a short id and summary beside their lane glyphs, which a
//! one-third column truncated — the left/main split is half and half so
//! Commits has room, the constraint change this module anticipated rather
//! than a redesign of the shape.
//!
//! # Maximize is a second shape, not a second layout engine (#625)
//!
//! [`split`] takes the pane the user has zoomed. When one is named it gets
//! the whole body and the other three come back as **zero-sized rects at the
//! body's origin** — still inside the terminal, still non-overlapping, still
//! summing to exactly the area. That keeps one invariant (`the_panes_tile_
//! the_area_exactly...`) covering both shapes instead of exempting the
//! zoomed one, and lets every drawing path stay a projection of a `Panes`
//! rather than branching on a mode.
//!
//! # Too small is said, not squeezed
//!
//! Below [`MIN_WIDTH`]×[`MIN_HEIGHT`] the answer is `None` and `ui.rs` draws
//! one line saying what the minimum is. The alternative — letting ratatui
//! clamp every region to zero and drawing bordered boxes with no interior —
//! renders something that looks like a frame and shows nothing, which is
//! the kind of quiet failure this codebase refuses (a pane that draws an
//! empty box claims there was content and it was blank).

use ratatui::layout::{Constraint, Layout, Rect};

use crate::app::Pane;

/// The smallest terminal the shell will draw a frame in: three bordered
/// panes need three rows each (border, one line, border) plus the status
/// row, and a third of the width must still fit a repository name.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;

/// The five regions of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Panes {
    pub repositories: Rect,
    pub working_tree: Rect,
    pub commits: Rect,
    pub main: Rect,
    pub status: Rect,
}

impl Panes {
    pub fn of(&self, pane: Pane) -> Rect {
        match pane {
            Pane::Repositories => self.repositories,
            Pane::WorkingTree => self.working_tree,
            Pane::Commits => self.commits,
            Pane::Main => self.main,
        }
    }
}

/// Tile `area` into the five regions, or `None` when it is below the
/// minimum.
///
/// `maximized` names the pane the user has zoomed (#625); `None` is the
/// ordinary four-pane shape. A zoomed pane takes the entire body and the
/// other three come back empty — see the module doc for why empty rather
/// than a separate return type.
pub fn split(area: Rect, maximized: Option<Pane>) -> Option<Panes> {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let [body, status] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if let Some(pane) = maximized {
        return Some(maximize(pane, body, status));
    }
    // #457's graph rows carry a short id and a summary beside their lane
    // glyphs (`render_commit_line`); a one-third column truncates that
    // before the summary is legible. Widening the left column to half is
    // the "constraint change... not a redesign" this module anticipated —
    // same two-column shape, wider split.
    let [left, main] =
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas(body);
    let [repositories, working_tree, commits] = Layout::vertical([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(left);
    Some(Panes {
        repositories,
        working_tree,
        commits,
        main,
        status,
    })
}

/// One pane over the whole body, the other three collapsed to nothing.
///
/// The collapsed rects sit at the body's origin so they stay inside the
/// terminal and inside the tiling invariant; a zero-area rect contributes no
/// cells and intersects nothing.
fn maximize(pane: Pane, body: Rect, status: Rect) -> Panes {
    let hidden = Rect::new(body.x, body.y, 0, 0);
    let mut panes = Panes {
        repositories: hidden,
        working_tree: hidden,
        commits: hidden,
        main: hidden,
        status,
    };
    *match pane {
        Pane::Repositories => &mut panes.repositories,
        Pane::WorkingTree => &mut panes.working_tree,
        Pane::Commits => &mut panes.commits,
        Pane::Main => &mut panes.main,
    } = body;
    panes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    fn cells(r: Rect) -> u32 {
        u32::from(r.width) * u32::from(r.height)
    }

    #[test]
    fn a_terminal_below_the_minimum_gets_no_panes() {
        assert_eq!(split(area(MIN_WIDTH - 1, MIN_HEIGHT), None), None);
        assert_eq!(split(area(MIN_WIDTH, MIN_HEIGHT - 1), None), None);
        assert_eq!(split(area(0, 0), None), None);
        assert!(
            split(area(MIN_WIDTH, MIN_HEIGHT), None).is_some(),
            "the minimum itself is drawable"
        );
        assert_eq!(
            split(area(MIN_WIDTH - 1, MIN_HEIGHT), Some(Pane::Commits)),
            None,
            "zooming does not make a terminal big enough to draw in"
        );
    }

    #[test]
    fn the_panes_tile_the_area_exactly_with_no_overlap_and_no_gap() {
        // Both shapes, one invariant. A maximized frame hides three panes by
        // giving them no cells, so the same arithmetic has to hold — a zoom
        // that leaked a row would show here rather than in a screenshot.
        let shapes = [
            None,
            Some(Pane::Repositories),
            Some(Pane::WorkingTree),
            Some(Pane::Commits),
            Some(Pane::Main),
        ];
        for (w, h) in [(MIN_WIDTH, MIN_HEIGHT), (80, 24), (91, 31), (200, 60)] {
            for maximized in shapes {
                let whole = area(w, h);
                let panes = split(whole, maximized).expect("above the minimum");
                let all = [
                    panes.repositories,
                    panes.working_tree,
                    panes.commits,
                    panes.main,
                    panes.status,
                ];
                let total: u32 = all.iter().map(|r| cells(*r)).sum();
                assert_eq!(
                    total,
                    cells(whole),
                    "{w}x{h} {maximized:?}: the regions must cover every cell once"
                );
                for (i, a) in all.iter().enumerate() {
                    assert!(
                        whole.union(*a) == whole,
                        "{w}x{h} {maximized:?}: region {i} leaves the terminal"
                    );
                    for (j, b) in all.iter().enumerate() {
                        if i != j {
                            assert!(
                                a.intersection(*b).is_empty(),
                                "{w}x{h} {maximized:?}: regions {i} and {j} overlap: {a:?} {b:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// INVARIANT (#625): a maximized pane gets the whole body — every row the
    /// terminal has except the status strip — and no other pane is drawn.
    ///
    /// MUTATION 1 (remove): return the ordinary four-pane split whatever
    /// `maximized` says.
    /// MUTATION 2 (weaken): give the zoomed pane the left column's width
    /// instead of the whole body.
    #[test]
    fn a_maximized_pane_takes_the_whole_body_and_the_others_vanish() {
        let whole = area(90, 31);
        let unzoomed = split(whole, None).unwrap();
        for zoomed in Pane::ALL {
            let panes = split(whole, Some(zoomed)).unwrap();
            assert_eq!(
                panes.of(zoomed),
                Rect::new(0, 0, 90, 30),
                "{zoomed:?} is zoomed and must fill the body"
            );
            // Cells, not rows: Main is already full-height unzoomed (it is
            // the right HALF), so what zooming buys it is width. Asserting
            // height alone would have passed for three panes and been wrong
            // about the fourth.
            assert!(
                cells(panes.of(zoomed)) > cells(unzoomed.of(zoomed)),
                "{zoomed:?} zoomed ({:?}) is no bigger than unzoomed ({:?}) — the whole point of the key",
                panes.of(zoomed),
                unzoomed.of(zoomed)
            );
            for other in Pane::ALL.into_iter().filter(|p| *p != zoomed) {
                assert!(
                    panes.of(other).is_empty(),
                    "{other:?} still has cells while {zoomed:?} is zoomed: {:?}",
                    panes.of(other)
                );
            }
            assert_eq!(
                panes.status, unzoomed.status,
                "the status strip survives a zoom — it is where the way back is named"
            );
        }
    }

    #[test]
    fn the_status_strip_is_the_last_row_full_width() {
        let panes = split(area(80, 24), None).unwrap();
        assert_eq!(panes.status, Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn the_main_pane_takes_the_right_half_and_the_left_column_stacks_three() {
        let panes = split(area(90, 31), None).unwrap();
        assert_eq!(panes.main, Rect::new(45, 0, 45, 30));
        for left in [panes.repositories, panes.working_tree, panes.commits] {
            assert_eq!(left.x, 0);
            assert_eq!(left.width, 45);
            assert_eq!(left.height, 10);
        }
        assert_eq!(panes.repositories.y, 0);
        assert_eq!(panes.working_tree.y, 10);
        assert_eq!(panes.commits.y, 20);
    }

    #[test]
    fn at_the_minimum_every_pane_still_has_one_interior_row() {
        let panes = split(area(MIN_WIDTH, MIN_HEIGHT), None).unwrap();
        for pane in Pane::ALL {
            let r = panes.of(pane);
            assert!(
                r.height >= 3,
                "{pane:?} is {} rows: a border needs 2, a line needs 1",
                r.height
            );
            assert!(r.width >= 3, "{pane:?} is {} wide", r.width);
        }
    }
}
