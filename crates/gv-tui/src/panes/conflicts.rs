//! Conflict inspection and resolution in the terminal (M10.07, #462).
//!
//! Pure. No ratatui, no socket, no thread: [`ConflictsPane`] holds what the
//! overlay shows, [`ConflictsPane::apply`] folds one [`Act`] in and returns
//! the [`Request`]s the shell must dispatch, and [`ConflictsPane::receive_*`]
//! fold answers back. Every rule below is therefore an ordinary `#[test]`
//! with nothing but a struct in sight — the same placement argument
//! `git-vista-conflicts` makes for the model this module draws.
//!
//! # It draws the M4.31 model; it does not re-decide anything
//!
//! #462's governing sentence is "using the same model M4.31 shipped — not a
//! second conflict implementation", so every judgement below is delegated:
//!
//! | question | who answers |
//! |---|---|
//! | what state is this pane in | [`PaneState::for_stage`], [`PaneState::with_content`], [`result_pane_state`] |
//! | what does that state say to a user | [`PaneState::describe`] |
//! | what is this pane called | [`View::label`] |
//! | may `Take ours` be offered, and if not why | [`ResolutionSurface::take_ours`] and [`Withheld::describe`] |
//! | what shape of conflict is this | [`ResolutionSurface::note`] |
//! | may a line-level resolver open at all | [`ResolutionSurface::text_resolution_allowed`] |
//! | what blocks does the marker file hold | [`markers::parse`] |
//! | what content does a set of choices produce | [`markers::compose`] |
//!
//! Nothing in this file recomputes any of those. In particular the eligibility
//! predicate is **read**, never re-derived: `text_resolution_allowed` traces
//! back to `ConflictedFile::text_resolvable`, the same call the server makes
//! before executing a content resolution. #430 shipped a wrong sentence
//! because that rule had two implementations; there is still one.
//!
//! # Why the four panes are a summary strip plus one body, not a 2×2 grid
//!
//! The frame's floor is 40 columns ([`crate::layout::MIN_WIDTH`]). Quartered,
//! that is 20 columns of source per pane — narrow enough that every line of
//! every version is truncated, and the user is reading the *shape* of four
//! boxes rather than their contents. That is the same failure as a pane that
//! draws an empty box: it looks like inspection and is not.
//!
//! So all four panes are always **stated** — one row each, carrying that
//! pane's own [`PaneState::describe`] sentence, so "there is no ancestor" is
//! on screen the whole time — and the focused one is shown full width.
//! `Tab` and `1`–`4` move between them. Every pane stays reachable, which is
//! what #428's first criterion asks, and none of them is ever a blank box.
//!
//! # An absent thing says so, everywhere, including inside the editor
//!
//! A `Block::Conflict` whose `base` is `None` is a merge-style marker file
//! with no recorded ancestor. It renders as that sentence, never as an empty
//! ancestor section — the same distinction ADR 0063 draws for stages, applied
//! one layer in.

use git_vista_conflicts::core::{
    result_pane_state, ConflictPanes, Pane as View, PaneState, ResolutionSurface, ResultRead,
};
use git_vista_conflicts::markers::{self, Block, Choice};
use git_vista_core::diff::BlobContent;
use git_vista_protocol::conflict::{ConflictedFile, Resolution};
use git_vista_protocol::{CommitOid, ConflictSource, GenerationToken};

/// Which screen of the overlay is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// Every conflicted path.
    #[default]
    List,
    /// One path's four panes and the resolutions offered for it.
    Inspect,
    /// One path's marker blocks, the composed result, and the free-text edit.
    Editor,
}

/// What key dispatch needs to know, and nothing else.
///
/// A separate, tiny enum rather than handing `keys.rs` the whole pane: the
/// only thing a keymap may depend on is which bindings are live, and
/// [`KeyMode::Insert`] is the one that matters most — while it is on, `q` is
/// a letter the user typed, not a request to quit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    List,
    Inspect,
    Editor,
    Insert,
}

/// A read or write the shell must dispatch. The pane names the path; the
/// shell adds the repository, which is the shell's to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `GET /api/conflicts`.
    Conflicts,
    /// `GET /api/blob/{oid}` for one stage of one path.
    Stage {
        path: String,
        pane: View,
        oid: String,
    },
    /// `GET /api/worktree-file/{*path}` — the result pane.
    Result { path: String },
    /// `GET /api/conflict-source/{*path}` — the marker file and its token.
    Source { path: String },
    /// `POST /api/resolve-conflict` — a whole side, or the deletion.
    ResolveWholeFile {
        path: String,
        resolution: Resolution,
    },
    /// `POST /api/resolve-conflict-content` (ADR 0069).
    ResolveContent {
        path: String,
        expected_stages: [Option<CommitOid>; 3],
        expected_source: GenerationToken,
        content: String,
    },
}

/// One key press, after `keys.rs` has translated it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Close,
    Back,
    Refresh,
    Down,
    Up,
    /// One visible page of the overlay body, and its two ends (#625).
    PageDown,
    PageUp,
    Top,
    Bottom,
    Open,
    FocusPane(View),
    NextPane,
    Take(Resolution),
    OpenEditor,
    Choose(Choice),
    BeginEdit,
    EndEdit,
    Apply,
    Type(char),
    Backspace,
    Newline,
    CaretLeft,
    CaretRight,
}

/// Which end of a screen [`Act::Top`] and [`Act::Bottom`] mean (#625).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Start,
    End,
}

/// How one row of the overlay body is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Heading,
    Plain,
    /// A sentence the model produced about state or about what is on offer:
    /// "Not present on this side", a conflict's shape note, a withheld
    /// control's reason.
    ///
    /// Its own tone rather than sharing [`Tone::Muted`], and the difference is
    /// not cosmetic. These sentences ARE the content on the screens where they
    /// appear — "there is no ancestor" is what criterion 1 asks the terminal to
    /// say — so they are drawn at full weight. Dimming them would leave the
    /// most important line on the pane as the faintest thing on it.
    State,
    /// Genuinely secondary text: file context between conflicts, the ancestor
    /// body.
    Muted,
    /// Something that failed — an unreadable stage, a refused write.
    Fault,
    Ours,
    Theirs,
}

/// One logical row. Only the visible window of these is ever built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub tone: Tone,
    /// The cursor sits on this row: the list selection, or the block the
    /// editor's choice keys act on.
    pub selected: bool,
    /// Column of the text caret on this row, while hand-editing.
    pub caret: Option<usize>,
}

fn row(text: impl Into<String>, tone: Tone) -> Row {
    Row {
        text: text.into(),
        tone,
        selected: false,
        caret: None,
    }
}

fn selected_row(text: impl Into<String>, tone: Tone) -> Row {
    Row {
        selected: true,
        ..row(text, tone)
    }
}

/// A minimal multi-line text buffer: what the user typed, and where the
/// caret is.
///
/// `caret` is a **byte** offset that is always on a `char` boundary — every
/// mutation and every move goes through this type, and none of them can
/// produce an interior index. A `usize` counting characters would be simpler
/// to move but would make every insert an O(n) scan and every render a second
/// one; this way the slice operations are the cheap ones and only the
/// vertical moves walk the line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextEdit {
    text: String,
    caret: usize,
}

impl TextEdit {
    pub fn new(text: String) -> TextEdit {
        let caret = text.len();
        TextEdit { text, caret }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// `(line, column)` of the caret, both zero-based and both in characters
    /// — a renderer places a cell, not a byte.
    pub fn position(&self) -> (usize, usize) {
        let before = &self.text[..self.caret];
        let line = before.matches('\n').count();
        let column = before
            .rfind('\n')
            .map_or(before, |nl| &before[nl + 1..])
            .chars()
            .count();
        (line, column)
    }

    pub fn insert(&mut self, ch: char) {
        self.text.insert(self.caret, ch);
        self.caret += ch.len_utf8();
    }

    /// Delete the character before the caret. At the very start this does
    /// nothing rather than wrapping — there is nothing before the first byte.
    pub fn backspace(&mut self) {
        let Some((at, ch)) = self.text[..self.caret].char_indices().next_back() else {
            return;
        };
        self.text.replace_range(at..at + ch.len_utf8(), "");
        self.caret = at;
    }

    pub fn left(&mut self) {
        if let Some((at, _)) = self.text[..self.caret].char_indices().next_back() {
            self.caret = at;
        }
    }

    pub fn right(&mut self) {
        if let Some(ch) = self.text[self.caret..].chars().next() {
            self.caret += ch.len_utf8();
        }
    }

    /// Move one display line, keeping the column where it can. `delta` is
    /// -1 or 1; anything past the first or last line stays put.
    pub fn vertical(&mut self, delta: isize) {
        let (line, column) = self.position();
        let Some(target) = line.checked_add_signed(delta) else {
            return;
        };
        let lines: Vec<&str> = self.line_slices();
        let Some(row) = lines.get(target) else {
            return;
        };
        let start = row.as_ptr() as usize - self.text.as_ptr() as usize;
        let offset = row
            .char_indices()
            .nth(column)
            .map_or(row.len(), |(index, _)| index);
        self.caret = start + offset;
    }

    /// The buffer's lines as slices of it, with a trailing empty line when
    /// the text ends in a newline — a caret can legitimately sit there, and a
    /// splitter that dropped it would refuse to move onto the last line the
    /// user just made.
    fn line_slices(&self) -> Vec<&str> {
        let mut lines: Vec<&str> = self.text.split('\n').collect();
        if lines.is_empty() {
            lines.push("");
        }
        lines
    }
}

/// A path's four panes, once one has been opened for inspection.
#[derive(Clone, Debug)]
struct Inspect {
    path: String,
    panes: ConflictPanes,
    focus: View,
}

/// The line-level resolver's state for one path.
#[derive(Clone, Debug)]
struct Editor {
    path: String,
    source: ConflictSource,
    blocks: Vec<Block>,
    choices: Vec<Choice>,
    /// Which conflict ordinal the choice keys act on.
    block: usize,
    /// The free-text buffer. `Some` from the moment the user opens it.
    buffer: Option<TextEdit>,
    /// True once a keystroke actually changed the buffer.
    ///
    /// Separate from `buffer.is_some()` deliberately: merely *looking* at the
    /// composed text must not freeze the block buttons, and a typed character
    /// must. Criterion 4 lives on this flag.
    hand_edited: bool,
    /// Insert mode: every printable key is a character, not a command.
    inserting: bool,
}

/// The conflict overlay's whole state.
#[derive(Debug, Default)]
pub struct ConflictsPane {
    open: bool,
    repo: Option<String>,
    screen: Screen,
    /// `None` while the first read is out.
    files: Option<Result<Vec<ConflictedFile>, String>>,
    cursor: usize,
    scroll: usize,
    inspect: Option<Inspect>,
    editor: Option<Editor>,
    /// A write is out; the controls are inert until it answers.
    busy: bool,
    /// The last thing worth saying: a refusal, a server sentence, a success.
    message: Option<(String, bool)>,
    /// Rows the overlay's body had in the last drawn frame (#625). The
    /// overlay owns the whole window, so this is the biggest page in the
    /// program — and it changes with the terminal, which is why it is
    /// observed rather than assumed.
    viewport: usize,
}

impl ConflictsPane {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn key_mode(&self) -> KeyMode {
        match self.screen {
            Screen::List => KeyMode::List,
            Screen::Inspect => KeyMode::Inspect,
            Screen::Editor if self.editor.as_ref().is_some_and(|e| e.inserting) => KeyMode::Insert,
            Screen::Editor => KeyMode::Editor,
        }
    }

