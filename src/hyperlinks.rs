//! OSC 8 hyperlinks painted over a drawn frame.
//!
//! Assumes the terminal attaches the open hyperlink to every cell printed
//! while it is open and keeps it on those cells until they are printed again.
//! Neither ratatui nor crossterm knows about hyperlinks, so after each frame
//! the pager re-prints every link cell inside an OSC 8 open/close pair, and
//! re-prints last frame's link cells that are no longer links so they lose
//! theirs. Everything is re-printed from the frame buffer, so what the
//! terminal shows never diverges from what ratatui thinks is there. The
//! cursor is put back where ratatui left it, so a visible cursor (the search
//! prompt's) does not jump to the last link printed.
//!
//! Gotcha: this goes around ratatui's diff, which cannot notice a link change
//! under an unchanged cell (two links with identical text at the same spot).
//! Re-printing every link cell every frame is what makes that safe; the cost
//! is one screenful of link text per frame, which is small next to the scroll
//! redraw itself.

use std::collections::HashSet;
use std::io::{self, Write};
use std::ops::Range;
use std::rc::Rc;

use crossterm::cursor::{RestorePosition, SavePosition};
use crossterm::queue;
use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Position;
use unicode_width::UnicodeWidthStr;

/// A link's cells on screen: row `y`, columns `x`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedLink {
    pub y: u16,
    pub x: Range<u16>,
    pub target: Rc<str>,
}

/// Remembers which cells carried a link last frame so they can be cleared.
#[derive(Debug, Default)]
pub struct Painter {
    previous: HashSet<Position>,
}

/// The cells to re-print once a frame is on screen, in order.
#[derive(Debug, Default)]
pub struct Paint {
    runs: Vec<PaintRun>,
}

/// One stretch of cells printed under a single hyperlink (or none).
#[derive(Debug)]
struct PaintRun {
    target: Option<Rc<str>>,
    cells: Vec<(u16, u16, Cell)>,
}

impl Painter {
    /// Gathers from `buffer` what must be re-printed after this frame: every
    /// cell of `links`, then any cell that was a link last frame and is not
    /// one now.
    pub fn plan(&mut self, buffer: &Buffer, links: &[PlacedLink]) -> Paint {
        let mut current: HashSet<Position> = HashSet::new();
        let mut runs = Vec::with_capacity(links.len() + 1);
        for link in links {
            let cells = cells_in(buffer, link.y, link.x.clone());
            current.extend(cells.iter().map(|&(x, y, _)| Position::new(x, y)));
            if !cells.is_empty() {
                runs.push(PaintRun {
                    target: Some(Rc::clone(&link.target)),
                    cells,
                });
            }
        }

        let mut stale: Vec<Position> = self
            .previous
            .difference(&current)
            .copied()
            .filter(|&pos| buffer.area.contains(pos) && !is_continuation(buffer, pos))
            .collect();
        stale.sort_by_key(|pos| (pos.y, pos.x));
        let cells: Vec<(u16, u16, Cell)> = stale
            .into_iter()
            .map(|pos| (pos.x, pos.y, buffer[pos].clone()))
            .collect();
        if !cells.is_empty() {
            runs.push(PaintRun {
                target: None,
                cells,
            });
        }

        self.previous = current;
        Paint { runs }
    }
}

impl Paint {
    /// Whether there is anything to print.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Writes the runs to the terminal, each linked run wrapped in an OSC 8
    /// open/close pair. The `id` parameter ties every piece of one link
    /// together, so a link wrapped over several lines highlights as one.
    pub fn apply<B>(self, backend: &mut B) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        if self.is_empty() {
            return Ok(());
        }
        queue!(backend, SavePosition)?;
        for run in self.runs {
            if let Some(target) = &run.target {
                let id = Rc::as_ptr(target).cast::<u8>() as usize;
                write!(backend, "\x1b]8;id={id:x};{target}\x1b\\")?;
            }
            backend.draw(run.cells.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
            if run.target.is_some() {
                write!(backend, "\x1b]8;;\x1b\\")?;
            }
        }
        queue!(backend, RestorePosition)?;
        Backend::flush(backend)
    }
}

/// The cells of row `y` in columns `x`, clipped to the buffer, skipping the
/// trailing cells of wide characters the way ratatui's own diff does.
fn cells_in(buffer: &Buffer, y: u16, x: Range<u16>) -> Vec<(u16, u16, Cell)> {
    let mut cells = Vec::new();
    if y < buffer.area.top() || y >= buffer.area.bottom() {
        return cells;
    }
    let end = x.end.min(buffer.area.right());
    let mut column = x.start.max(buffer.area.left());
    while column < end {
        let cell = &buffer[(column, y)];
        let width = u16::try_from(cell.symbol().width().max(1)).unwrap_or(1);
        cells.push((column, y, cell.clone()));
        column = column.saturating_add(width);
    }
    cells
}

