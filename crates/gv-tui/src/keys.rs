//! Key dispatch (M10.02, #457 — phase 2a): one crossterm [`KeyEvent`] in,
//! at most one [`Action`] out. Pure and host-tested; the event loop calls it
//! and never interprets a key itself.
//!
//! # The bindings
//!
//! | Key | Action |
//! |---|---|
//! | `q`, `Ctrl-C` | quit |
//! | `Tab`, `l` | focus the next pane |
//! | `BackTab`, `h` | focus the previous pane |
//! | `←` / `→` | focus outside Main; horizontal scroll in Main |
//! | `1`–`4` | focus that pane |
//! | `j`, `↓` | cursor down |
//! | `k`, `↑` | cursor up |
//! | `Enter` | load a repository, open a commit, or follow a parent |
//! | `Space` | preview selected working-tree file / diff file / hunk / line |
//! | `a` | preview stage-all or unstage-all from the selected status section |
//! | `d` | guard discard of the selected unstaged tracked path |
//! | `y` | approve the visible discard confirmation or plan review |
//! | `n`, `Esc` | refuse the visible confirmation or plan review |
//! | `[` / `]` | select the previous/next parent in Main |
//! | `r`, `F5` | refresh |
//!
//! The vi-shaped `hjkl` set is lazygit's, and lazygit is the interface this
//! milestone is modelled on; the arrow and Tab set is for everyone else.
//!
//! # Only a press counts
//!
//! Terminals that report key *releases* (Windows, and any terminal with the
//! kitty keyboard protocol on) deliver two events per keystroke. Dispatch
//! ignores `Release` and `Repeat` is treated as a press — so `q` quits once,
//! not twice, and a held `j` still scrolls.
//!
//! The staging bindings are pane-specific. Dispatch only identifies intent;
//! `App` decides whether the selected row can honestly perform it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{Action, Pane};