    /// Open the overlay on `repo` and ask for its conflicts.
    pub fn open(&mut self, repo: String) -> Vec<Request> {
        let same_repo = self.repo.as_deref() == Some(repo.as_str());
        self.open = true;
        self.screen = Screen::List;
        self.message = None;
        self.busy = false;
        self.scroll = 0;
        if !same_repo {
            self.cursor = 0;
            self.inspect = None;
            self.editor = None;
        }
        self.repo = Some(repo);
        self.files = None;
        vec![Request::Conflicts]
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// The repository this view is bound to. An answer for any other one is
    /// dropped — the same request-key discipline `DetailPane` uses.
    pub fn repo(&self) -> Option<&str> {
        self.repo.as_deref()
    }

    pub fn message(&self) -> Option<(&str, bool)> {
        self.message.as_ref().map(|(t, e)| (t.as_str(), *e))
    }

    /// The keys live on the current screen, for the status line.
    pub fn hint(&self) -> String {
        match self.key_mode() {
            KeyMode::List => {
                String::from("j/k move · Enter inspect · r refresh · Esc close")
            }
            KeyMode::Inspect => String::from(
                "Tab/1-4 pane · j/k scroll · o ours · t theirs · d delete · e line editor · Esc back",
            ),
            KeyMode::Editor => String::from(
                "j/k block · o/t/b choose · i edit text · Enter apply · Esc back",
            ),
            KeyMode::Insert => String::from("typing — Esc leaves the text buffer"),
        }
    }

    // ---- folding answers in ------------------------------------------

    pub fn receive_conflicts(&mut self, repo: &str, result: Result<Vec<ConflictedFile>, String>) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        if let Err(message) = &result {
            self.message = Some((message.clone(), true));
        }
        self.files = Some(result);
        self.clamp_cursor();
    }

    pub fn receive_stage(
        &mut self,
        repo: &str,
        path: &str,
        pane: View,
        result: Result<BlobContent, String>,
    ) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        let Some(inspect) = self.inspect.as_mut().filter(|i| i.path == path) else {
            return;
        };
        let slot = inspect.panes.pane_mut(pane);
        *slot = slot.clone().with_content(result);
    }

