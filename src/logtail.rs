//! The tail of a job's log file, for the preview window.
//!
//! Reading is a background job like every cluster command here: `/work` is a
//! shared filesystem, and a stat that stalls must not take the interface with
//! it. The path is the request, so the preview switching jobs discards
//! whatever the last one had in flight.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

use crate::poller::Poller;

/// How much of the end of the file to read. A training log runs to hundreds of
/// megabytes and the window shows a screenful, so reading the whole file to
/// find its last lines would cost the size of the file for a fixed answer.
const WINDOW: u64 = 64 * 1024;

/// How many lines to keep. Comfortably more than the window is tall, so there
/// is something to scroll back through, and bounded so that a file of one
/// enormous line cannot be held twice over.
const LINES: usize = 400;

/// How often to re-read. A running job is still writing, and a preview that
/// did not keep up would be a worse `tail -f` than the one you already have.
const INTERVAL: Duration = Duration::from_secs(1);

/// Read the last lines of `path`, newest last.
///
/// `None` is the idle request: with no preview open there is nothing to read,
/// and the thread waits for a path rather than being started and stopped with
/// the window.
pub fn fetch(path: Option<String>) -> io::Result<Vec<String>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };

    let mut file = File::open(&path)?;

    // Seek to the last `WINDOW` bytes rather than reading forward to them.
    let end = file.seek(SeekFrom::End(0))?;
    let from = end.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(from))?;

    let mut bytes = Vec::new();
    file.take(WINDOW).read_to_end(&mut bytes)?;

    Ok(tail(&String::from_utf8_lossy(&bytes), from > 0))
}

/// The lines to show from a chunk of the end of a file. `partial` says the
/// chunk started mid-file, where the first line is the tail of one whose
/// beginning was never read and is not one to show.
fn tail(text: &str, partial: bool) -> Vec<String> {
    let mut lines = text.lines();
    if partial {
        lines.next();
    }

    let lines: Vec<&str> = lines.collect();
    lines[lines.len().saturating_sub(LINES)..]
        .iter()
        .map(|line| readable(line))
        .collect()
}

/// What a terminal would have shown for one line.
///
/// A progress bar redraws itself by returning to the start of the line, so
/// everything before the last carriage return has already been written over;
/// keeping it would render one line of a `tqdm` bar as thousands of columns of
/// its own history. Colour codes and tabs are the other two things a log
/// carries that a table cell cannot place.
fn readable(line: &str) -> String {
    let line = line.rsplit('\r').next().unwrap_or(line);
    strip_ansi(line).replace('\t', "    ")
}

/// Drop the escape sequences a coloured log is full of. Anything from an
/// escape up to the letter that ends the sequence goes, which covers the
/// colour and cursor codes without needing to understand any of them.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line.chars();

    while let Some(c) = rest.next() {
        if c != '\u{1b}' {
            // Other control characters have no width and no meaning here. The
            // tab is the exception: it is spacing, and [`readable`] spells it
            // out once the escapes are gone.
            if !c.is_control() || c == '\t' {
                out.push(c);
            }
            continue;
        }

        // `ESC [ … <letter>` is the common form, and runs to the letter that
        // ends it; `ESC <letter>` is the short one, and is already over.
        if rest.next() == Some('[') {
            for c in rest.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }

    out
}

/// Start the reader. It idles until the preview asks for a path.
pub fn poll() -> Poller<Option<String>, Vec<String>> {
    Poller::spawn(INTERVAL, None, fetch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_every_line_of_a_short_file() {
        assert_eq!(tail("one\ntwo\nthree\n", false), ["one", "two", "three"]);
    }

    /// A chunk read from the middle of a file opens on the tail of a line
    /// whose beginning was never read.
    #[test]
    fn drops_the_line_the_window_cut_in_half() {
        assert_eq!(tail("ff 0.83\nnext\n", true), ["next"]);
        // The same text, read from the start of the file, keeps it.
        assert_eq!(tail("ff 0.83\nnext\n", false), ["ff 0.83", "next"]);
    }

    #[test]
    fn keeps_only_the_last_lines() {
        let text: String = (0..LINES * 2).map(|n| format!("line {n}\n")).collect();
        let lines = tail(&text, false);

        assert_eq!(lines.len(), LINES);
        assert_eq!(lines[0], format!("line {}", LINES));
        assert_eq!(lines[LINES - 1], format!("line {}", LINES * 2 - 1));
    }

    /// A progress bar rewrites one line over and over; only its last state was
    /// ever on screen.
    #[test]
    fn shows_only_what_a_progress_bar_last_wrote() {
        assert_eq!(
            readable("10%|## |\r50%|##### |\r99%|#########|"),
            "99%|#########|"
        );
    }

    #[test]
    fn drops_colour_codes() {
        assert_eq!(
            readable("\u{1b}[31mFAILED\u{1b}[0m: step 3"),
            "FAILED: step 3"
        );
        assert_eq!(readable("\u{1b}[1;33mwarn\u{1b}[m x"), "warn x");
    }

    #[test]
    fn spells_a_tab_out_in_spaces() {
        assert_eq!(readable("loss\t0.83"), "loss    0.83");
    }

    /// Nothing to read is not an error: it is the reader with no preview open.
    #[test]
    fn reads_nothing_when_no_preview_is_open() {
        assert_eq!(fetch(None).expect("idle"), Vec::<String>::new());
    }

    /// A pending job names a file that does not exist yet, which the window
    /// has to report rather than showing as an empty log.
    #[test]
    fn reports_a_file_that_is_not_there() {
        assert!(fetch(Some("/nonexistent/slurm-1.out".to_string())).is_err());
    }
}
