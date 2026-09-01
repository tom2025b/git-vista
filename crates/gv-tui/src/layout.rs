//! The frame's geometry (M10.02, #457 — phase 2a): one terminal [`Rect`] in,
//! the five regions out. Pure and host-tested.
//!
//! # The shape
//!
//! lazygit's: a left column one third wide carrying three stacked panes
//! (Repositories, Branches, Commits) and a main pane taking the right two
//! thirds, with a one-row status strip along the bottom. The three left
//! panes share their column equally; #457 will want the Commits pane to
//! take more once the graph draws there, and that is a constraint change in
//! [`split`] with a test to match, not a redesign.
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
    pub branches: Rect,
    pub commits: Rect,
    pub main: Rect,
    pub status: Rect,
}

impl Panes {
    pub fn of(&self, pane: Pane) -> Rect {
        match pane {
            Pane::Repositories => self.repositories,
            Pane::Branches => self.branches,
            Pane::Commits => self.commits,
            Pane::Main => self.main,
        }
    }
}

/// Tile `area` into the five regions, or `None` when it is below the
/// minimum.
pub fn split(area: Rect) -> Option<Panes> {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let [body, status] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let [left, main] =
        Layout::horizontal([Constraint::Ratio(1, 3), Constraint::Ratio(2, 3)]).areas(body);
    let [repositories, branches, commits] = Layout::vertical([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(left);
    Some(Panes {
        repositories,
        branches,
        commits,
        main,
        status,
    })
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
        assert_eq!(split(area(MIN_WIDTH - 1, MIN_HEIGHT)), None);
        assert_eq!(split(area(MIN_WIDTH, MIN_HEIGHT - 1)), None);
        assert_eq!(split(area(0, 0)), None);
        assert!(
            split(area(MIN_WIDTH, MIN_HEIGHT)).is_some(),
            "the minimum itself is drawable"
        );
    }

    #[test]
    fn the_panes_tile_the_area_exactly_with_no_overlap_and_no_gap() {
        for (w, h) in [(MIN_WIDTH, MIN_HEIGHT), (80, 24), (91, 31), (200, 60)] {
            let whole = area(w, h);
            let panes = split(whole).expect("above the minimum");
            let all = [
                panes.repositories,
                panes.branches,
                panes.commits,
                panes.main,
                panes.status,
            ];
            let total: u32 = all.iter().map(|r| cells(*r)).sum();
            assert_eq!(
                total,
                cells(whole),
                "{w}x{h}: the regions must cover every cell once"
            );
            for (i, a) in all.iter().enumerate() {
                assert!(
                    whole.union(*a) == whole,
                    "{w}x{h}: region {i} leaves the terminal"
                );
                for (j, b) in all.iter().enumerate() {
                    if i != j {
                        assert!(
                            a.intersection(*b).is_empty(),
                            "{w}x{h}: regions {i} and {j} overlap: {a:?} {b:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_status_strip_is_the_last_row_full_width() {
        let panes = split(area(80, 24)).unwrap();
        assert_eq!(panes.status, Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn the_main_pane_takes_the_right_two_thirds_and_the_left_column_stacks_three() {
        let panes = split(area(90, 31)).unwrap();
        assert_eq!(panes.main, Rect::new(30, 0, 60, 30));
        for left in [panes.repositories, panes.branches, panes.commits] {
            assert_eq!(left.x, 0);
            assert_eq!(left.width, 30);
            assert_eq!(left.height, 10);
        }
        assert_eq!(panes.repositories.y, 0);
        assert_eq!(panes.branches.y, 10);
        assert_eq!(panes.commits.y, 20);
    }

    #[test]
    fn at_the_minimum_every_pane_still_has_one_interior_row() {
        let panes = split(area(MIN_WIDTH, MIN_HEIGHT)).unwrap();
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