    pub fn receive_result(&mut self, repo: &str, path: &str, read: ResultRead) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        let Some(inspect) = self.inspect.as_mut().filter(|i| i.path == path) else {
            return;
        };
        inspect.panes.result = result_pane_state(read);
    }

    pub fn receive_source(
        &mut self,
        repo: &str,
        path: &str,
        result: Result<ConflictSource, String>,
    ) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        if self.inspect.as_ref().map(|i| i.path.as_str()) != Some(path) {
            return;
        }
        self.busy = false;
        match result {
            Err(message) => self.message = Some((message, true)),
            Ok(source) => {
                let blocks = markers::parse(&source.content);
                let choices = vec![Choice::Unchosen; markers::conflict_count(&blocks)];
                self.editor = Some(Editor {
                    path: path.to_string(),
                    source,
                    blocks,
                    choices,
                    block: 0,
                    buffer: None,
                    hand_edited: false,
                    inserting: false,
                });
                self.screen = Screen::Editor;
                self.scroll = 0;
                self.message = None;
            }
        }
    }

    /// A resolution came back. Success sends the view back to a fresh list —
    /// the path it was showing is, by construction, no longer conflicted.
    pub fn receive_resolved(&mut self, repo: &str, path: &str, result: Result<(), String>) -> bool {
        if self.repo.as_deref() != Some(repo) {
            return false;
        }
        self.busy = false;
        match result {
            Err(message) => {
                // The server's own sentence, kept whole. The four content
                // refusals name four different things that moved, and
                // collapsing them into "it failed" throws away the only part
                // that says what to do next.
                self.message = Some((message, true));
                false
            }
            Ok(()) => {
                self.message = Some((format!("resolved {path}"), false));
                self.inspect = None;
                self.editor = None;
                self.screen = Screen::List;
                self.scroll = 0;
                self.files = None;
                true
            }
        }
    }

    // ---- folding key presses in --------------------------------------

    pub fn apply(&mut self, act: Act) -> Vec<Request> {
        match act {
            Act::Close => {
                self.close();
                Vec::new()
            }
            Act::Back => self.back(),
            Act::Refresh => {
                self.files = None;
                self.message = None;
                vec![Request::Conflicts]
            }
            Act::Down => self.move_cursor(1),
            Act::Up => self.move_cursor(-1),
            Act::PageDown => {
                let page = self.page();
                self.move_cursor(page)
            }
            Act::PageUp => {
                let page = self.page();
                self.move_cursor(-page)
            }
            Act::Top => self.jump(Edge::Start),
            Act::Bottom => self.jump(Edge::End),
            Act::Open => self.open_selected(),
            Act::NextPane => {
                if let Some(inspect) = self.inspect.as_mut() {
                    let next = (View::ALL
                        .iter()
                        .position(|p| *p == inspect.focus)
                        .expect("every pane is in ALL")
                        + 1)
                        % View::ALL.len();
                    inspect.focus = View::ALL[next];
                    self.scroll = 0;
                }
                Vec::new()
            }
            Act::FocusPane(pane) => {
                if let Some(inspect) = self.inspect.as_mut() {
                    inspect.focus = pane;
                    self.scroll = 0;
                }
                Vec::new()
            }
            Act::Take(resolution) => self.take(resolution),
            Act::OpenEditor => self.open_editor(),
            Act::Choose(choice) => {
                self.choose(choice);
                Vec::new()
            }
            Act::BeginEdit => {
                self.begin_edit();
                Vec::new()
            }
            Act::EndEdit => {
                if let Some(editor) = self.editor.as_mut() {
                    editor.inserting = false;
                }
                Vec::new()
            }
            Act::Apply => self.apply_content(),
            Act::Type(ch) => {
                self.edit(|buffer| buffer.insert(ch));
                Vec::new()
            }
            Act::Newline => {
                self.edit(|buffer| buffer.insert('\n'));
                Vec::new()
            }
            Act::Backspace => {
                self.edit(TextEdit::backspace);
                Vec::new()
            }
            Act::CaretLeft => {
                self.move_caret(TextEdit::left);
                Vec::new()
            }
            Act::CaretRight => {
                self.move_caret(TextEdit::right);
                Vec::new()
            }
        }
    }

    fn back(&mut self) -> Vec<Request> {
        match self.screen {
            Screen::List => self.close(),
            Screen::Inspect => {
                self.screen = Screen::List;
                self.scroll = 0;
            }
            Screen::Editor => {
                self.editor = None;
                self.screen = Screen::Inspect;
                self.scroll = 0;
            }
        }
        Vec::new()
    }

    /// Record the overlay body's height from the frame just drawn (#625).
    pub fn observe(&mut self, rows: usize) {
        self.viewport = rows;
    }

    /// One page of the overlay, never zero.
    fn page(&self) -> isize {
        // A terminal deeper than `isize::MAX` rows is not a case; the
        // saturating cast is here so no arithmetic in this file can panic.
        self.viewport.max(1).min(isize::MAX as usize) as isize
    }

    /// Jump to the first or last row of whatever this screen is showing.
    fn jump(&mut self, edge: Edge) -> Vec<Request> {
        let target = match edge {
            Edge::Start => 0,
            Edge::End => usize::MAX,
        };
        match self.screen {
            Screen::List => {
                let last = self.files_ref().map_or(0, <[_]>::len).saturating_sub(1);
                self.cursor = target.min(last);
            }
            Screen::Inspect => {
                self.scroll = target.min(self.row_count().saturating_sub(1));
            }
            Screen::Editor => {
                if let Some(editor) = self.editor.as_mut() {
                    if editor.inserting {
                        return Vec::new();
                    }
                    let last = editor.choices.len().saturating_sub(1);
                    editor.block = target.min(last);
                }
            }
        }
        Vec::new()
    }

    fn move_cursor(&mut self, delta: isize) -> Vec<Request> {
        match self.screen {
            Screen::List => {
                let last = self.files_ref().map_or(0, <[_]>::len).saturating_sub(1);
                self.cursor = self.cursor.saturating_add_signed(delta).min(last);
            }
            Screen::Inspect => {
                // Clamped to the rows there are, not left to run off the end
                // and be clamped again at draw time. Unbounded, a `PageDown`
                // held down would build a scroll position hundreds of pages
                // past the file, and the first `PageUp` after it would appear
                // to do nothing at all.
                self.scroll = self
                    .scroll
                    .saturating_add_signed(delta)
                    .min(self.row_count().saturating_sub(1));
            }
            Screen::Editor => {
                if let Some(editor) = self.editor.as_mut() {
                    if editor.inserting {
                        if let Some(buffer) = editor.buffer.as_mut() {
                            buffer.vertical(delta);
                        }
                        return Vec::new();
                    }
                    let last = editor.choices.len().saturating_sub(1);
                    editor.block = editor.block.saturating_add_signed(delta).min(last);
                }
            }
        }
        Vec::new()
    }

    fn open_selected(&mut self) -> Vec<Request> {
        if self.screen == Screen::Editor {
            return self.apply_content();
        }
        if self.screen != Screen::List {
            return Vec::new();
        }
        let Some(file) = self
            .files_ref()
            .and_then(|files| files.get(self.cursor))
            .cloned()
        else {
            return Vec::new();
        };
        let panes = ConflictPanes::open(&file);
        // Only a pane the model says is awaiting content is fetched. A binary
        // or absent side resolves from metadata alone, so a conflict with a
        // 200 MB binary side costs one listing, not a download — the same rule
        // the browser client's assembler follows.
        let mut requests = vec![Request::Result {
            path: file.path.clone(),
        }];
        for pane in [View::Base, View::Ours, View::Theirs] {
            if let PaneState::AwaitingContent { oid } = panes.pane(pane) {
                requests.push(Request::Stage {
                    path: file.path.clone(),
                    pane,
                    oid: oid.clone(),
                });
            }
        }
        self.inspect = Some(Inspect {
            path: file.path.clone(),
            panes,
            focus: View::Ours,
        });
        self.editor = None;
        self.screen = Screen::Inspect;
        self.scroll = 0;
        self.message = None;
        requests
    }

    /// A whole-side resolution, or the deletion.
    ///
    /// Refused here when the model withheld the control, in the model's own
    /// words. The row for a withheld control carries no key, so this is the
    /// second half of the same answer rather than the only one: a user who
    /// presses the key anyway is told why, not walked into a 409.
    fn take(&mut self, resolution: Resolution) -> Vec<Request> {
        if self.busy {
            return Vec::new();
        }
        let Some(inspect) = self.inspect.as_ref() else {
            return Vec::new();
        };
        let offer = match resolution {
            Resolution::TakeOurs => &inspect.panes.surface.take_ours,
            Resolution::TakeTheirs => &inspect.panes.surface.take_theirs,
            Resolution::TakeDeletion => &inspect.panes.surface.take_deletion,
        };
        if let Err(withheld) = offer {
            self.message = Some((withheld.describe(), true));
            return Vec::new();
        }
        let path = inspect.path.clone();
        self.busy = true;
        self.message = None;
        vec![Request::ResolveWholeFile { path, resolution }]
    }

    fn open_editor(&mut self) -> Vec<Request> {
        if self.busy {
            return Vec::new();
        }
        let Some(inspect) = self.inspect.as_ref() else {
            return Vec::new();
        };
        // Read from the model, never recomputed. This flag is
        // `ConflictedFile::text_resolvable` — the identical question the
        // server asks before executing a content resolution — so the editor
        // cannot open on a file the executor would refuse.
        if !inspect.panes.surface.text_resolution_allowed {
            self.message =
                Some((
                    inspect.panes.surface.note.clone().unwrap_or_else(|| {
                        String::from("This file cannot be resolved line by line.")
                    }),
                    true,
                ));
            return Vec::new();
        }
        let path = inspect.path.clone();
        self.busy = true;
        self.message = None;
        vec![Request::Source { path }]
    }

    /// Choose a side for the block under the cursor.
    ///
    /// **A no-op once the text has been hand-edited**, which is criterion 4:
    /// re-composing from the buttons would throw the user's typing away
    /// without a word, and silently discarding what somebody typed is the
    /// worst failure available in an editor. The buttons go inert instead,
    /// and the row says so.
    fn choose(&mut self, choice: Choice) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.hand_edited {
            self.message = Some((
                String::from(
                    "the text has been edited by hand — block choices no longer change it",
                ),
                false,
            ));
            return;
        }
        if let Some(slot) = editor.choices.get_mut(editor.block) {
            *slot = choice;
        }
    }

    fn begin_edit(&mut self) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.inserting = true;
        // Re-seeded on every entry UNTIL the first real edit, and never after
        // it. Before a hand-edit the buffer is only a view of the current
        // composition, so opening it again after changing a block must show
        // that change; after one, it is the user's own text and re-seeding it
        // would be the silent discard criterion 4 exists to forbid.
        if editor.hand_edited {
            return;
        }
        // Seeded from the composition when there is one, and otherwise from
        // the marker file git actually wrote. Never from empty: an empty
        // buffer would offer "delete everything in this file" as the starting
        // point of a resolution.
        let seed = markers::compose(&editor.blocks, &editor.choices)
            .unwrap_or_else(|| editor.source.content.clone());
        editor.buffer = Some(TextEdit::new(seed));
    }

    fn edit(&mut self, change: impl FnOnce(&mut TextEdit)) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if !editor.inserting {
            return;
        }
        let Some(buffer) = editor.buffer.as_mut() else {
            return;
        };
        change(buffer);
        editor.hand_edited = true;
    }

    fn move_caret(&mut self, change: impl FnOnce(&mut TextEdit)) {
        let Some(buffer) = self
            .editor
            .as_mut()
            .filter(|editor| editor.inserting)
            .and_then(|editor| editor.buffer.as_mut())
        else {
            return;
        };
        change(buffer);
    }

    /// Submit the line-level resolution.
    ///
    /// `expected_stages` and `expected_source` are echoed back exactly as the
    /// server served them and are never recomputed here — a client that
    /// computed its own could only ever agree with itself, and ADR 0069's
    /// third and fourth gates would then prove nothing.
    fn apply_content(&mut self) -> Vec<Request> {
        if self.busy {
            return Vec::new();
        }
        let Some(editor) = self.editor.as_ref() else {
            return Vec::new();
        };
        let composed = if editor.hand_edited {
            editor
                .buffer
                .as_ref()
                .map(|buffer| buffer.text().to_string())
        } else {
            markers::compose(&editor.blocks, &editor.choices)
        };
        let Some(content) = composed else {
            let open = markers::unchosen(&editor.blocks, &editor.choices).len();
            self.message = Some((
                format!("{open} conflict(s) still need a choice before this can be applied"),
                true,
            ));
            return Vec::new();
        };
        let request = Request::ResolveContent {
            path: editor.path.clone(),
            expected_stages: editor.source.stages.clone(),
            expected_source: editor.source.source.clone(),
            content,
        };
        self.busy = true;
        self.message = None;
        vec![request]
    }

    fn files_ref(&self) -> Option<&[ConflictedFile]> {
        match self.files.as_ref() {
            Some(Ok(files)) => Some(files),
            _ => None,
        }
    }

    fn clamp_cursor(&mut self) {
        let last = self.files_ref().map_or(0, <[_]>::len).saturating_sub(1);
        self.cursor = self.cursor.min(last);
    }
}

// ---- the row projection ----------------------------------------------
//
// Rows are produced through a visitor and only the visible window of them is
// ever materialized, the same shape `DetailPane` uses and for the same
// reason: a conflict side is bounded by the server's content cap, not by the
// height of a terminal, and building thirty thousand `String`s per redraw to
// show forty of them is how a viewer becomes unusable on exactly the file
// that matters most.
//
// `row_count` is therefore arithmetic rather than a counting visit. That is a
// second implementation of the layout, and the honest way to hold two
// implementations of one thing is to pin them against each other:
// `row_count_agrees_with_the_rows_actually_emitted` fails the moment they
// drift, on every screen.

impl ConflictsPane {
    /// How many rows the body has, for scroll clamping.
    pub fn row_count(&self) -> usize {
        match self.screen {
            Screen::List => 1 + self.list_body_len(),
            Screen::Inspect => self.inspect.as_ref().map_or(1, inspect_row_count),
            Screen::Editor => self.editor.as_ref().map_or(1, editor_row_count),
        }
    }

    /// The rows in `[offset, offset + limit)`.
    pub fn window(&self, offset: usize, limit: usize) -> Vec<Row> {
        let mut window = RowWindow::new(offset, limit);
        self.visit_rows(|row| window.push(row));
        window.rows
    }

    /// Where the body should be scrolled to for a `height`-row viewport:
    /// the user's own scroll, clamped to the end, and pulled far enough to
    /// keep this screen's selection on screen.
    ///
    /// A caret the viewport has scrolled away from is a caret the user cannot
    /// see, and an invisible caret in a buffer that accepts every keystroke is
    /// a way to edit the wrong line of a file you are about to write. The file
    /// list has the same hazard with a slower fuse: `End` in a list longer than
    /// the overlay moves the cursor and nothing else, so the highlight leaves
    /// the window entirely and `Enter` opens a path that is nowhere on screen.
    pub fn view_offset(&self, height: usize) -> usize {
        let max = self.row_count().saturating_sub(height);
        let offset = self.scroll.min(max);
        let Some(focus) = self.focus_row() else {
            return offset;
        };
        if height == 0 {
            return offset;
        }
        if focus < offset {
            focus
        } else if focus >= offset + height {
            focus + 1 - height
        } else {
            offset
        }
    }

    /// The absolute row index of the thing the viewport must not lose: the
    /// file cursor on the list, the text caret while hand-editing, and the
    /// block cursor on the editor the rest of the time.
    ///
    /// `None` only on `Inspect`, which has no cursor of its own — there the
    /// user's own `scroll` *is* the cursor and follows itself.
    ///
    /// # The editor answers twice, and the caret wins
    ///
    /// In insert mode `visit_editor` marks **two** rows selected: the block
    /// heading the cursor is on, and the line the caret is on down in the
    /// Result section. They are both real, and the one the viewport must not
    /// lose is the caret — that is the buffer taking keystrokes. So
    /// [`Self::caret_row`] is asked first and only falls through to the block
    /// heading when there is no caret to follow.
    fn focus_row(&self) -> Option<usize> {
        match self.screen {
            Screen::List => {
                // Row 0 is the heading; file `n` is drawn at row `n + 1`.
                let last = self.files_ref().filter(|f| !f.is_empty())?.len() - 1;
                Some(1 + self.cursor.min(last))
            }
            Screen::Inspect => None,
            Screen::Editor => self.caret_row().or_else(|| self.selected_row_index()),
        }
    }

