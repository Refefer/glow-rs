//! `glow-rs`: render a markdown file in the terminal and page through it.
//!
//! Assumes stdout is an interactive terminal; when it is not (piped into
//! another command or redirected to a file) the rendered text is written
//! plainly, without styles, at the requested or an 80-column width so the
//! tool stays usable in scripts.
//!
//! Gotcha: `-w` is honored even when wider than the terminal — see
//! [`pager`] for the clipping behavior that implies.

#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]

mod document;
mod hyperlinks;
mod links;
mod markdown;
mod pager;
mod search;
mod wrap;

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use crate::document::Document;
use crate::links::LinkResolver;
use crate::pager::Width;

/// Width used for the non-interactive dump when `-w` is not given.
const PIPED_WIDTH: usize = 80;

#[derive(Parser, Debug)]
#[command(
    name = "glow-rs",
    version,
    about = "Render a markdown file in the terminal with less-style paging",
    after_help = "Keys: j/k/arrows line, J/K (d/u) half page, f/b/space/PgUp/PgDn page, \
                  g/G/Home/End ends, /pattern ?pattern search, n/N next/previous match, \
                  q/Esc quit."
)]
struct Cli {
    /// Markdown file to render.
    file: PathBuf,

    /// Column width to render at. Omit to follow the terminal width,
    /// re-wrapping whenever the window is resized.
    #[arg(short = 'w', long = "width", value_name = "COLS")]
    width: Option<NonZeroUsize>,
}

/// Everything that can stop a run before the pager exits cleanly.
#[derive(Debug)]
enum GlowError {
    Read { path: PathBuf, source: io::Error },
    Terminal(io::Error),
}

impl fmt::Display for GlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlowError::Read { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            GlowError::Terminal(source) => write!(f, "terminal error: {source}"),
        }
    }
}

impl From<io::Error> for GlowError {
    fn from(source: io::Error) -> Self {
        GlowError::Terminal(source)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("glow-rs: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), GlowError> {
    let source = std::fs::read_to_string(&cli.file).map_err(|source| GlowError::Read {
        path: cli.file.clone(),
        source,
    })?;
    let links = LinkResolver::for_document(&cli.file).map_err(|source| GlowError::Read {
        path: cli.file.clone(),
        source,
    })?;
    let render = |columns: usize| markdown::render_markdown(&source, columns, &links);

    if !io::stdout().is_terminal() {
        let columns = cli.width.map_or(PIPED_WIDTH, NonZeroUsize::get);
        return print_plain(&render(columns)).map_err(GlowError::Terminal);
    }

    let width = cli.width.map_or(Width::Auto, Width::Fixed);
    pager::run(&display_title(&cli.file), width, render)?;
    Ok(())
}

/// The file name alone for the prompt line; the full path is the shell's job.
fn display_title(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Writes the rendered lines without styling, one per row.
fn print_plain(doc: &Document) -> io::Result<()> {
    let mut out = io::stdout().lock();
    for line in doc.lines() {
        writeln!(out, "{}", line.text())?;
    }
    out.flush()
}
