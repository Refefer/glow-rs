//! Less-style full-screen viewer for pre-rendered text.
//!
//! Assumes the caller can re-render the document at any column width: the
//! pager owns the terminal, tracks the scroll position, and asks for a fresh
//! render whenever the width it follows changes. Everything that decides
//! *where* the viewport goes is a pure value transform on [`Viewport`], so the
//! event loop is only glue between crossterm events and those transforms.
//!
//! Links are clickable: after every frame the link cells on screen are
//! re-printed inside OSC 8 escapes (see [`crate::hyperlinks`]). Mouse capture
//! stays off, so clicks and text selection belong to the terminal, and the
//! wheel arrives as the arrow keys the terminal translates it into.
//!
//! Gotcha: a fixed `-w` wider than the terminal is honored, not clamped, so
//! the right edge is clipped exactly like `less -S`. Auto width re-wraps on
//! every resize and keeps the reader at the same proportional position rather
//! than the same line index, since line indices shift when text reflows.

use std::io;
use std::num::NonZeroUsize;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::document::Document;
use crate::hyperlinks::{Paint, Painter, PlacedLink};

/// Where the render width comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Always this many columns, regardless of the terminal.
    Fixed(NonZeroUsize),
    /// The terminal's current width, re-rendered on resize.
    Auto,
}

impl Width {
    /// The column count to render at for a terminal `terminal_cols` wide.
    pub fn columns(self, terminal_cols: u16) -> usize {
        match self {
            Width::Fixed(cols) => cols.get(),
            Width::Auto => usize::from(terminal_cols).max(1),
        }
    }
}

/// How far one scroll command moves, in terms of the visible height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Line,
    HalfPage,
    Page,
}

/// A change of reading position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Down(Step),
    Up(Step),
    Top,
    Bottom,
}

/// Everything a keypress can ask the pager to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Move(Motion),
    Quit,
}

/// Maps a key to a command using less/vi conventions; unknown keys are ignored.
pub fn command_for(key: KeyEvent) -> Option<Command> {
    use KeyCode::{Char, Down, End, Enter, Esc, Home, PageDown, PageUp, Up};

    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    let command = match (key.code, ctrl) {
        (Char('q') | Char('Q') | Esc, false) | (Char('c'), true) => Command::Quit,

        (Char('j') | Down | Enter, false) | (Char('e') | Char('n') | Char('j'), true) => {
            Command::Move(Motion::Down(Step::Line))
        }
        (Char('k') | Up, false) | (Char('y') | Char('p') | Char('k'), true) => {
            Command::Move(Motion::Up(Step::Line))
        }

        (Char('J') | Char('d'), false) | (Char('d'), true) => {
            Command::Move(Motion::Down(Step::HalfPage))
        }
        (Char('K') | Char('u'), false) | (Char('u'), true) => {
            Command::Move(Motion::Up(Step::HalfPage))
        }

        (Char('f') | Char(' ') | PageDown, false) | (Char('f') | Char('v'), true) => {
            Command::Move(Motion::Down(Step::Page))
        }
        (Char('b') | PageUp, false) | (Char('b'), true) => Command::Move(Motion::Up(Step::Page)),

        (Char('g') | Char('<') | Home, false) => Command::Move(Motion::Top),
        (Char('G') | Char('>') | End, false) => Command::Move(Motion::Bottom),

        _ => return None,
    };
    Some(command)
}

/// The visible window over a column of rendered lines.
///
/// Every method returns a new value with the offset clamped into
/// `0..=max_offset`, so a `Viewport` can never point past the content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    offset: usize,
    content_lines: usize,
    height: usize,
}

impl Viewport {
    /// A viewport parked at the top of `content_lines` lines, `height` tall.
    pub fn new(content_lines: usize, height: usize) -> Self {
        Self {
            offset: 0,
            content_lines,
            height,
        }
    }

    /// Index of the first visible line.
    #[cfg(test)]
    pub fn offset(self) -> usize {
        self.offset
    }

    /// The largest offset that still fills the viewport (or 0 when the
    /// content fits).
    pub fn max_offset(self) -> usize {
        self.content_lines.saturating_sub(self.height)
    }

    /// Whether the last content line is on screen.
    pub fn at_bottom(self) -> bool {
        self.offset >= self.max_offset()
    }