    /// The absolute row index the text caret sits on, while hand-editing.
    fn caret_row(&self) -> Option<usize> {
        let editor = self.editor.as_ref()?;
        if !editor.inserting {
            return None;
        }
        let (line, _) = editor.buffer.as_ref()?.position();
        Some(editor_result_first_row(editor) + line)
    }

    /// Where the first row this screen marks selected is drawn — **found in
    /// the rows the screen actually emits**, not recomputed from the widths
    /// of the blocks above it.
    ///
    /// # Why this walks instead of doing the arithmetic (#634)
    ///
    /// The obvious implementation is `4 + the widths of every block before
    /// this one`, and it would have been the **third** copy of the editor's
    /// row arithmetic: `editor_result_first_row` and `visit_editor` are
    /// already the two that the comment above [`Self::row_count`] warns about
    /// and that `row_count_agrees_with_the_rows_actually_emitted_on_every_screen`
    /// exists to hold together. A third could drift from both, and a
    /// selection offset by one block is a viewport that scrolls to the wrong
    /// conflict while looking entirely correct.
    ///
    /// Walking removes the possibility rather than testing for it: this
    /// returns an index *into* `visit_rows`' own output, so it cannot
    /// disagree with what is drawn. It stops at the first selected row, and
    /// the caller decides which selection matters when a screen marks more
    /// than one.
    fn selected_row_index(&self) -> Option<usize> {
        let mut index = 0usize;
        let mut found = None;
        self.visit_rows(|row| {
            if row.selected {
                found = Some(index);
                return false;
            }
            index += 1;
            true
        });
        found
    }

    fn list_body_len(&self) -> usize {
        match self.files.as_ref() {
            None | Some(Err(_)) => 1,
            Some(Ok(files)) if files.is_empty() => 1,
            Some(Ok(files)) => files.len(),
        }
    }

    fn visit_rows(&self, mut emit: impl FnMut(Row) -> bool) {
        match self.screen {
            Screen::List => self.visit_list(&mut emit),
            Screen::Inspect => match self.inspect.as_ref() {
                Some(inspect) => visit_inspect(inspect, &mut emit),
                None => {
                    let _ = emit(row("No conflict is open.", Tone::Muted));
                }
            },
            Screen::Editor => match self.editor.as_ref() {
                Some(editor) => visit_editor(editor, &mut emit),
                None => {
                    let _ = emit(row("No conflict is open.", Tone::Muted));
                }
            },
        }
    }

    fn visit_list(&self, emit: &mut impl FnMut(Row) -> bool) {
        let heading = match self.files.as_ref() {
            None => String::from("Conflicts — loading…"),
            Some(Err(_)) => String::from("Conflicts — could not be read"),
            Some(Ok(files)) => format!(
                "Conflicts — {} path{}",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
        };
        if !emit(row(heading, Tone::Heading)) {
            return;
        }
        match self.files.as_ref() {
            None => {
                let _ = emit(row("Reading the index…", Tone::State));
            }
            Some(Err(message)) => {
                let _ = emit(row(message.clone(), Tone::Fault));
            }
            Some(Ok(files)) if files.is_empty() => {
                // Said plainly rather than left blank: an empty body would be
                // indistinguishable from a list that failed to draw.
                let _ = emit(row(
                    "Nothing is conflicted in this repository.",
                    Tone::State,
                ));
            }
            Some(Ok(files)) => {
                for (index, file) in files.iter().enumerate() {
                    // The note is the model's own sentence about this
                    // conflict's shape — binary, delete/modify, or a side that
                    // could not be read. `None` for an ordinary text conflict,
                    // deliberately: a note on every row would train the eye to
                    // skip the rows that have something to say.
                    let surface = ResolutionSurface::of(file);
                    let text = match surface.note {
                        Some(note) => format!("{}  —  {note}", file.path),
                        None => file.path.clone(),
                    };
                    let line = if index == self.cursor {
                        selected_row(text, Tone::Plain)
                    } else {
                        row(text, Tone::Plain)
                    };
                    if !emit(line) {
                        return;
                    }
                }
            }
        }
    }
}

fn inspect_row_count(inspect: &Inspect) -> usize {
    // path + [note] + blank + four summary rows + blank + focus heading
    // + content + blank + "Resolutions" + three controls + the editor row.
    let note = usize::from(inspect.panes.surface.note.is_some());
    1 + note + 1 + 4 + 1 + 1 + content_row_count(inspect.panes.pane(inspect.focus)) + 1 + 1 + 3 + 1
}

/// How many rows one pane's body takes.
///
/// Every non-`Text` state is exactly one row — its own sentence. `Text` with
/// no content is also one row, and it says the version is empty rather than
/// drawing a blank: `Text { content: "" }` is the one state entitled to claim
/// "this version was blank" (ADR 0063), and a claim nobody can see is not
/// made.
fn content_row_count(state: &PaneState) -> usize {
    match state {
        PaneState::Text { content, .. } if !content.is_empty() => content.lines().count().max(1),
        _ => 1,
    }
}

fn visit_content(state: &PaneState, emit: &mut impl FnMut(Row) -> bool) -> bool {
    match state {
        PaneState::Text { content, .. } if !content.is_empty() => {
            for line in content.lines() {
                if !emit(row(line, Tone::Plain)) {
                    return false;
                }
            }
            true
        }
        PaneState::Text { .. } => emit(row("(this version is empty)", Tone::State)),
        // Absent, Unreadable, Binary, AwaitingContent and ContentUnavailable
        // each say what they are, in the model's own words. The two that are
        // faults rather than facts are marked as such.
        other => {
            let tone = match other {
                PaneState::Unreadable { .. } | PaneState::ContentUnavailable { .. } => Tone::Fault,
                _ => Tone::State,
            };
            emit(row(other.describe(), tone))
        }
    }
}

fn visit_inspect(inspect: &Inspect, emit: &mut impl FnMut(Row) -> bool) {
    if !emit(row(inspect.path.clone(), Tone::Heading)) {
        return;
    }
    if let Some(note) = &inspect.panes.surface.note {
        if !emit(row(note.clone(), Tone::State)) {
            return;
        }
    }
    if !emit(row("", Tone::Plain)) {
        return;
    }

    // All four panes are stated at once, whichever one is being read. This is
    // where "there is no ancestor" stays on screen instead of being something
    // the user has to go and look for.
    for pane in View::ALL {
        let state = inspect.panes.pane(pane);
        let text = format!("{:<18} {}", pane.label(), state.describe());
        let tone = match state {
            PaneState::Unreadable { .. } | PaneState::ContentUnavailable { .. } => Tone::Fault,
            _ => Tone::State,
        };
        let line = if pane == inspect.focus {
            selected_row(text, tone)
        } else {
            row(text, tone)
        };
        if !emit(line) {
            return;
        }
    }

    if !emit(row("", Tone::Plain)) {
        return;
    }
    if !emit(row(inspect.focus.label(), Tone::Heading)) {
        return;
    }
    if !visit_content(inspect.panes.pane(inspect.focus), emit) {
        return;
    }
    if !emit(row("", Tone::Plain)) {
        return;
    }
    if !emit(row("Resolutions", Tone::Heading)) {
        return;
    }

    // A withheld control is drawn with NO key beside it — that is what "not
    // offered" looks like on a keyboard — and carries the model's sentence
    // saying why. Pressing the key anyway is refused in `take`, with the same
    // sentence; the two halves agree because both read this one `Result`.
    let surface = &inspect.panes.surface;
    for (key, label, offer) in [
        ('o', "Take ours", &surface.take_ours),
        ('t', "Take theirs", &surface.take_theirs),
        ('d', "Delete the file", &surface.take_deletion),
    ] {
        let line = match offer {
            Ok(()) => row(format!("  {key}   {label}"), Tone::Plain),
            Err(withheld) => row(
                format!("      {label} — {}", withheld.describe()),
                Tone::State,
            ),
        };
        if !emit(line) {
            return;
        }
    }

    // The line-level editor's own offer, from the same flag the server asks
    // before it will execute one.
    let editor_row = if surface.text_resolution_allowed {
        row("  e   Resolve line by line", Tone::Plain)
    } else {
        row(
            "      Resolve line by line — not available for this file",
            Tone::State,
        )
    };
    let _ = emit(editor_row);
}

/// The composed text a submission would carry, or `None` while a block is
/// still unchosen and nothing has been typed.
fn editor_result(editor: &Editor) -> Option<String> {
    if editor.hand_edited || editor.inserting {
        if let Some(buffer) = editor.buffer.as_ref() {
            return Some(buffer.text().to_string());
        }
    }
    markers::compose(&editor.blocks, &editor.choices)
}

fn editor_state_sentence(editor: &Editor) -> String {
    if editor.hand_edited {
        return String::from("edited by hand — block choices no longer change this text");
    }
    let open = markers::unchosen(&editor.blocks, &editor.choices).len();
    if open == 0 {
        String::from("every conflict chosen")
    } else {
        format!(
            "{open} of {} conflict(s) still need a choice",
            editor.choices.len()
        )
    }
}

fn block_row_count(block: &Block) -> usize {
    match block {
        Block::Context { text } => text.lines().count().max(1),
        Block::Conflict { ours, theirs, base } => {
            // heading + the ancestor (its lines, or the one sentence saying
            // there is none) + both sides.
            1 + base.as_ref().map_or(1, |text| text.lines().count().max(1))
                + ours.lines().count().max(1)
                + theirs.lines().count().max(1)
        }
    }
}

/// The absolute index of the first row of the Result section — what the
/// caret's line number is measured from.
fn editor_result_first_row(editor: &Editor) -> usize {
    // path + state sentence + blank + "Blocks" + every block + blank + "Result"
    4 + editor.blocks.iter().map(block_row_count).sum::<usize>() + 2
}

fn editor_row_count(editor: &Editor) -> usize {
    let body = match editor_result(editor) {
        Some(text) => text.lines().count().max(1),
        None => 1,
    };
    editor_result_first_row(editor) + body
}

fn visit_editor(editor: &Editor, emit: &mut impl FnMut(Row) -> bool) {
    for head in [
        row(editor.path.clone(), Tone::Heading),
        row(editor_state_sentence(editor), Tone::State),
        row("", Tone::Plain),
        row("Blocks", Tone::Heading),
    ] {
        if !emit(head) {
            return;
        }
    }

    let total = editor.choices.len();
    let mut nth = 0usize;
    for block in &editor.blocks {
        match block {
            Block::Context { text } => {
                for line in lines_or_one(text) {
                    if !emit(row(format!("  {line}"), Tone::Muted)) {
                        return;
                    }
                }
            }
            Block::Conflict { ours, theirs, base } => {
                let index = nth;
                nth += 1;
                let choice = editor
                    .choices
                    .get(index)
                    .copied()
                    .unwrap_or(Choice::Unchosen);
                let heading = format!("Conflict {} of {total} — {}", index + 1, choice.describe());
                let line = if index == editor.block {
                    selected_row(heading, Tone::Heading)
                } else {
                    row(heading, Tone::Heading)
                };
                if !emit(line) {
                    return;
                }
                // An absent ancestor says so. Git omits it under the default
                // merge style, and an empty ancestor section would claim a
                // common ancestor existed and was blank.
                match base {
                    Some(text) => {
                        for line in lines_or_one(text) {
                            if !emit(row(format!("| {line}"), Tone::Muted)) {
                                return;
                            }
                        }
                    }
                    None => {
                        if !emit(row(
                            "| no recorded ancestor in this marker file",
                            Tone::State,
                        )) {
                            return;
                        }
                    }
                }
                for line in lines_or_one(ours) {
                    if !emit(row(format!("< {line}"), Tone::Ours)) {
                        return;
                    }
                }
                for line in lines_or_one(theirs) {
                    if !emit(row(format!("> {line}"), Tone::Theirs)) {
                        return;
                    }
                }
            }
        }
    }

    if !emit(row("", Tone::Plain)) {
        return;
    }
    if !emit(row("Result", Tone::Heading)) {
        return;
    }
    let caret = editor
        .inserting
        .then(|| editor.buffer.as_ref().map(TextEdit::position))
        .flatten();
    match editor_result(editor) {
        None => {
            let _ = emit(row(editor_state_sentence(editor), Tone::State));
        }
        Some(text) => {
            for (index, line) in lines_or_one(&text).into_iter().enumerate() {
                let mut line = row(line, Tone::Plain);
                if let Some((caret_line, column)) = caret {
                    if caret_line == index {
                        line.caret = Some(column);
                    }
                }
                if !emit(line) {
                    return;
                }
            }
        }
    }
}

/// A text's lines, or one empty line when it has none — so a body always
/// occupies the row `block_row_count` reserved for it.
fn lines_or_one(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        vec![""]
    } else {
        lines
    }
}

