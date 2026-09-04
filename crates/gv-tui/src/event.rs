//! The shell event loop and its input seam (M10.02, #457 — phase 2a).
//!
//! [`run`] is generic over both the terminal backend and the two sources of
//! effects: [`Inputs`] and [`DataPort`]. Tests therefore drive the exact loop
//! with finite scripts and a TestBackend—no raw terminal, socket, sleep, or
//! global state. A failed read is ordinary [`crate::app::Data`], folded into
//! the status line; only terminal/input failures end the loop with an error.

use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::app::{App, Viewport};
use crate::data::DataPort;
use crate::{keys, ui};

pub const TICK: Duration = Duration::from_millis(50);

pub enum Input {
    Key(KeyEvent),
    Resize,
    Tick,
}

pub trait Inputs {
    fn next(&mut self, timeout: Duration) -> Result<Input, String>;
}

pub struct CrosstermInputs;

impl Inputs for CrosstermInputs {
    fn next(&mut self, timeout: Duration) -> Result<Input, String> {
        if !event::poll(timeout).map_err(|error| error.to_string())? {
            return Ok(Input::Tick);
        }
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(key) => Ok(Input::Key(key)),
            Event::Resize(_, _) => Ok(Input::Resize),
            _ => Ok(Input::Tick),
        }
    }
}

pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    inputs: &mut dyn Inputs,
    port: &mut dyn DataPort,
) -> Result<(), String> {
    for request in app.start() {
        port.request(request);
    }
    loop {
        // Drawing is also the measurement (#625): the frame reports how many
        // rows each surface had room for, and the app is told before the next
        // key is read. So a page key always moves by the height of the frame
        // the user is looking at — after a resize, and after a zoom, without
        // either having to be re-derived from anything.
        let mut measured = Viewport::default();
        terminal
            .draw(|frame| measured = ui::draw(frame, app))
            .map_err(|error| error.to_string())?;
        app.observe(measured);
        if app.quit {
            break;
        }
        match inputs.next(TICK)? {
            Input::Key(key) => {
                // Three keymaps, and the ORDER of these two arms is a
                // decision rather than a merge artefact. Both #461's command
                // prompt and #462's conflict overlay capture the keyboard so
                // that a printable key is text and not a command; the overlay
                // is checked first because it is a full-screen takeover whose
                // own keymap has no binding that opens a command prompt, so
                // "both at once" is a state the two keymaps cannot produce.
                // If a later slice makes it reachable, the overlay is still
                // the right winner: it is the one holding an unsaved edit.
                let action = if app.conflicts.is_open() {
                    keys::dispatch_conflict(key, app.conflicts.key_mode())
                } else if app.command_input.is_some() {
                    keys::dispatch_command(key)
                } else {
                    keys::dispatch(key, app.focus)
                };
                if let Some(action) = action {
                    for request in app.apply(action) {
                        port.request(request);
                    }
                }
            }
            Input::Tick => {
                for fetch in app.tick() {
                    port.request(fetch);
                }
            }
            Input::Resize => {}
        }
        while let Some(data) = port.poll() {
            for request in app.receive(data) {
                port.request(request);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as SizeCell;
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::rc::Rc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use git_vista_protocol::RepositoryDescriptor;
    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::layout::{Position, Size};

    use super::*;
    use crate::app::{Data, Pane, Request};

    const THREE: &str = r#"[
      {"repository":"r1","worktree":"w1","name":"alpha","kind":"main_worktree","read_only":false},
      {"repository":"r2","worktree":"w2","name":"beta","kind":"bare","read_only":true},
      {"repository":"r3","worktree":"w3","name":"gamma","kind":"linked_worktree","read_only":false}
    ]"#;

    fn catalog(wire: &str) -> Vec<RepositoryDescriptor> {
        serde_json::from_str(wire).expect("the wire literal is valid")
    }

    fn key(c: char) -> Input {
        Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn press(code: KeyCode) -> Input {
        Input::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// More repositories than any page under test, so nothing below is
    /// measuring a clamp at the end of the list instead of a page.
    fn a_long_catalog() -> Vec<RepositoryDescriptor> {
        let entries: Vec<String> = (0..400)
            .map(|i| {
                format!(
                    r#"{{"repository":"r{i}","worktree":"w{i}","name":"repo-{i}","kind":"bare","read_only":true}}"#
                )
            })
            .collect();
        catalog(&format!("[{}]", entries.join(",")))
    }

    /// Drive the real loop at a real terminal size: the catalog arrives, then
    /// `keys` are pressed, then `q`. Answers the Repositories cursor.
    ///
    /// Going through [`run`] rather than calling `App::apply` directly is the
    /// point — the page size only exists because a frame was drawn and
    /// measured, and this is the only way to test that the measurement
    /// reaches the key press.
    fn cursor_after(width: u16, height: u16, keys: Vec<Input>) -> usize {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        let mut app = App::new();
        let mut events: Vec<Result<Input, String>> = vec![Ok(Input::Tick)];
        events.extend(keys.into_iter().map(Ok));
        events.push(Ok(key('q')));
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Ok(a_long_catalog()))]),
        };
        run(&mut terminal, &mut app, &mut Script::new(events), &mut port).unwrap();
        app.cursor(Pane::Repositories)
    }

    type ResizeSignal = (Rc<SizeCell<(u16, u16)>>, (u16, u16));

    struct Script {
        events: VecDeque<Result<Input, String>>,
        resize: Option<ResizeSignal>,
    }

    impl Script {
        fn new(events: impl IntoIterator<Item = Result<Input, String>>) -> Script {
            Script {
                events: events.into_iter().collect(),
                resize: None,
            }
        }

        fn resizing(
            events: impl IntoIterator<Item = Result<Input, String>>,
            size: Rc<SizeCell<(u16, u16)>>,
            target: (u16, u16),
        ) -> Script {
            Script {
                events: events.into_iter().collect(),
                resize: Some((size, target)),
            }
        }
    }

    impl Inputs for Script {
        fn next(&mut self, _timeout: Duration) -> Result<Input, String> {
            let event = self
                .events
                .pop_front()
                .unwrap_or_else(|| Err(String::from("script exhausted before the app quit")))?;
            if matches!(event, Input::Resize) {
                if let Some((size, target)) = &self.resize {
                    size.set(*target);
                }
            }
            Ok(event)
        }
    }

    #[derive(Default)]
    struct FakePort {
        seen: Vec<Request>,
        answers: VecDeque<Data>,
    }

    impl DataPort for FakePort {
        fn request(&mut self, request: Request) {
            self.seen.push(request);
        }

        fn poll(&mut self) -> Option<Data> {
            self.answers.pop_front()
        }
    }

    fn terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).expect("a test terminal")
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let mut out = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                out.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn start_requests_the_catalog_and_q_ends_the_loop_cleanly() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(key('q'))]);
        let mut port = FakePort::default();

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert_eq!(port.seen, [Request::Catalog]);
        assert!(app.quit);
    }

    #[test]
    fn a_failed_read_shows_on_the_status_line_and_the_loop_keeps_running() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(Input::Tick), Ok(key('q'))]);
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Err(String::from(
                "GET /api/catalog answered 503: catalog rebuilding",
            )))]),
        };

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert!(app.quit, "the q after the failed read was still dispatched");
        assert!(buffer_text(terminal.backend().buffer()).contains("503"));
    }

    #[test]
    fn an_answer_is_folded_in_before_the_next_key_is_dispatched() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(Input::Tick), Ok(key('j')), Ok(key('q'))]);
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Ok(catalog(THREE)))]),
        };

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert_eq!(app.cursor(Pane::Repositories), 1);
        assert!(buffer_text(terminal.backend().buffer()).contains("beta"));
    }

    #[test]
    fn r_asks_the_port_again_once_the_first_answer_is_in() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(Input::Tick), Ok(key('r')), Ok(key('q'))]);
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Ok(Vec::new()))]),
        };

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert_eq!(port.seen, [Request::Catalog, Request::Catalog]);
    }

    #[test]
    fn an_input_failure_ends_the_loop_with_its_message_not_a_panic() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Err(String::from("the input stream vanished"))]);
        let mut port = FakePort::default();

        let error = run(&mut terminal, &mut app, &mut script, &mut port).unwrap_err();

        assert_eq!(error, "the input stream vanished");
        assert_eq!(port.seen, [Request::Catalog]);
    }

    /// INVARIANT (#625): one `PageDown` moves by the pane's CURRENT visible
    /// row count, so the same key reaches further in a taller terminal.
    ///
    /// **Two heights, and that is the whole point.** At a single height a
    /// hardcoded page size and a measured one are indistinguishable — and a
    /// constant is exactly the defect this slice would otherwise ship
    /// silently, because it would look right in every small-pane test and be
    /// wrong the moment the zoom key handed that pane the whole window.
    ///
    /// The numbers are the pane's drawn interior: at 80x24 the Repositories
    /// pane is 8 rows with a border top and bottom, at 80x60 it is 20.
    ///
    /// MUTATION 1 (remove): page by one row, as `j` does.
    /// MUTATION 2 (weaken): page by a constant — any constant, since no
    /// single number can satisfy both heights.
    #[test]
    fn a_page_is_the_panes_current_height_so_the_same_key_reaches_further_in_a_taller_terminal() {
        let short = cursor_after(80, 24, vec![press(KeyCode::PageDown)]);
        let tall = cursor_after(80, 60, vec![press(KeyCode::PageDown)]);

        assert_eq!(short, 6, "80x24: the pane shows 6 rows, so a page is 6");
        assert_eq!(tall, 18, "80x60: the pane shows 18 rows, so a page is 18");
        assert!(
            tall > short,
            "a taller terminal must page further; both landed on {short}"
        );
    }

    /// INVARIANT (#625): a maximized pane pages by its NEW, larger height.
    ///
    /// This is the interaction the two halves of the issue have with each
    /// other. A page size measured once, or read from the four-pane layout,
    /// would be right until somebody pressed `z` — and `z` exists precisely
    /// so that the pane is bigger than the layout says.
    ///
    /// MUTATION 1 (remove): ignore `maximized` in `layout::split`.
    /// MUTATION 2 (weaken): measure the viewport once, before the key loop,
    /// instead of after every frame.
    #[test]
    fn zooming_a_pane_makes_its_pages_bigger_in_the_same_terminal() {
        let plain = cursor_after(80, 24, vec![press(KeyCode::PageDown)]);
        let zoomed = cursor_after(80, 24, vec![key('z'), press(KeyCode::PageDown)]);

        assert_eq!(plain, 6, "one third of the left column, less its border");
        assert_eq!(zoomed, 21, "the whole body, less its border");
        assert!(
            zoomed > plain,
            "the zoom key changed nothing about how far a page goes"
        );

        // …and pressing it again puts the small page back, so the toggle is a
        // toggle rather than a one-way door.
        let unzoomed = cursor_after(80, 24, vec![key('z'), key('z'), press(KeyCode::PageDown)]);
        assert_eq!(unzoomed, plain);
    }

    #[test]
    fn home_and_end_reach_the_ends_of_the_list_whatever_the_terminal_size() {
        assert_eq!(cursor_after(80, 24, vec![press(KeyCode::End)]), 399);
        assert_eq!(
            cursor_after(80, 24, vec![press(KeyCode::End), press(KeyCode::Home)]),
            0
        );
        assert_eq!(
            cursor_after(80, 24, vec![press(KeyCode::PageUp)]),
            0,
            "a page up from the top stays at the top rather than wrapping"
        );
    }

    struct ResizeBackend {
        inner: TestBackend,
        size: Rc<SizeCell<(u16, u16)>>,
    }

    impl Backend for ResizeBackend {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.draw(content)
        }

        fn append_lines(&mut self, count: u16) -> Result<(), Self::Error> {
            self.inner.append_lines(count)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            let (width, height) = self.size.get();
            if self.inner.size()? != Size::new(width, height) {
                self.inner.resize(width, height);
            }
            self.inner.flush()
        }
    }

    #[test]
    fn a_resize_is_drawn_at_the_new_size() {
        let size = Rc::new(SizeCell::new((80, 24)));
        let backend = ResizeBackend {
            inner: TestBackend::new(80, 24),
            size: Rc::clone(&size),
        };
        let mut terminal = Terminal::new(backend).expect("a resizing test terminal");
        let mut app = App::new();
        let mut script =
            Script::resizing([Ok(Input::Resize), Ok(key('q'))], Rc::clone(&size), (30, 8));
        let mut port = FakePort::default();

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        let buffer = terminal.backend().inner.buffer();
        assert_eq!(buffer.area.as_size(), Size::new(30, 8));
        assert!(buffer_text(buffer).contains("40x10"));
    }
}