    /// The half-open range of line indices currently visible.
    pub fn visible_range(self) -> std::ops::Range<usize> {
        let end = self
            .offset
            .saturating_add(self.height)
            .min(self.content_lines);
        self.offset..end
    }

    /// Same content and height, parked at `offset` (clamped).
    pub fn with_offset(self, offset: usize) -> Self {
        Self {
            offset: offset.min(self.max_offset()),
            ..self
        }
    }

    /// Same content and offset, with a new visible height (offset re-clamped).
    pub fn with_height(self, height: usize) -> Self {
        Self { height, ..self }.with_offset(self.offset)
    }

    /// Swaps in a re-rendered document of `content_lines` lines, keeping the
    /// reader at the same proportional position through the text.
    ///
    /// A reader parked at the very bottom stays at the bottom, so a resize
    /// while reading the end of a file does not strand them mid-document.
    pub fn with_content(self, content_lines: usize) -> Self {
        let next = Self {
            content_lines,
            ..self
        };
        let offset = if self.content_lines == 0 {
            0
        } else if self.at_bottom() && self.offset > 0 {
            next.max_offset()
        } else {
            // u128 keeps the product from overflowing on absurdly long files.
            let scaled = u128::from(self.offset as u64) * u128::from(content_lines as u64)
                / u128::from(self.content_lines as u64);
            usize::try_from(scaled).unwrap_or(usize::MAX)
        };
        next.with_offset(offset)
    }

    /// How many lines `step` moves at the current height; never zero, so a
    /// keypress always makes progress even in a one-row terminal.
    fn lines_for(self, step: Step) -> usize {
        match step {
            Step::Line => 1,
            Step::HalfPage => (self.height / 2).max(1),
            Step::Page => self.height.max(1),
        }
    }

    /// Applies one motion.
    pub fn moved(self, motion: Motion) -> Self {
        match motion {
            Motion::Down(step) => {
                self.with_offset(self.offset.saturating_add(self.lines_for(step)))
            }
            Motion::Up(step) => self.with_offset(self.offset.saturating_sub(self.lines_for(step))),
            Motion::Top => self.with_offset(0),
            Motion::Bottom => self.with_offset(self.max_offset()),
        }
    }
}

/// Runs the interactive pager until the reader quits.
///
/// `render` is called with a column count and must return the document
/// wrapped to that width; it runs once up front and again after any resize
/// that changes the render width.
pub fn run(title: &str, width: Width, render: impl Fn(usize) -> Document) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, title, width, render);
    ratatui::restore();
    result
}

/// The rendered document plus the geometry it was rendered for.
struct Rendered {
    doc: Document,
    columns: usize,
}

/// Whether the event loop keeps going after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// Everything the event loop mutates between frames.
struct Session<F> {
    width: Width,
    render: F,
    rendered: Rendered,
    viewport: Viewport,
    /// Like less, the file name is only shown until the first keypress.
    first_screen: bool,
}

impl<F: Fn(usize) -> Document> Session<F> {
    fn new(width: Width, render: F, terminal_cols: u16, terminal_rows: u16) -> Self {
        let columns = width.columns(terminal_cols);
        let rendered = Rendered {
            doc: render(columns),
            columns,
        };
        let viewport = Viewport::new(rendered.doc.lines().len(), body_height(terminal_rows));
        Self {
            width,
            render,
            rendered,
            viewport,
            first_screen: true,
        }
    }