struct RowWindow {
    start: usize,
    limit: usize,
    rows: Vec<Row>,
}

impl RowWindow {
    fn new(start: usize, limit: usize) -> RowWindow {
        RowWindow {
            start,
            limit,
            rows: Vec::with_capacity(limit.min(256)),
        }
    }

    /// `false` means the window is full and projection can stop.
    fn push(&mut self, row: Row) -> bool {
        if self.limit == 0 {
            return false;
        }
        if self.start > 0 {
            self.start -= 1;
            return true;
        }
        if self.rows.len() < self.limit {
            self.rows.push(row);
        }
        self.rows.len() < self.limit
    }
}

#[cfg(test)]
mod tests {
    use git_vista_conflicts::core::Withheld;
    use git_vista_core::diff::WorktreeFileContent;
    use git_vista_protocol::conflict::{NotTextResolvable, Stage};
    use git_vista_protocol::status::ConflictKind;
    use git_vista_protocol::GenerationToken;

    use super::*;

    const REPO: &str = "repo-1";
    const PATH: &str = "a.txt";
    const MARKER: &str =
        "before\n<<<<<<< HEAD\nours line\n=======\ntheirs line\n>>>>>>> theirs\nafter\n";

    fn oid(seed: char) -> CommitOid {
        CommitOid::new(std::iter::repeat_n(seed, 40).collect::<String>()).unwrap()
    }

    fn present(seed: char) -> Stage {
        Stage::Present {
            oid: oid(seed),
            binary: false,
            size_bytes: 10,
        }
    }

    /// An ordinary text conflict with **no common ancestor** — the add/add
    /// shape, and the fixture criterion 1 is about.
    fn text_conflict() -> ConflictedFile {
        ConflictedFile {
            path: PATH.to_string(),
            kind: ConflictKind::BothAdded,
            base: Stage::Absent {},
            ours: present('a'),
            theirs: present('b'),
            not_text_resolvable: None,
        }
    }

    fn binary_conflict() -> ConflictedFile {
        ConflictedFile {
            path: PATH.to_string(),
            kind: ConflictKind::BothModified,
            base: present('c'),
            ours: Stage::Present {
                oid: oid('a'),
                binary: true,
                size_bytes: 4096,
            },
            theirs: present('b'),
            not_text_resolvable: Some(NotTextResolvable::Binary {
                ours: true,
                theirs: false,
            }),
        }
    }

    /// They deleted it; our side still has it. `theirs` holds nothing to take.
    fn delete_modify_conflict() -> ConflictedFile {
        ConflictedFile {
            path: PATH.to_string(),
            kind: ConflictKind::DeletedByThem,
            base: present('c'),
            ours: present('a'),
            theirs: Stage::Absent {},
            not_text_resolvable: Some(NotTextResolvable::Deletion {
                ours_deleted: false,
                theirs_deleted: true,
            }),
        }
    }

    fn unreadable_conflict() -> ConflictedFile {
        ConflictedFile {
            path: PATH.to_string(),
            kind: ConflictKind::BothModified,
            base: present('c'),
            ours: Stage::Unreadable {
                reason: "loose object corrupt".to_string(),
            },
            theirs: present('b'),
            not_text_resolvable: None,
        }
    }

    fn source(content: &str) -> ConflictSource {
        ConflictSource {
            path: PATH.to_string(),
            content: content.to_string(),
            truncated: false,
            binary: false,
            source: GenerationToken::new("conflict-v1:9f86d081").unwrap(),
            stages: [None, Some(oid('a')), Some(oid('b'))],
        }
    }

    fn listing(files: Vec<ConflictedFile>) -> ConflictsPane {
        let mut pane = ConflictsPane::default();
        pane.open(REPO.to_string());
        pane.receive_conflicts(REPO, Ok(files));
        pane
    }

    fn inspecting(file: ConflictedFile) -> ConflictsPane {
        let mut pane = listing(vec![file]);
        pane.apply(Act::Open);
        pane
    }

    fn editing(marker: &str) -> ConflictsPane {
        let mut pane = inspecting(text_conflict());
        pane.apply(Act::OpenEditor);
        pane.receive_source(REPO, PATH, Ok(source(marker)));
        pane
    }

    fn rows(pane: &ConflictsPane) -> Vec<Row> {
        pane.window(0, usize::MAX)
    }

    fn texts(pane: &ConflictsPane) -> Vec<String> {
        rows(pane).into_iter().map(|row| row.text).collect()
    }

    fn body(pane: &ConflictsPane) -> String {
        texts(pane).join("\n")
    }

    // ---- the two implementations of the layout, pinned together ---------

    #[test]
    fn row_count_agrees_with_the_rows_actually_emitted_on_every_screen() {
        // `row_count` is arithmetic and `visit_rows` is a walk. Two
        // implementations of one layout, and the scroll clamp trusts the
        // arithmetic one — so a screen whose count is short scrolls to a row
        // that is not there, and one whose count is long refuses to reach the
        // last row of a file.
        //
        // MUTATION A: drop the `+ note` term from `inspect_row_count`.
        // MUTATION B: drop the ancestor's row from `block_row_count`.
        // Either passes every other test in this module.
        let mut cases: Vec<(&str, ConflictsPane)> = vec![
            ("list", listing(vec![text_conflict()])),
            ("list, empty", listing(Vec::new())),
            ("inspect, no note", inspecting(text_conflict())),
            ("inspect, with a note", inspecting(binary_conflict())),
            ("inspect, unreadable", inspecting(unreadable_conflict())),
            ("editor, merge-style", editing(MARKER)),
            (
                "editor, diff3-style",
                editing(
                    "a\n<<<<<<< HEAD\nours\n||||||| base\nancestor\n=======\ntheirs\n>>>>>>> t\nz\n",
                ),
            ),
            ("editor, no conflict at all", editing("plain text\n")),
        ];

        // …and one with content actually loaded into the focused pane, which
        // is the arm where the count stops being a constant.
        let mut loaded = inspecting(text_conflict());
        loaded.receive_stage(
            REPO,
            PATH,
            View::Ours,
            Ok(BlobContent {
                oid: oid('a').as_str().to_string(),
                content: "one\ntwo\nthree\n".to_string(),
                truncated: false,
                binary: false,
            }),
        );
        cases.push(("inspect, ours loaded", loaded));

        let mut chosen = editing(MARKER);
        chosen.apply(Act::Choose(Choice::Ours));
        cases.push(("editor, one block chosen", chosen));

        for (name, pane) in cases {
            assert_eq!(
                pane.row_count(),
                rows(&pane).len(),
                "{name}: row_count() and visit_rows() disagree"
            );
        }
    }

    // ---- criterion 1: a pane that has nothing says so -------------------

    #[test]
    fn a_pane_with_no_ancestor_says_so_and_is_never_a_blank_row() {
        // #462's first criterion, and the one an empty box would silently
        // fail. The words are the model's (`PaneState::describe`), asserted
        // here as a literal rather than by calling `describe()` — a test that
        // calls the function that defines the mapping proves only that the
        // function is itself.
        let mut pane = inspecting(text_conflict());
        pane.apply(Act::FocusPane(View::Base));

        let drawn = rows(&pane);
        let base_rows: Vec<&Row> = drawn
            .iter()
            .filter(|row| row.text.contains("Not present on this side"))
            .collect();
        assert!(
            !base_rows.is_empty(),
            "the absent ancestor never said so:\n{}",
            body(&pane)
        );

        // And the focused body is that sentence, not an empty row. Find the
        // "Base" heading and check what follows it.
        let heading = drawn
            .iter()
            .position(|row| row.text == "Base" && row.tone == Tone::Heading)
            .expect("the focused pane's heading is missing");
        assert_eq!(
            drawn[heading + 1].text,
            "Not present on this side",
            "the focused ancestor pane drew something other than its own sentence"
        );
        assert_ne!(
            drawn[heading + 1].text.trim(),
            "",
            "an empty row claims the ancestor existed and was blank"
        );
    }