/// Whether `pos` is the trailing half of a wide character, which must not be
/// printed on its own or the terminal drops the character in front of it.
fn is_continuation(buffer: &Buffer, pos: Position) -> bool {
    pos.x > buffer.area.left()
        && buffer.area.contains(Position::new(pos.x - 1, pos.y))
        && buffer[(pos.x - 1, pos.y)].symbol().width() > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    /// A writer whose bytes the test can read back after the backend owns it.
    #[derive(Clone, Default)]
    struct Captured(Rc<std::cell::RefCell<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("utf8")
        }
    }

    fn buffer_with(rows: &[&str]) -> Buffer {
        let width = rows.iter().map(|r| r.width()).max().unwrap_or(0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width as u16, rows.len() as u16));
        for (y, row) in rows.iter().enumerate() {
            buffer.set_line(0, y as u16, &Line::from(*row), width as u16);
        }
        buffer
    }

    fn placed(y: u16, x: Range<u16>, target: &Rc<str>) -> PlacedLink {
        PlacedLink {
            y,
            x,
            target: Rc::clone(target),
        }
    }

    fn symbols(run: &PaintRun) -> String {
        run.cells.iter().map(|(_, _, c)| c.symbol()).collect()
    }

    #[test]
    fn plan_collects_link_cells_clipped_to_the_buffer() {
        let buffer = buffer_with(&["see docs here", "second"]);
        let target: Rc<str> = Rc::from("https://example.com");
        let mut painter = Painter::default();

        let paint = painter.plan(
            &buffer,
            &[placed(0, 4..8, &target), placed(0, 10..99, &target)],
        );

        assert_eq!(paint.runs.len(), 2);
        assert_eq!(symbols(&paint.runs[0]), "docs");
        assert_eq!(symbols(&paint.runs[1]), "ere");
        assert!(paint.runs.iter().all(|r| r.target.is_some()));
    }

    #[test]
    fn cells_that_stop_being_links_are_reprinted_without_one() {
        let buffer = buffer_with(&["abcdef"]);
        let target: Rc<str> = Rc::from("file:///x");
        let mut painter = Painter::default();

        painter.plan(&buffer, &[placed(0, 0..4, &target)]);
        let paint = painter.plan(&buffer, &[placed(0, 2..4, &target)]);

        assert_eq!(paint.runs.len(), 2);
        assert_eq!(symbols(&paint.runs[0]), "cd");
        assert!(paint.runs[0].target.is_some());
        assert_eq!(symbols(&paint.runs[1]), "ab");
        assert!(paint.runs[1].target.is_none());
    }

    #[test]
    fn wide_characters_are_printed_once() {
        let buffer = buffer_with(&["a界b"]);
        let target: Rc<str> = Rc::from("file:///x");
        let mut painter = Painter::default();

        let paint = painter.plan(&buffer, &[placed(0, 0..4, &target)]);
        let columns: Vec<u16> = paint.runs[0].cells.iter().map(|(x, _, _)| *x).collect();

        assert_eq!(columns, [0, 1, 3]);
        assert_eq!(symbols(&paint.runs[0]), "a界b");

        // Next frame nothing is linked: the wide char's trailing cell is not
        // re-printed on its own.
        let paint = painter.plan(&buffer, &[]);
        let columns: Vec<u16> = paint.runs[0].cells.iter().map(|(x, _, _)| *x).collect();
        assert_eq!(columns, [0, 1, 3]);
    }

    #[test]
    fn apply_wraps_linked_runs_in_osc8() {
        use ratatui::backend::CrosstermBackend;

        let buffer = buffer_with(&["docs"]);
        let target: Rc<str> = Rc::from("https://example.com/a");
        let paint = Painter::default().plan(&buffer, &[placed(0, 0..4, &target)]);
        let captured = Captured::default();
        let mut backend = CrosstermBackend::new(captured.clone());

        paint.apply(&mut backend).expect("apply");
        let out = captured.text();

        let open = out.find("\x1b]8;id=").expect("open sequence");
        let uri = out.find(";https://example.com/a\x1b\\").expect("uri");
        let close = out.rfind("\x1b]8;;\x1b\\").expect("close sequence");
        let text = out.find("docs").expect("text");
        assert!(open < uri && uri < text && text < close, "{out:?}");
        assert!(
            out.starts_with("\x1b7") && out.ends_with("\x1b8"),
            "cursor saved and restored around the paint: {out:?}"
        );
    }

    #[test]
    fn empty_paint_writes_nothing() {
        use ratatui::backend::CrosstermBackend;

        let captured = Captured::default();
        let mut backend = CrosstermBackend::new(captured.clone());
        Paint::default().apply(&mut backend).expect("apply");
        assert!(captured.text().is_empty());
    }
}