    /// Applies one terminal event without drawing.
    fn apply(&mut self, event: Event) -> Flow {
        match event {
            Event::Key(key) => {
                self.first_screen = false;
                match command_for(key) {
                    Some(Command::Quit) => return Flow::Quit,
                    Some(Command::Move(motion)) => self.viewport = self.viewport.moved(motion),
                    None => {}
                }
            }
            Event::Resize(cols, rows) => {
                let columns = self.width.columns(cols);
                if columns != self.rendered.columns {
                    self.rendered = Rendered {
                        doc: (self.render)(columns),
                        columns,
                    };
                    self.viewport = self.viewport.with_content(self.rendered.doc.lines().len());
                }
                self.viewport = self.viewport.with_height(body_height(rows));
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Blocks for one event, then applies it and every event already queued
    /// behind it.
    ///
    /// A mouse-wheel notch reaches us as a burst of arrow keys (the terminal
    /// translates the wheel because mouse capture is off), and a resize drag
    /// as a stream of resizes. Drawing once per event made the screen fall
    /// further behind the input the longer the burst ran; draining the queue
    /// first means each frame shows where the reader actually is.
    fn apply_pending(&mut self) -> io::Result<Flow> {
        let mut event = event::read()?;
        loop {
            if self.apply(event) == Flow::Quit {
                return Ok(Flow::Quit);
            }
            if !event::poll(Duration::ZERO)? {
                return Ok(Flow::Continue);
            }
            event = event::read()?;
        }
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    title: &str,
    width: Width,
    render: impl Fn(usize) -> Document,
) -> io::Result<()> {
    let size = terminal.size()?;
    let mut session = Session::new(width, render, size.width, size.height);
    let mut painter = Painter::default();

    loop {
        let mut paint = Paint::default();
        terminal.draw(|frame| {
            let prompt =
                Prompt::for_screen(title, session.first_screen, session.viewport.at_bottom());
            let links = draw(frame, &session.rendered.doc, session.viewport, prompt);
            paint = painter.plan(frame.buffer_mut(), &links);
        })?;
        paint.apply(terminal.backend_mut())?;

        if session.apply_pending()? == Flow::Quit {
            return Ok(());
        }
    }
}

/// Rows left for content once the prompt line takes the bottom row.
fn body_height(terminal_rows: u16) -> usize {
    usize::from(terminal_rows.saturating_sub(1))
}

/// What the bottom row says, following less's default prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A bare `:`, printed plainly: mid-file, after the first screen.
    Colon,
    /// A message printed in reverse video: the file name on the first
    /// screen and/or `(END)` once the last line is visible.
    Standout(String),
}

impl Prompt {
    /// The prompt for a screen: `title`, then `(END)`, whichever apply.
    pub fn for_screen(title: &str, first_screen: bool, at_bottom: bool) -> Self {
        let parts: Vec<&str> = [(first_screen, title), (at_bottom, "(END)")]
            .into_iter()
            .filter_map(|(show, part)| show.then_some(part))
            .collect();
        if parts.is_empty() {
            Prompt::Colon
        } else {
            Prompt::Standout(parts.join(" "))
        }
    }

    fn into_line(self) -> Line<'static> {
        match self {
            Prompt::Colon => Line::from(":"),
            Prompt::Standout(text) => {
                Line::styled(text, Style::default().add_modifier(Modifier::REVERSED))
            }
        }
    }
}

/// Draws the visible window and prompt, returning where each link landed so
/// its cells can be made clickable.
fn draw(
    frame: &mut ratatui::Frame,
    doc: &Document,
    viewport: Viewport,
    prompt: Prompt,
) -> Vec<PlacedLink> {
    let [body, prompt_row] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    // Each visible line is written straight into the frame by reference;
    // cloning a screenful of styled spans per frame is wasted work when the
    // frame is redrawn for every scroll step.
    let visible = &doc.lines()[viewport.visible_range()];
    let rows = body.rows().take(usize::from(body.height));
    let mut links = Vec::new();
    for (row, line) in rows.zip(visible) {
        frame.render_widget(line.line(), row);
        links.extend(line.links().iter().filter_map(|link| {
            let clip = |column: usize| u16::try_from(column).unwrap_or(u16::MAX).min(body.width);
            let x = clip(link.columns.start)..clip(link.columns.end);
            (!x.is_empty()).then(|| PlacedLink {
                y: row.y,
                x,
                target: link.target.clone(),
            })
        }));
    }
    // Rows past the end of the file are marked with `~`, as in less.
    let filler = Line::from("~");
    for row in body.rows().skip(visible.len()) {
        frame.render_widget(&filler, row);
    }

    frame.render_widget(Paragraph::new(prompt.into_line()), prompt_row);
    links
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn less_style_keys_map_to_motions() {
        let down = Some(Command::Move(Motion::Down(Step::Line)));
        assert_eq!(command_for(press(KeyCode::Char('j'))), down);
        assert_eq!(command_for(press(KeyCode::Down)), down);
        assert_eq!(command_for(press(KeyCode::Enter)), down);
        assert_eq!(command_for(ctrl('e')), down);

        let up = Some(Command::Move(Motion::Up(Step::Line)));
        assert_eq!(command_for(press(KeyCode::Char('k'))), up);
        assert_eq!(command_for(press(KeyCode::Up)), up);

        let half_down = Some(Command::Move(Motion::Down(Step::HalfPage)));
        assert_eq!(command_for(press(KeyCode::Char('J'))), half_down);
        assert_eq!(command_for(press(KeyCode::Char('d'))), half_down);
        assert_eq!(command_for(ctrl('d')), half_down);
        let half_up = Some(Command::Move(Motion::Up(Step::HalfPage)));
        assert_eq!(command_for(press(KeyCode::Char('K'))), half_up);
        assert_eq!(command_for(ctrl('u')), half_up);

        let page_down = Some(Command::Move(Motion::Down(Step::Page)));
        assert_eq!(command_for(press(KeyCode::Char(' '))), page_down);
        assert_eq!(command_for(press(KeyCode::PageDown)), page_down);
        assert_eq!(command_for(ctrl('f')), page_down);
        let page_up = Some(Command::Move(Motion::Up(Step::Page)));
        assert_eq!(command_for(press(KeyCode::Char('b'))), page_up);
        assert_eq!(command_for(press(KeyCode::PageUp)), page_up);

        assert_eq!(
            command_for(press(KeyCode::Char('g'))),
            Some(Command::Move(Motion::Top))
        );
        assert_eq!(
            command_for(press(KeyCode::End)),
            Some(Command::Move(Motion::Bottom))
        );
    }

    #[test]
    fn quit_keys() {
        assert_eq!(command_for(press(KeyCode::Char('q'))), Some(Command::Quit));
        assert_eq!(command_for(press(KeyCode::Esc)), Some(Command::Quit));
        assert_eq!(command_for(ctrl('c')), Some(Command::Quit));
    }

    #[test]
    fn unknown_keys_and_releases_are_ignored() {
        assert_eq!(command_for(press(KeyCode::Char('x'))), None);
        let mut release = press(KeyCode::Char('j'));
        release.kind = KeyEventKind::Release;
        assert_eq!(command_for(release), None);
    }

    #[test]
    fn motions_clamp_to_the_content() {
        let view = Viewport::new(100, 10);
        assert_eq!(view.moved(Motion::Up(Step::Page)).offset(), 0);
        assert_eq!(view.moved(Motion::Down(Step::Line)).offset(), 1);
        assert_eq!(view.moved(Motion::Down(Step::HalfPage)).offset(), 5);
        assert_eq!(view.moved(Motion::Down(Step::Page)).offset(), 10);
        assert_eq!(view.moved(Motion::Bottom).offset(), 90);
        assert_eq!(
            view.moved(Motion::Bottom)
                .moved(Motion::Down(Step::Page))
                .offset(),
            90
        );
        assert!(view.moved(Motion::Bottom).at_bottom());
        assert_eq!(view.moved(Motion::Bottom).moved(Motion::Top).offset(), 0);
    }

    #[test]
    fn content_that_fits_never_scrolls() {
        let view = Viewport::new(5, 10);
        assert_eq!(view.max_offset(), 0);
        assert_eq!(view.moved(Motion::Down(Step::Page)).offset(), 0);
        assert!(view.at_bottom());
        assert_eq!(view.visible_range(), 0..5);
    }

    #[test]
    fn steps_make_progress_in_a_tiny_terminal() {
        let view = Viewport::new(10, 1);
        assert_eq!(view.moved(Motion::Down(Step::HalfPage)).offset(), 1);
        assert_eq!(view.moved(Motion::Down(Step::Page)).offset(), 1);
        let zero = Viewport::new(10, 0);
        assert_eq!(zero.moved(Motion::Down(Step::Page)).offset(), 1);
    }

    #[test]
    fn height_change_reclamps_the_offset() {
        let view = Viewport::new(100, 10).moved(Motion::Bottom);
        assert_eq!(view.with_height(50).offset(), 50);
        assert_eq!(view.with_height(200).offset(), 0);
    }

    #[test]
    fn reflow_keeps_the_proportional_position() {
        let view = Viewport::new(100, 10).with_offset(40);
        assert_eq!(view.with_content(200).offset(), 80);
        assert_eq!(view.with_content(50).offset(), 20);
    }

    #[test]
    fn reflow_keeps_a_reader_at_the_bottom_at_the_bottom() {
        let view = Viewport::new(100, 10).moved(Motion::Bottom);
        assert!(view.with_content(300).at_bottom());
        assert!(view.with_content(30).at_bottom());
    }

    #[test]
    fn reflow_from_the_top_stays_at_the_top() {
        let view = Viewport::new(100, 10);
        assert_eq!(view.with_content(300).offset(), 0);
        assert_eq!(Viewport::new(0, 10).with_content(300).offset(), 0);
    }

    #[test]
    fn prompt_follows_less() {
        assert_eq!(
            Prompt::for_screen("README.md", true, false),
            Prompt::Standout(String::from("README.md"))
        );
        assert_eq!(
            Prompt::for_screen("README.md", true, true),
            Prompt::Standout(String::from("README.md (END)"))
        );
        assert_eq!(Prompt::for_screen("README.md", false, false), Prompt::Colon);
        assert_eq!(
            Prompt::for_screen("README.md", false, true),
            Prompt::Standout(String::from("(END)"))
        );
    }

    /// `lines` lines of text, each `link_from..` columns of which is a link.
    fn document(lines: usize, link_from: usize) -> Document {
        use crate::document::{DocLine, Run};
        use std::rc::Rc;

        Document::new(
            (0..lines)
                .map(|i| {
                    let text = format!("line {i} is wider than the screen");
                    let (plain, linked) = text.split_at(link_from);
                    DocLine::from_runs(vec![
                        Run::plain(plain, Style::default()),
                        Run::new(
                            linked,
                            Style::default(),
                            Some(Rc::from(format!("file:///{i}"))),
                        ),
                    ])
                })
                .collect(),
        )
    }

    /// Draws `lines` lines of text scrolled to `offset` on a 10x5 test
    /// terminal and returns the rows as strings.
    fn draw_rows(lines: usize, offset: usize, prompt: Prompt) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let doc = document(lines, 0);
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("test terminal");
        let viewport = Viewport::new(lines, 4).with_offset(offset);
        terminal
            .draw(|frame| {
                draw(frame, &doc, viewport, prompt);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        assert!(buffer[(0, 4)].modifier.contains(Modifier::REVERSED));
        (0..5)
            .map(|y| (0..10).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn draw_shows_the_visible_window_truncated_to_the_width() {
        let end = Prompt::Standout("(END)".into());
        assert_eq!(
            draw_rows(6, 2, end),
            [
                "line 2 is ",
                "line 3 is ",
                "line 4 is ",
                "line 5 is ",
                "(END)     "
            ]
        );
    }

    #[test]
    fn draw_places_visible_links_by_screen_row_clipped_to_the_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let doc = document(6, 5);
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("test terminal");
        let viewport = Viewport::new(6, 4).with_offset(2);
        let mut placed = Vec::new();
        terminal
            .draw(|frame| placed = draw(frame, &doc, viewport, Prompt::Colon))
            .expect("draw");

        let summary: Vec<(u16, Range<u16>, String)> = placed
            .iter()
            .map(|l| (l.y, l.x.clone(), l.target.to_string()))
            .collect();
        assert_eq!(
            summary,
            [
                (0, 5..10, String::from("file:///2")),
                (1, 5..10, String::from("file:///3")),
                (2, 5..10, String::from("file:///4")),
                (3, 5..10, String::from("file:///5")),
            ]
        );
    }

    #[test]
    fn draw_fills_rows_past_the_end_with_tildes() {
        let end = Prompt::Standout("(END)".into());
        assert_eq!(
            draw_rows(2, 0, end),
            [
                "line 0 is ",
                "line 1 is ",
                "~         ",
                "~         ",
                "(END)     "
            ]
        );
    }

    #[test]
    fn fixed_width_ignores_the_terminal() {
        let fixed = Width::Fixed(NonZeroUsize::new(72).expect("non-zero"));
        assert_eq!(fixed.columns(200), 72);
        assert_eq!(fixed.columns(10), 72);
        assert_eq!(Width::Auto.columns(120), 120);
        assert_eq!(Width::Auto.columns(0), 1);
    }
}