    #[test]
    fn all_four_panes_state_themselves_at_once_not_only_the_focused_one() {
        // The reason this overlay is a summary strip plus one body rather than
        // a 2x2 grid: whichever pane is being read, every pane's own sentence
        // stays on screen, so "there is no ancestor" is never something the
        // user has to go looking for.
        let pane = inspecting(text_conflict());
        let drawn = body(&pane);
        for label in ["Base", "Ours", "Theirs", "Result (read-only)"] {
            assert!(
                drawn.contains(label),
                "pane {label} was not stated at all:\n{drawn}"
            );
        }
        assert!(drawn.contains("Not present on this side"));
        assert!(drawn.contains("Loading…"), "an unfetched side must say so");
    }

    #[test]
    fn an_unreadable_stage_is_a_fault_and_never_reads_as_empty() {
        let pane = inspecting(unreadable_conflict());
        let drawn = rows(&pane);
        let row = drawn
            .iter()
            .find(|row| row.text.contains("loose object corrupt"))
            .unwrap_or_else(|| panic!("the unreadable side did not say why:\n{}", body(&pane)));
        assert_eq!(
            row.tone,
            Tone::Fault,
            "an unreadable side must read as a fault, not as an ordinary state"
        );
    }

    #[test]
    fn a_missing_worktree_file_reads_as_absent_and_a_failed_read_as_a_fault() {
        // The result pane's two non-content answers are different facts. In a
        // delete/modify conflict git legitimately leaves nothing on disk, and
        // reporting that as a failed read tells the user something broke when
        // nothing did.
        let mut absent = inspecting(delete_modify_conflict());
        absent.receive_result(REPO, PATH, ResultRead::NoFile);
        absent.apply(Act::FocusPane(View::Result));
        assert!(
            body(&absent).contains("Not present on this side"),
            "a missing worktree file did not read as absent:\n{}",
            body(&absent)
        );

        let mut failed = inspecting(delete_modify_conflict());
        failed.receive_result(REPO, PATH, ResultRead::Failed("permission denied".into()));
        failed.apply(Act::FocusPane(View::Result));
        let drawn = rows(&failed);
        let row = drawn
            .iter()
            .find(|row| row.text.contains("permission denied"))
            .expect("a failed read did not say why");
        assert_eq!(row.tone, Tone::Fault);
    }

    #[test]
    fn an_empty_version_says_it_is_empty_rather_than_drawing_nothing() {
        // `Text { content: "" }` is the ONE state entitled to claim this
        // version was blank (ADR 0063). A claim nobody can see is not made, so
        // it is said in words.
        let mut pane = inspecting(text_conflict());
        pane.receive_stage(
            REPO,
            PATH,
            View::Ours,
            Ok(BlobContent {
                oid: oid('a').as_str().to_string(),
                content: String::new(),
                truncated: false,
                binary: false,
            }),
        );
        pane.apply(Act::FocusPane(View::Ours));
        assert!(
            body(&pane).contains("(this version is empty)"),
            "an empty side drew a blank row:\n{}",
            body(&pane)
        );
    }

    // ---- criterion 3: the refusal is rendered, the action is not offered -

    #[test]
    fn a_withheld_control_is_drawn_with_no_key_and_refuses_when_pressed() {
        // Both halves of "not offered": the row carries no key to press, and
        // pressing it anyway is refused in the model's own words rather than
        // walking the user into a 409. They agree because both read the same
        // `Result` on the surface.
        let mut pane = inspecting(delete_modify_conflict());
        let drawn = body(&pane);

        assert!(
            drawn.contains("  o   Take ours"),
            "the offered control lost its key:\n{drawn}"
        );
        assert!(
            !drawn.contains("  t   Take theirs"),
            "a control for an absent side was offered a key:\n{drawn}"
        );
        assert!(
            drawn.contains(&Withheld::SideAbsent.describe()),
            "the withheld control did not say why:\n{drawn}"
        );

        let requests = pane.apply(Act::Take(Resolution::TakeTheirs));
        assert!(
            requests.is_empty(),
            "pressing a withheld control still asked the server to do it"
        );
        assert_eq!(
            pane.message().map(|(text, _)| text.to_string()),
            Some(Withheld::SideAbsent.describe()),
            "the refusal did not reach the user"
        );

        // The control that IS offered still works, or this test would pass on
        // a pane that refuses everything.
        let requests = pane.apply(Act::Take(Resolution::TakeOurs));
        assert_eq!(
            requests,
            vec![Request::ResolveWholeFile {
                path: PATH.to_string(),
                resolution: Resolution::TakeOurs
            }]
        );
    }

    #[test]
    fn nothing_is_offered_at_all_while_a_side_could_not_be_read() {
        // `all_sides_readable` outranks everything, deletion included: the
        // decision to delete is still made against a file the user cannot
        // fully inspect.
        let mut pane = inspecting(unreadable_conflict());
        let drawn = body(&pane);
        assert!(!drawn.contains("  o   Take ours"), "{drawn}");
        assert!(!drawn.contains("  t   Take theirs"), "{drawn}");
        assert!(!drawn.contains("  d   Delete the file"), "{drawn}");
        for resolution in [
            Resolution::TakeOurs,
            Resolution::TakeTheirs,
            Resolution::TakeDeletion,
        ] {
            assert!(
                pane.apply(Act::Take(resolution)).is_empty(),
                "{resolution:?} was sent for a file with an unreadable side"
            );
        }
    }

    #[test]
    fn the_line_editor_opens_only_on_the_flag_the_server_computed() {
        // Criterion 3's other half. `text_resolution_allowed` traces to
        // `ConflictedFile::text_resolvable` — the identical question the
        // server asks before executing a content resolution. Recomputed here,
        // the two could disagree and the editor would open on a file the
        // executor refuses.
        //
        // MUTATION: replace the flag with `file.not_text_resolvable.is_none()`
        // — a plausible-looking local re-derivation that drops the per-side
        // `is_text()` clause. It agrees on the fixtures below EXCEPT the
        // binary one, which is why that case is here.
        let mut binary = inspecting(binary_conflict());
        assert!(
            binary.apply(Act::OpenEditor).is_empty(),
            "the line editor was offered for a binary conflict"
        );
        assert!(
            body(&binary).contains("not available for this file"),
            "the binary conflict did not say the editor is unavailable:\n{}",
            body(&binary)
        );
        assert!(
            binary
                .message()
                .is_some_and(|(text, error)| error && text.contains("binary")),
            "pressing `e` on a binary conflict said nothing useful: {:?}",
            binary.message()
        );

        let mut text = inspecting(text_conflict());
        assert_eq!(
            text.apply(Act::OpenEditor),
            vec![Request::Source {
                path: PATH.to_string()
            }],
            "the editor refused an ordinary text conflict"
        );
    }

    #[test]
    fn a_binary_side_bars_the_editor_even_when_the_server_sent_no_typed_reason() {
        // The case that separates READING the predicate from recomputing it,
        // and the reason the previous test alone is not enough.
        //
        // `ConflictedFile::text_resolvable` is three clauses: no typed reason,
        // AND both live sides actually text. The obvious local re-derivation
        // keeps only the first — and on every ordinary fixture the two agree,
        // which is exactly how a wrong copy of a rule survives its own tests.
        //
        // This fixture is the documented disagreement: the wire carries no
        // `not_text_resolvable`, but a stage says it is binary, and the
        // protocol's own doc says the per-side flag wins because "rendering
        // real binary bytes as lossy text is worse than withholding a pane".
        //
        // MUTATION: `let allowed = inspect.panes.surface.note.is_none();` or
        // `file.not_text_resolvable.is_none()` in place of the flag. Both pass
        // every other test in this module and fail here.
        let disagreeing = ConflictedFile {
            path: PATH.to_string(),
            kind: ConflictKind::BothModified,
            base: present('c'),
            ours: Stage::Present {
                oid: oid('a'),
                binary: true,
                size_bytes: 4096,
            },
            theirs: present('b'),
            not_text_resolvable: None,
        };
        assert!(
            !disagreeing.text_resolvable(),
            "fixture is wrong: the protocol must already refuse this file"
        );
        assert!(
            disagreeing.not_text_resolvable.is_none(),
            "fixture is wrong: the naive re-derivation must say yes here"
        );

        let mut pane = inspecting(disagreeing);
        assert!(
            pane.apply(Act::OpenEditor).is_empty(),
            "the editor opened on a file the server would refuse to resolve as text"
        );
        assert!(
            body(&pane).contains("not available for this file"),
            "the pane offered a line editor it cannot deliver:\n{}",
            body(&pane)
        );
    }

    #[test]
    fn a_conflicts_shape_is_named_in_the_list_and_on_the_pane() {
        // The note is the model's sentence, and #430's second criterion is
        // that a delete/modify conflict names which side did what. Asserted as
        // the literal words rather than by calling `note_for` again.
        let pane = listing(vec![delete_modify_conflict()]);
        assert!(
            body(&pane).contains("They deleted this file; our side still has it"),
            "the list did not name the conflict's shape:\n{}",
            body(&pane)
        );
        let ordinary = listing(vec![text_conflict()]);
        assert_eq!(
            texts(&ordinary)[1],
            PATH,
            "an ordinary text conflict was given a note it does not need"
        );
    }

    // ---- criterion 4: a hand-edit is never silently discarded ------------