/// Translate one key event. `None` means the key is unbound.
pub fn dispatch(key: KeyEvent, pane: Pane) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Some(Action::Quit),
        _ if !plain => None,
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('y') => Some(Action::Approve),
        KeyCode::Char('n') | KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Tab | KeyCode::Char('l') => Some(Action::FocusNext),
        KeyCode::BackTab | KeyCode::Char('h') => Some(Action::FocusPrev),
        KeyCode::Right if pane == Pane::Main => Some(Action::HorizontalRight),
        KeyCode::Left if pane == Pane::Main => Some(Action::HorizontalLeft),
        KeyCode::Right => Some(Action::FocusNext),
        KeyCode::Left => Some(Action::FocusPrev),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::CursorDown),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::CursorUp),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Char(' ') if matches!(pane, Pane::WorkingTree | Pane::Main) => {
            Some(Action::PreviewSelection)
        }
        KeyCode::Char('a') if pane == Pane::WorkingTree => Some(Action::PreviewWholeTree),
        KeyCode::Char('d') if pane == Pane::WorkingTree => Some(Action::Discard),
        KeyCode::Char('[') if pane == Pane::Main => Some(Action::ParentPrev),
        KeyCode::Char(']') if pane == Pane::Main => Some(Action::ParentNext),
        KeyCode::F(5) | KeyCode::Char('r') => Some(Action::Refresh),
        KeyCode::Char(d @ '1'..='9') => Pane::from_number(d.to_digit(10)? as u8).map(Action::Focus),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Pane;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn global(key: KeyEvent) -> Option<Action> {
        dispatch(key, Pane::Repositories)
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        assert_eq!(global(press(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(global(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn tab_and_back_tab_move_focus_and_so_do_h_l_and_the_side_arrows() {
        for key in [
            press(KeyCode::Tab),
            press(KeyCode::Char('l')),
            press(KeyCode::Right),
        ] {
            assert_eq!(global(key), Some(Action::FocusNext), "{key:?}");
        }
        for key in [
            press(KeyCode::BackTab),
            press(KeyCode::Char('h')),
            press(KeyCode::Left),
        ] {
            assert_eq!(global(key), Some(Action::FocusPrev), "{key:?}");
        }
    }

    #[test]
    fn digits_one_to_four_focus_that_pane_and_other_digits_do_nothing() {
        assert_eq!(
            global(press(KeyCode::Char('1'))),
            Some(Action::Focus(Pane::Repositories))
        );
        assert_eq!(
            global(press(KeyCode::Char('2'))),
            Some(Action::Focus(Pane::WorkingTree))
        );
        assert_eq!(
            global(press(KeyCode::Char('3'))),
            Some(Action::Focus(Pane::Commits))
        );
        assert_eq!(
            global(press(KeyCode::Char('4'))),
            Some(Action::Focus(Pane::Main))
        );
        assert_eq!(global(press(KeyCode::Char('0'))), None);
        assert_eq!(global(press(KeyCode::Char('5'))), None);
        assert_eq!(global(press(KeyCode::Char('9'))), None);
    }

    #[test]
    fn j_k_and_the_vertical_arrows_move_the_cursor() {
        for key in [press(KeyCode::Char('j')), press(KeyCode::Down)] {
            assert_eq!(global(key), Some(Action::CursorDown), "{key:?}");
        }
        for key in [press(KeyCode::Char('k')), press(KeyCode::Up)] {
            assert_eq!(global(key), Some(Action::CursorUp), "{key:?}");
        }
    }

    #[test]
    fn r_and_f5_refresh() {
        assert_eq!(global(press(KeyCode::Char('r'))), Some(Action::Refresh));
        assert_eq!(global(press(KeyCode::F(5))), Some(Action::Refresh));
    }

    #[test]
    fn a_key_release_is_ignored_even_for_q_but_a_repeat_still_counts() {
        let release = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(
            global(release),
            None,
            "a release after the press must not quit twice"
        );
        let repeat = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        };
        assert_eq!(
            global(repeat),
            Some(Action::CursorDown),
            "a held key keeps scrolling"
        );
    }

    #[test]
    fn a_modified_letter_is_not_its_plain_binding() {
        // `Ctrl-q`, `Alt-j`: unbound, so a chord meant for the terminal or
        // the multiplexer never leaks into the app as the bare letter.
        assert_eq!(global(ctrl('q')), None);
        assert_eq!(
            global(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn an_unbound_key_is_none() {
        for key in [press(KeyCode::Char('x')), press(KeyCode::F(1))] {
            assert_eq!(global(key), None, "{key:?}");
        }
        assert_eq!(global(press(KeyCode::Char(' '))), None);
    }

    #[test]
    fn enter_activates_rows_in_all_four_built_panes() {
        let enter = press(KeyCode::Enter);
        for pane in Pane::ALL {
            assert_eq!(dispatch(enter, pane), Some(Action::Activate), "{pane:?}");
        }
    }

    /// INVARIANT: every #459 action is keyboard-reachable only in the panes
    /// where its selection has meaning, while approval/refusal remain global.
    ///
    /// MUTATION 1 (remove): make Space inert in Working Tree and Main.
    /// MUTATION 2 (weaken): make the all-tree shortcut active in every pane.
    #[test]
    fn staging_and_review_keys_are_scoped_without_hiding_cancel() {
        assert_eq!(
            dispatch(press(KeyCode::Char(' ')), Pane::WorkingTree),
            Some(Action::PreviewSelection)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char(' ')), Pane::Main),
            Some(Action::PreviewSelection)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('a')), Pane::WorkingTree),
            Some(Action::PreviewWholeTree)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('d')), Pane::WorkingTree),
            Some(Action::Discard)
        );
        assert_eq!(global(press(KeyCode::Char('y'))), Some(Action::Approve));
        assert_eq!(global(press(KeyCode::Char('n'))), Some(Action::Cancel));
        assert_eq!(global(press(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(global(press(KeyCode::Char('a'))), None);
        assert_eq!(global(press(KeyCode::Char('d'))), None);
    }

    #[test]
    fn main_arrows_scroll_horizontally_while_tab_and_h_l_still_move_focus() {
        assert_eq!(
            dispatch(press(KeyCode::Left), Pane::Main),
            Some(Action::HorizontalLeft)
        );
        assert_eq!(
            dispatch(press(KeyCode::Right), Pane::Main),
            Some(Action::HorizontalRight)
        );
        assert_eq!(
            dispatch(press(KeyCode::Left), Pane::Commits),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('h')), Pane::Main),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('l')), Pane::Main),
            Some(Action::FocusNext)
        );
    }

    #[test]
    fn brackets_select_parents_only_in_the_main_pane() {
        assert_eq!(
            dispatch(press(KeyCode::Char('[')), Pane::Main),
            Some(Action::ParentPrev)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char(']')), Pane::Main),
            Some(Action::ParentNext)
        );
        assert_eq!(dispatch(press(KeyCode::Char('[')), Pane::Commits), None);
        assert_eq!(
            dispatch(press(KeyCode::Char(']')), Pane::Repositories),
            None
        );
    }
}
