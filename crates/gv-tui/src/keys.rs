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
//! | `[` / `]` | select the previous/next parent in Main |
//! | `r`, `F5` | refresh |
//! | `a` | approve the open plan review |
//! | `Esc` | refuse/close the open plan review |
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
//! # Where per-pane keys will go
//!
//! Phase 2a's bindings are the same in every pane. The first pane-specific
//! key (#459's `s` to stage, say) adds a layer here that consults the
//! focused pane; the signature grows a `Pane` when that key exists, not
//! before.

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
        KeyCode::Tab | KeyCode::Char('l') => Some(Action::FocusNext),
        KeyCode::BackTab | KeyCode::Char('h') => Some(Action::FocusPrev),
        KeyCode::Right if pane == Pane::Main => Some(Action::HorizontalRight),
        KeyCode::Left if pane == Pane::Main => Some(Action::HorizontalLeft),
        KeyCode::Right => Some(Action::FocusNext),
        KeyCode::Left => Some(Action::FocusPrev),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::CursorDown),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::CursorUp),
        KeyCode::Enter if pane != Pane::Branches => Some(Action::Activate),
        KeyCode::Char('[') if pane == Pane::Main => Some(Action::ParentPrev),
        KeyCode::Char(']') if pane == Pane::Main => Some(Action::ParentNext),
        KeyCode::F(5) | KeyCode::Char('r') => Some(Action::Refresh),
        KeyCode::Char('c') => Some(Action::CancelOperation),
        KeyCode::Char('a') => Some(Action::ApprovePlan),
        KeyCode::Esc => Some(Action::RefusePlan),
        KeyCode::Char(':') => Some(Action::OpenCommand),
        KeyCode::Char(d @ '1'..='9') => Pane::from_number(d.to_digit(10)? as u8).map(Action::Focus),
        _ => None,
    }
}

/// Translate input while the `:` command palette owns the keyboard.
pub fn dispatch_command(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Some(Action::Quit),
        KeyCode::Esc => Some(Action::RefusePlan),
        KeyCode::Enter => Some(Action::SubmitCommand),
        KeyCode::Backspace => Some(Action::CommandBackspace),
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            Some(Action::CommandChar(character))
        }
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
            Some(Action::Focus(Pane::Branches))
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
        for key in [
            press(KeyCode::Char('x')),
            press(KeyCode::Char(' ')),
            press(KeyCode::F(1)),
        ] {
            assert_eq!(global(key), None, "{key:?}");
        }
    }

    #[test]
    fn a_approves_and_escape_refuses_a_plan_review() {
        assert_eq!(global(press(KeyCode::Char('a'))), Some(Action::ApprovePlan));
        assert_eq!(global(press(KeyCode::Esc)), Some(Action::RefusePlan));
        assert_eq!(
            global(press(KeyCode::Char('c'))),
            Some(Action::CancelOperation)
        );
    }

    #[test]
    fn colon_opens_the_palette_and_palette_keys_do_not_leak_navigation() {
        assert_eq!(global(press(KeyCode::Char(':'))), Some(Action::OpenCommand));
        assert_eq!(
            dispatch_command(press(KeyCode::Char('j'))),
            Some(Action::CommandChar('j'))
        );
        assert_eq!(
            dispatch_command(press(KeyCode::Backspace)),
            Some(Action::CommandBackspace)
        );
        assert_eq!(
            dispatch_command(press(KeyCode::Enter)),
            Some(Action::SubmitCommand)
        );
        assert_eq!(
            dispatch_command(press(KeyCode::Esc)),
            Some(Action::RefusePlan)
        );
        assert_eq!(dispatch_command(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn enter_activates_rows_that_open_something_but_not_the_branches_placeholder() {
        let enter = press(KeyCode::Enter);
        for pane in [Pane::Repositories, Pane::Commits, Pane::Main] {
            assert_eq!(dispatch(enter, pane), Some(Action::Activate), "{pane:?}");
        }
        assert_eq!(dispatch(enter, Pane::Branches), None);
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