    #[test]
    fn a_hand_edit_makes_block_choices_inert_and_keeps_every_typed_character() {
        // THE test of this module. Criterion 4, and the worst failure
        // available in an editor: re-composing from the buttons after somebody
        // has typed throws their work away without a word.
        //
        // MUTATION A: delete the `if editor.hand_edited { return }` guard in
        // `choose`. MUTATION B: make `apply_content` prefer the composition
        // over the buffer. Each one silently discards the typing, and each
        // fails a different assertion below.
        let mut pane = editing(MARKER);
        pane.apply(Act::Choose(Choice::Ours));
        pane.apply(Act::BeginEdit);
        for ch in "ZZZ".chars() {
            pane.apply(Act::Type(ch));
        }
        pane.apply(Act::EndEdit);
        let typed = pane
            .editor
            .as_ref()
            .unwrap()
            .buffer
            .as_ref()
            .unwrap()
            .text()
            .to_string();
        assert!(typed.contains("ZZZ"), "the typing never landed: {typed:?}");

        // Now press the block buttons. Nothing about the text may move.
        pane.apply(Act::Choose(Choice::Theirs));
        pane.apply(Act::Choose(Choice::Both));
        assert_eq!(
            pane.editor
                .as_ref()
                .unwrap()
                .buffer
                .as_ref()
                .unwrap()
                .text(),
            typed,
            "a block choice rewrote text the user had typed"
        );
        assert_eq!(
            pane.editor.as_ref().unwrap().choices[0],
            Choice::Ours,
            "a block choice was recorded while the text was hand-edited"
        );
        assert!(
            pane.message()
                .is_some_and(|(text, _)| text.contains("edited by hand")),
            "the user was not told why the buttons stopped working"
        );

        // …and the submission carries the typed text, not the composition.
        let requests = pane.apply(Act::Apply);
        match requests.as_slice() {
            [Request::ResolveContent { content, .. }] => {
                assert_eq!(content, &typed, "the submission discarded the hand-edit");
            }
            other => panic!("expected one content resolution, got {other:?}"),
        }
    }

    #[test]
    fn reopening_the_buffer_before_any_edit_reseeds_from_the_current_choices() {
        // Until a real edit exists the buffer is only a view of the
        // composition, so opening it again after changing a block must show
        // that change. After one, it is the user's text and must not be
        // reseeded — which is the previous test.
        let mut pane = editing(MARKER);
        pane.apply(Act::Choose(Choice::Ours));
        pane.apply(Act::BeginEdit);
        assert_eq!(
            pane.editor
                .as_ref()
                .unwrap()
                .buffer
                .as_ref()
                .unwrap()
                .text(),
            "before\nours line\nafter\n"
        );
        pane.apply(Act::EndEdit);
        pane.apply(Act::Choose(Choice::Theirs));
        pane.apply(Act::BeginEdit);
        assert_eq!(
            pane.editor
                .as_ref()
                .unwrap()
                .buffer
                .as_ref()
                .unwrap()
                .text(),
            "before\ntheirs line\nafter\n",
            "the buffer kept a stale composition after the choice changed"
        );
    }

    #[test]
    fn entering_and_leaving_the_buffer_without_typing_does_not_freeze_the_buttons() {
        // The flag is "a keystroke changed it", not "the buffer exists".
        // Merely looking at the composed text must not make the block choices
        // inert — that would be criterion 4 firing on someone who did nothing.
        let mut pane = editing(MARKER);
        pane.apply(Act::BeginEdit);
        pane.apply(Act::EndEdit);
        pane.apply(Act::Choose(Choice::Theirs));
        assert_eq!(pane.editor.as_ref().unwrap().choices[0], Choice::Theirs);
        assert!(!pane.editor.as_ref().unwrap().hand_edited);
    }

    #[test]
    fn an_unchosen_block_asks_for_nothing_and_says_how_many_are_open() {
        // `compose` returns None while any block is unchosen, and this refuses
        // rather than submitting a guess. A resolution invented on the user's
        // behalf is the failure `markers::compose`'s own return type exists to
        // prevent, and it must not be reintroduced one layer out.
        let mut pane = editing(MARKER);
        let requests = pane.apply(Act::Apply);
        assert!(
            requests.is_empty(),
            "an unresolved conflict was submitted anyway"
        );
        assert!(
            pane.message()
                .is_some_and(|(text, error)| error && text.contains("1 conflict(s) still need")),
            "the refusal did not say what is missing: {:?}",
            pane.message()
        );
    }

    #[test]
    fn a_content_resolution_echoes_the_served_stages_and_token_unchanged() {
        // ADR 0069 gates 3 and 4 compare these against a fresh scan and a
        // re-minted token inside the executor's lock. A client that computed
        // its own could only ever agree with itself — the gates would pass by
        // construction and prove nothing.
        //
        // MUTATION: rebuild `expected_stages` from anything local. The gate
        // still passes server-side on an unchanged repository, so only an
        // assertion on identity catches it.
        let served = source(MARKER);
        let mut pane = editing(MARKER);
        pane.apply(Act::Choose(Choice::Both));
        match pane.apply(Act::Apply).as_slice() {
            [Request::ResolveContent {
                path,
                expected_stages,
                expected_source,
                content,
            }] => {
                assert_eq!(path, PATH);
                assert_eq!(expected_stages, &served.stages);
                assert_eq!(expected_source, &served.source);
                assert_eq!(
                    content, "before\nours line\ntheirs line\nafter\n",
                    "the composed content is not what the shared composer produces"
                );
            }
            other => panic!("expected one content resolution, got {other:?}"),
        }
    }

    #[test]
    fn a_marker_file_with_no_recorded_ancestor_says_so_inside_the_editor_too() {
        // ADR 0063's distinction, one layer in: git omits the ancestor under
        // the default merge style, and an empty ancestor section would claim a
        // common ancestor existed and was blank.
        let plain = editing(MARKER);
        assert!(
            body(&plain).contains("no recorded ancestor in this marker file"),
            "a merge-style block drew a blank ancestor:\n{}",
            body(&plain)
        );

        let diff3 = editing(
            "<<<<<<< HEAD\nours\n||||||| base\nancestor text\n=======\ntheirs\n>>>>>>> t\n",
        );
        let drawn = body(&diff3);
        assert!(
            drawn.contains("| ancestor text"),
            "a diff3 block lost its ancestor:\n{drawn}"
        );
        assert!(
            !drawn.contains("no recorded ancestor"),
            "a diff3 block claimed it had no ancestor:\n{drawn}"
        );
    }

    // ---- reads, request keys, and answers for the wrong thing ------------

    #[test]
    fn opening_a_conflict_asks_only_for_the_panes_that_await_content() {
        // The model resolves an absent or binary side from metadata alone, so
        // a conflict with a 200 MB binary side costs one listing, not a
        // download. The same rule the browser client's assembler follows.
        let mut pane = listing(vec![binary_conflict()]);
        let requests = pane.apply(Act::Open);
        assert!(requests.contains(&Request::Result {
            path: PATH.to_string()
        }));
        let stages: Vec<View> = requests
            .iter()
            .filter_map(|request| match request {
                Request::Stage { pane, .. } => Some(*pane),
                _ => None,
            })
            .collect();
        assert_eq!(
            stages,
            vec![View::Base, View::Theirs],
            "the binary side was fetched, or a readable one was skipped"
        );

        // An absent side is not fetched either.
        let mut absent = listing(vec![text_conflict()]);
        let stages: Vec<View> = absent
            .apply(Act::Open)
            .iter()
            .filter_map(|request| match request {
                Request::Stage { pane, .. } => Some(*pane),
                _ => None,
            })
            .collect();
        assert_eq!(stages, vec![View::Ours, View::Theirs]);
    }

    #[test]
    fn an_answer_for_another_repository_or_another_path_is_dropped() {
        // The same request-key discipline `DetailPane` uses. A late answer
        // must not repaint a view the user has already left.
        let mut pane = inspecting(text_conflict());
        pane.receive_stage(
            "another-repo",
            PATH,
            View::Ours,
            Ok(BlobContent {
                oid: oid('a').as_str().to_string(),
                content: "wrong repository".to_string(),
                truncated: false,
                binary: false,
            }),
        );
        pane.receive_result(
            REPO,
            "some/other/path",
            ResultRead::Wrote(WorktreeFileContent {
                path: "some/other/path".to_string(),
                content: "wrong path".to_string(),
                truncated: false,
                binary: false,
            }),
        );
        let drawn = body(&pane);
        assert!(!drawn.contains("wrong repository"), "{drawn}");
        assert!(!drawn.contains("wrong path"), "{drawn}");
    }

    #[test]
    fn a_stale_blob_answer_cannot_turn_an_absent_pane_into_empty_text() {
        // Delegated to `PaneState::with_content`, which returns any pane that
        // is not `AwaitingContent` unchanged. Pinned here because this overlay
        // is the thing that would otherwise let it through the back door.
        let mut pane = inspecting(text_conflict());
        pane.receive_stage(
            REPO,
            PATH,
            View::Base,
            Ok(BlobContent {
                oid: oid('a').as_str().to_string(),
                content: String::new(),
                truncated: false,
                binary: false,
            }),
        );
        pane.apply(Act::FocusPane(View::Base));
        assert!(
            body(&pane).contains("Not present on this side"),
            "a late blob answer overwrote an absent ancestor:\n{}",
            body(&pane)
        );
    }

    #[test]
    fn a_successful_resolution_drops_the_list_rather_than_editing_it() {
        // `conflicts::scan` is stateless and re-reads git on every call (ADR
        // 0063). A client that removed the row itself would be asserting an
        // outcome it never observed — resolving one path can change others.
        let mut pane = inspecting(text_conflict());
        let refetch = pane.receive_resolved(REPO, PATH, Ok(()));
        assert!(refetch, "a successful resolution did not ask for a refetch");
        assert_eq!(pane.screen(), Screen::List);
        assert!(pane.files.is_none(), "the stale list survived a resolution");
        assert!(body(&pane).contains("loading"), "{}", body(&pane));
    }

    #[test]
    fn a_refused_resolution_keeps_the_view_and_the_servers_own_sentence() {
        // The four content refusals name four different things that moved, and
        // collapsing them into "it failed" throws away the only part that says
        // what to do next.
        let mut pane = editing(MARKER);
        pane.apply(Act::Choose(Choice::Ours));
        pane.apply(Act::Apply);
        let refetch = pane.receive_resolved(
            REPO,
            PATH,
            Err(
                "a.txt changed since you opened it — the version you resolved \
                 against is no longer current. Reopen it and try again."
                    .to_string(),
            ),
        );
        assert!(!refetch, "a refusal asked for a refetch");
        assert_eq!(pane.screen(), Screen::Editor, "a refusal closed the editor");
        assert!(pane
            .message()
            .is_some_and(|(text, error)| error && text.contains("no longer current")));
        // …and the controls are live again, so a refusal is recoverable.
        assert!(
            !pane.apply(Act::Apply).is_empty(),
            "the pane stayed busy after a refusal"
        );
    }

    #[test]
    fn a_second_press_while_a_write_is_out_asks_for_nothing() {
        let mut pane = inspecting(text_conflict());
        assert_eq!(pane.apply(Act::Take(Resolution::TakeOurs)).len(), 1);
        assert!(
            pane.apply(Act::Take(Resolution::TakeOurs)).is_empty(),
            "a held key sent the same resolution twice"
        );
    }

    // ---- the keymap's modes ---------------------------------------------

    #[test]
    fn the_key_mode_follows_the_screen_and_insert_is_its_own() {
        let mut pane = ConflictsPane::default();
        assert_eq!(pane.key_mode(), KeyMode::List);
        pane.open(REPO.to_string());
        pane.receive_conflicts(REPO, Ok(vec![text_conflict()]));
        pane.apply(Act::Open);
        assert_eq!(pane.key_mode(), KeyMode::Inspect);
        pane.apply(Act::OpenEditor);
        pane.receive_source(REPO, PATH, Ok(source(MARKER)));
        assert_eq!(pane.key_mode(), KeyMode::Editor);
        pane.apply(Act::BeginEdit);
        assert_eq!(pane.key_mode(), KeyMode::Insert);
        pane.apply(Act::EndEdit);
        assert_eq!(pane.key_mode(), KeyMode::Editor);
    }

    #[test]
    fn back_walks_out_one_screen_at_a_time_and_then_closes() {
        let mut pane = editing(MARKER);
        pane.apply(Act::Back);
        assert_eq!(pane.screen(), Screen::Inspect);
        pane.apply(Act::Back);
        assert_eq!(pane.screen(), Screen::List);
        assert!(pane.is_open());
        pane.apply(Act::Back);
        assert!(!pane.is_open());
    }

    // ---- the text buffer -------------------------------------------------

    #[test]
    fn the_buffer_inserts_deletes_and_moves_across_multibyte_characters() {
        // The caret is a byte offset, so every one of these is a chance to
        // land inside a character and panic on the next slice.
        let mut buffer = TextEdit::new("héllo".to_string());
        assert_eq!(buffer.position(), (0, 5));
        buffer.left();
        buffer.left();
        buffer.left();
        buffer.left();
        assert_eq!(buffer.position(), (0, 1));
        buffer.insert('ü');
        assert_eq!(buffer.text(), "hüéllo");
        buffer.backspace();
        assert_eq!(buffer.text(), "héllo");
        buffer.right();
        buffer.insert('\n');
        assert_eq!(buffer.text(), "hé\nllo");
        assert_eq!(buffer.position(), (1, 0));
    }

    #[test]
    fn the_buffer_moves_between_lines_and_stops_at_both_ends() {
        let mut buffer = TextEdit::new("one\ntwo\nthree".to_string());
        assert_eq!(buffer.position(), (2, 5));
        buffer.vertical(-1);
        assert_eq!(buffer.position(), (1, 3), "column clamps to a shorter line");
        buffer.vertical(-1);
        assert_eq!(buffer.position(), (0, 3));
        buffer.vertical(-1);
        assert_eq!(buffer.position(), (0, 3), "moved above the first line");
        buffer.vertical(1);
        buffer.vertical(1);
        buffer.vertical(1);
        assert_eq!(buffer.position(), (2, 3), "moved below the last line");
    }

    #[test]
    fn backspace_at_the_very_start_does_nothing_rather_than_wrapping() {
        let mut buffer = TextEdit::new(String::new());
        buffer.backspace();
        assert_eq!(buffer.text(), "");
        buffer.right();
        assert_eq!(buffer.position(), (0, 0));
    }

    #[test]
    fn the_viewport_follows_the_caret_out_of_the_visible_window() {
        // An invisible caret in a buffer that accepts every keystroke is a way
        // to type into a line you are not looking at.
        let mut pane = editing(MARKER);
        pane.apply(Act::Choose(Choice::Both));
        pane.apply(Act::BeginEdit);
        let caret = pane.caret_row().expect("insert mode has a caret row");
        assert!(
            pane.view_offset(3) <= caret && caret < pane.view_offset(3) + 3,
            "the caret at row {caret} fell outside the window at offset {}",
            pane.view_offset(3)
        );
        // With no insert mode there is no caret to follow, and the scroll is
        // the user's own, clamped.
        pane.apply(Act::EndEdit);
        assert!(pane.caret_row().is_none());
        assert_eq!(pane.view_offset(usize::MAX), 0);
    }

    #[test]
    fn the_viewport_follows_the_block_cursor_out_of_the_visible_window() {
        // The third face of the same hazard (#634), and the last one the page
        // keys opened. Outside insert mode `move_cursor` and `jump` move
        // `editor.block` and nothing else, `caret_row` answers `None`, and
        // `scroll` is never touched on this screen — so `End` and `PageDown`
        // looked inert, and the choice keys then acted on a conflict that was
        // nowhere on screen.
        //
        // Asserting that `editor.block` reached the last index would have
        // passed throughout the defect's life. What has to hold is that the
        // row the block cursor is on is among the rows actually DRAWN.
        const HEIGHT: usize = 6;
        const BLOCKS: usize = 12;

        let mut marker = String::new();
        for n in 0..BLOCKS {
            marker.push_str(&format!("context {n}\n"));
            marker.push_str(&format!(
                "<<<<<<< HEAD\nours {n}\n=======\ntheirs {n}\n>>>>>>> theirs\n"
            ));
        }
        let mut pane = editing(&marker);
        pane.observe(HEIGHT);

        assert!(
            pane.row_count() > HEIGHT,
            "the fixture must be TALLER than the viewport or this test cannot \
             fail: {} rows in a {HEIGHT}-row window",
            pane.row_count()
        );
        assert_eq!(
            rows(&pane).iter().filter(|row| row.selected).count(),
            1,
            "outside insert mode exactly one row is selected — the block \
             heading. More than one and the assertion below is measuring \
             something else"
        );

        for (act, expected) in [
            (Act::Bottom, format!("Conflict {BLOCKS} of {BLOCKS}")),
            (
                Act::PageUp,
                format!("Conflict {} of {BLOCKS}", BLOCKS - HEIGHT),
            ),
            (Act::Top, format!("Conflict 1 of {BLOCKS}")),
            (
                Act::PageDown,
                format!("Conflict {} of {BLOCKS}", HEIGHT + 1),
            ),
        ] {
            pane.apply(act);
            let offset = pane.view_offset(HEIGHT);
            let drawn = pane.window(offset, HEIGHT);
            let selected: Vec<&Row> = drawn.iter().filter(|row| row.selected).collect();
            assert_eq!(
                selected.len(),
                1,
                "after {act:?} the selected block is not among the {} rows \
                 drawn at offset {offset}: {:?}",
                drawn.len(),
                drawn.iter().map(|row| &row.text).collect::<Vec<_>>()
            );
            assert!(
                selected[0].text.contains(&expected),
                "after {act:?} the drawn selection is {:?}, not {expected}",
                selected[0].text
            );
        }
    }

    #[test]
    fn the_viewport_follows_the_file_cursor_out_of_the_visible_window() {
        // The list's own version of the hazard above, and the one the paging
        // keys created: `End` moves `cursor` and nothing else, so in a list
        // longer than the overlay the highlight left the drawn window
        // entirely — and `Enter` then opened a path that was nowhere on
        // screen. Asserting the cursor reached the last file would have
        // passed throughout; what has to hold is that the row the cursor is
        // on is among the rows actually drawn.
        const HEIGHT: usize = 5;
        let files: Vec<ConflictedFile> = (0..20)
            .map(|n| ConflictedFile {
                path: format!("file-{n:02}.txt"),
                ..text_conflict()
            })
            .collect();
        let mut pane = listing(files);
        pane.observe(HEIGHT);

        for (act, expected) in [
            (Act::Bottom, "file-19.txt"),
            (Act::PageUp, "file-14.txt"),
            (Act::Top, "file-00.txt"),
            (Act::PageDown, "file-05.txt"),
        ] {
            pane.apply(act);
            let offset = pane.view_offset(HEIGHT);
            let drawn = pane.window(offset, HEIGHT);
            let selected: Vec<&Row> = drawn.iter().filter(|row| row.selected).collect();
            assert_eq!(
                selected.len(),
                1,
                "after {act:?} the selected row is not among the {} rows drawn at offset {offset}: {:?}",
                drawn.len(),
                drawn.iter().map(|row| &row.text).collect::<Vec<_>>()
            );
            assert!(
                selected[0].text.contains(expected),
                "after {act:?} the drawn selection is {:?}, not {expected}",
                selected[0].text
            );
        }
    }

    #[test]
    fn a_visible_file_cursor_does_not_move_the_list_window() {
        // The other half of the clamp: following the cursor must not become
        // recentring on every keystroke. A cursor already inside the window
        // leaves the offset exactly where the user put it.
        const HEIGHT: usize = 5;
        let files: Vec<ConflictedFile> = (0..20)
            .map(|n| ConflictedFile {
                path: format!("file-{n:02}.txt"),
                ..text_conflict()
            })
            .collect();
        let mut pane = listing(files);
        pane.observe(HEIGHT);
        assert_eq!(pane.view_offset(HEIGHT), 0, "the list opens at the top");
        pane.apply(Act::Down);
        pane.apply(Act::Down);
        assert_eq!(
            pane.view_offset(HEIGHT),
            0,
            "a cursor still on screen scrolled the window anyway"
        );

        // An empty list has no cursor to follow and must not scroll off its
        // own explanatory row.
        let mut empty = listing(Vec::new());
        empty.observe(HEIGHT);
        empty.apply(Act::Bottom);
        assert_eq!(empty.view_offset(HEIGHT), 0);
    }
}
