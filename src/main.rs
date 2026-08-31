mod cost;
mod logfile;
mod logtail;
mod poller;
mod sacct;
mod scancel;
mod squeue;
mod tres;
mod update;
mod wallet;

use std::io;
use std::ops::Range;
use std::time::{Duration, Instant};

use chrono::Local;
use clap::{Parser, Subcommand};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::poller::Poller;
use crate::sacct::Run;
use crate::scancel::Cancels;
use crate::squeue::Job;
use crate::update::Outcome;
use crate::wallet::Project;

/// A top-like view of Slurm jobs on NCHC clusters.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Do not check for a newer release at startup.
    #[arg(long)]
    no_update: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Install the latest release and exit, without opening the screen.
    Update {
        /// Install it even when it is the version already here.
        #[arg(long)]
        force: bool,
    },
}

/// How long to wait for input before looking at the pollers again. Also the
/// longest a finished fetch can sit in a channel before it reaches the screen.
const TICK: Duration = Duration::from_millis(100);

/// How long the first `d` waits for the second. Long enough to be a double
/// press, short enough that a `d` you have since forgotten about cannot be
/// answered by one you meant as the first of a pair.
const CONFIRM: Duration = Duration::from_secs(1);

/// Something that renders as a table row.
trait Rows {
    /// Column titles and widths, in render order.
    const COLUMNS: &'static [(&'static str, Constraint)];

    /// What to call these in the pane title.
    const NOUN: &'static str;

    /// The cells, in `COLUMNS` order. Cells rather than a finished [`Row`],
    /// because the pane drops the ones scrolled off the left before it builds
    /// one; see [`Pane::draw`].
    fn cells(&self) -> Vec<Line<'_>>;

    /// The job this row is, for a preview window to name.
    fn id(&self) -> &str;

    /// Where it writes its output, or `-` for a job that writes no file.
    fn log(&self) -> &str;
}

impl Rows for Job {
    const COLUMNS: &'static [(&'static str, Constraint)] = &[
        ("JOBID", Constraint::Length(8)),
        ("PARTITION", Constraint::Length(9)),
        ("NAME", Constraint::Min(14)),
        ("USER", Constraint::Length(9)),
        ("STATE", Constraint::Length(9)),
        ("CPU", Constraint::Length(4)),
        ("GPU", Constraint::Length(3)),
        ("MEM", Constraint::Length(6)),
        ("TIME", Constraint::Length(10)),
        ("NODES", Constraint::Length(5)),
        ("NODELIST(REASON)", Constraint::Min(18)),
        ("LOG", Constraint::Min(24)),
    ];

    const NOUN: &'static str = "jobs";

    fn cells(&self) -> Vec<Line<'_>> {
        vec![
            Line::raw(&self.id),
            Line::raw(&self.partition),
            Line::raw(&self.name),
            Line::raw(&self.user),
            Line::styled(&self.state, state_style(&self.state)),
            Line::raw(&self.cpus),
            Line::raw(&self.gpus),
            Line::raw(&self.mem),
            Line::raw(&self.time),
            Line::raw(&self.nodes),
            Line::raw(&self.reason),
            Line::raw(&self.log),
        ]
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn log(&self) -> &str {
        &self.log
    }
}

impl Rows for Run {
    const COLUMNS: &'static [(&'static str, Constraint)] = &[
        ("JOBID", Constraint::Length(9)),
        ("PARTITION", Constraint::Length(9)),
        ("NAME", Constraint::Min(14)),
        ("USER", Constraint::Length(9)),
        ("STATE", Constraint::Length(13)),
        ("CPU", Constraint::Length(4)),
        ("GPU", Constraint::Length(3)),
        ("MEM", Constraint::Length(6)),
        ("ELAPSED", Constraint::Length(9)),
        ("END", Constraint::Length(11)),
        ("EXIT", Constraint::Length(6)),
        ("LOG", Constraint::Min(24)),
    ];

    const NOUN: &'static str = "jobs";

    fn cells(&self) -> Vec<Line<'_>> {
        vec![
            Line::raw(&self.id),
            Line::raw(&self.partition),
            Line::raw(&self.name),
            Line::raw(&self.user),
            Line::styled(&self.state, state_style(&self.state)),
            Line::raw(&self.cpus),
            Line::raw(&self.gpus),
            Line::raw(&self.mem),
            Line::raw(&self.elapsed),
            Line::raw(&self.end),
            Line::raw(&self.exit),
            Line::raw(&self.log),
        ]
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn log(&self) -> &str {
        &self.log
    }
}

/// A box `width` and `height` per cent of `area`, in the middle of it.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let box_width = area.width * width / 100;
    let box_height = area.height * height / 100;

    Rect {
        x: area.x + (area.width - box_width) / 2,
        y: area.y + (area.height - box_height) / 2,
        width: box_width,
        height: box_height,
    }
}

/// Colour a state the way you skim for one: green is fine, red is not.
fn state_style(state: &str) -> Style {
    match state {
        "RUNNING" | "COMPLETED" => Style::new().fg(Color::Green),
        "PENDING" | "SUSPENDED" | "REQUEUED" => Style::new().fg(Color::Yellow),
        "FAILED" | "TIMEOUT" | "CANCELLED" | "OUT_OF_MEMORY" | "NODE_FAIL" => {
            Style::new().fg(Color::Red)
        }
        _ => Style::new(),
    }
}

/// Rows from one background command, and how its last fetch went. Shared by
/// the two job panes and the wallet header, which differ only in how they
/// render what arrives.
struct Feed<Req, T> {
    items: Vec<T>,
    /// Last failure, shown in place of the row count.
    error: Option<String>,
    /// Cleared once the first fetch comes back, so an empty result and one we
    /// have not read yet do not look the same.
    loading: bool,
    /// Dropped on quit, which stops the thread behind it.
    poller: Poller<Req, Vec<T>>,
}

impl<Req: PartialEq, T> Feed<Req, T> {
    fn new(poller: Poller<Req, Vec<T>>) -> Self {
        Self {
            items: Vec::new(),
            error: None,
            loading: true,
            poller,
        }
    }

    /// Take whatever the poller has finished, keeping the previous rows on
    /// screen if the command failed. Reports whether anything arrived, since
    /// a caller holding a selection has to react.
    fn apply_updates(&mut self, request: &Req) -> bool {
        // Collected up front so the loop below can borrow self mutably, and
        // filtered so a fetch still in flight when `m` was pressed is ignored.
        let updates: Vec<_> = self
            .poller
            .drain()
            .filter(|(fetched, _)| fetched == request)
            .collect();
        if updates.is_empty() {
            return false;
        }

        for (_, update) in updates {
            match update {
                Ok(items) => {
                    self.items = items;
                    self.error = None;
                }
                Err(err) => self.error = Some(err.to_string()),
            }
        }

        self.loading = false;
        true
    }

    /// Ask for a fresh fetch now, rather than at the next tick.
    fn refresh(&self, request: Req) {
        self.poller.request(request);
    }

    /// How the last fetch went: an error, or how many rows came back. Shown
    /// next to the thing it describes, because with several commands on screen
    /// one shared status line could not say which had failed.
    fn status(&self, noun: &str) -> Span<'static> {
        match (&self.error, self.loading) {
            (Some(err), _) => Span::styled(err.clone(), Color::Red),
            (None, true) => Span::styled("loading", Color::DarkGray),
            (None, false) => {
                Span::styled(format!("{} {}", self.items.len(), noun), Color::DarkGray)
            }
        }
    }
}

/// One list: its feed, and how it is scrolled. Both panes keep their own, so
/// moving focus does not lose your place in the other.
///
/// The scroll position is ours rather than the table widget's, because the
/// widget only ever sees one screenful; see [`Pane::window`].
struct Pane<Req, T> {
    feed: Feed<Req, T>,
    /// Index into `feed.items`, not into what is on screen.
    selected: usize,
    /// The first of `feed.items` on screen.
    offset: usize,
    /// The first of `COLUMNS` on screen. A row wider than the terminal is the
    /// normal case here rather than an edge one — there are eleven columns
    /// before the log path — so `h` and `l` walk the table sideways instead of
    /// the columns fighting each other for the width.
    column: usize,
    /// Rows visible in this pane's body, measured at draw time for ctrl-d/u.
    page: usize,
}

impl<Req: PartialEq, T: Rows> Pane<Req, T> {
    fn new(poller: Poller<Req, Vec<T>>) -> Self {
        Self {
            feed: Feed::new(poller),
            selected: 0,
            offset: 0,
            column: 0,
            page: 0,
        }
    }

    /// Reports whether anything arrived, so the caller knows to redraw.
    fn apply_updates(&mut self, request: Req) -> bool {
        if !self.feed.apply_updates(&request) {
            return false;
        }

        // The list can shrink out from under the selection.
        self.selected = self.selected.min(self.feed.items.len().saturating_sub(1));
        true
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, name: &str, focused: bool) {
        // Body height less the two borders and the header row.
        self.page = usize::from(area.height.saturating_sub(3));

        let title = Line::from(vec![
            Span::raw(format!(" {name} · ")),
            self.feed.status(T::NOUN),
            Span::raw(" "),
        ]);

        let border = if focused {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new().fg(Color::DarkGray)
        };

        // Only the focused pane shows a cursor, so there is never a question
        // about which one j/k is about to move.
        let highlight = if focused {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };

        // Everything from the column `h`/`l` has scrolled to, rightwards. The
        // last column can be reached on its own, so there is always a way to
        // read a long path whole.
        let first = self.column.min(T::COLUMNS.len().saturating_sub(1));
        let columns = &T::COLUMNS[first..];

        let header = Row::new(columns.iter().map(|(title, _)| *title))
            .style(Style::new().add_modifier(Modifier::BOLD));
        let window = self.window();
        let rows = self.feed.items[window.clone()]
            .iter()
            .map(|item| Row::new(item.cells().into_iter().skip(first)));
        let table = Table::new(rows, columns.iter().map(|(_, width)| *width))
            .header(header)
            .row_highlight_style(highlight)
            .block(Block::bordered().border_style(border).title(title));

        // The widget is given the window alone, so the selection is renumbered
        // against it and the scrolling it would do itself has nothing left to
        // do.
        let mut state = TableState::new().with_selected(self.selected - window.start);
        frame.render_stateful_widget(table, area, &mut state);
    }
}

/// The movement keys, on whichever pane has focus. A trait rather than a match
/// per handler because the two panes hold different row types, so there is no
/// common `&mut Pane<_>` to reach for.
trait Scroll {
    fn select_next(&mut self);
    fn select_previous(&mut self);

    /// Move the selection half a screen, the way vim's ctrl-d / ctrl-u do.
    fn scroll_half(&mut self, down: bool);

    /// Move the table one column left or right, stopping at either end.
    fn scroll_column(&mut self, right: bool);

    /// The selected row's job id and log path, for `p` to open. `None` when
    /// there is no selection, or when that job writes no log.
    fn selected_log(&self) -> Option<(String, String)>;
}

impl<Req, T> Pane<Req, T> {
    /// The rows to hand the table this frame, scrolled to keep the selection
    /// on screen.
    ///
    /// [`Table`] collects every row it is given before drawing the handful
    /// that fit, so passing it the whole list costs a frame proportional to
    /// the number of jobs rather than to the size of the terminal — at the
    /// tens of thousands `sacct` reports for every user over a month, half a
    /// second of it. Scrolling here and passing only the window keeps a draw
    /// the same price at eighty jobs and at eighty thousand.
    fn window(&mut self) -> Range<usize> {
        let len = self.feed.items.len();
        if self.page == 0 || len == 0 {
            return 0..0;
        }

        self.selected = self.selected.min(len - 1);

        // Move by just enough to bring the selection back into view, so the
        // list holds still whenever it can.
        self.offset = self
            .offset
            .clamp(self.selected.saturating_sub(self.page - 1), self.selected)
            // A list that just shrank must not leave the window off the end.
            .min(len.saturating_sub(self.page));

        self.offset..(self.offset + self.page).min(len)
    }

    /// Move the selection `rows` up or down, stopping at either end.
    ///
    /// The clamp is ours to apply: the table widget is only ever shown a
    /// window, so it cannot pull a selection back from past the end the way
    /// it could when it held the whole list.
    fn move_selection(&mut self, rows: usize, down: bool) {
        let Some(last) = self.feed.items.len().checked_sub(1) else {
            return; // Nothing to select.
        };

        self.selected = if down {
            self.selected.saturating_add(rows).min(last)
        } else {
            self.selected.saturating_sub(rows)
        };
    }
}

impl<Req, T: Rows> Scroll for Pane<Req, T> {
    fn select_next(&mut self) {
        self.move_selection(1, true);
    }

    fn select_previous(&mut self) {
        self.move_selection(1, false);
    }

    fn scroll_half(&mut self, down: bool) {
        self.move_selection((self.page / 2).max(1), down);
    }

    fn scroll_column(&mut self, right: bool) {
        // Stop with the last column still on screen; scrolling past the end
        // would leave the pane empty with no sign of which way back.
        let last = T::COLUMNS.len().saturating_sub(1);
        self.column = if right {
            (self.column + 1).min(last)
        } else {
            self.column.saturating_sub(1)
        };
    }

    fn selected_log(&self) -> Option<(String, String)> {
        let row = self.feed.items.get(self.selected)?;
        match row.log() {
            // The dash the column shows for a job Slurm recorded no file for.
            "-" | "" => None,
            path => Some((row.id().to_string(), path.to_string())),
        }
    }
}

/// An open preview window: which job's log, and how far back through it.
///
/// The path is held rather than looked up again each frame, so a refresh
/// landing while the window is open cannot slide a different job's log under
/// it — the same reason `dd` reads its job id when the question is asked.
struct Preview {
    id: String,
    path: String,
    /// Lines scrolled up from the end. Zero is the live end of the file, which
    /// is where a preview opens and where a running job keeps writing.
    back: usize,
}

/// Which pane the movement keys act on. Both stay on screen and both keep
/// polling either way.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Queue,
    Recent,
}

/// What the footer is saying about a cancellation, in place of the keys.
///
/// A confirmation rather than a key that acts on its own: `d` sits next to the
/// movement keys, and one stray press must not take a job down with it.
enum Prompt {
    /// The first `d`: the next key, within [`CONFIRM`], decides.
    ///
    /// The job id is read when the prompt is armed rather than when it is
    /// answered, so a queue that refreshes between the two presses cannot slide
    /// a different job under the cursor and have it cancelled instead.
    Confirm(String, Instant),
    /// `scancel` asked for, and not yet back.
    Sent(String),
    /// Slurm took it. The job leaves the queue on its own time, so this says
    /// what was asked rather than that the job is gone.
    Done(String),
    /// It did not, and why — the whole reason a result is reported at all.
    Failed(String),
}

impl Prompt {
    fn line(&self) -> Line<'static> {
        match self {
            Self::Confirm(id, _) => Line::from(vec![
                Span::styled(format!(" d again to cancel job {id}"), Color::Yellow),
                Span::styled(" · anything else, or a pause, keeps it", Color::DarkGray),
            ]),
            Self::Sent(id) => Line::styled(format!(" cancelling {id} …"), Color::DarkGray),
            Self::Done(id) => Line::styled(format!(" cancelled {id}"), Color::Green),
            Self::Failed(err) => Line::styled(format!(" cancel · {err}"), Color::Red),
        }
    }
}

struct App {
    wallet: Feed<(), Project>,
    queue: Pane<bool, Job>,
    recent: Pane<(), Run>,
    /// What the recent runs cost, over each of [`cost::WINDOWS`]. Totalled when
    /// a fetch lands rather than when the line is drawn: a month of everyone's
    /// jobs is tens of thousands of rows, and holding `j` must not re-add them
    /// all for every frame.
    costs: [f64; cost::WINDOWS.len()],
    focus: Focus,
    /// Whether the queue asks `squeue` for only the current user's jobs. The
    /// recent pane is always the current user's, whatever this says.
    only_me: bool,
    /// The startup update check, which reports into the footer.
    update: update::Check,
    /// Cancellations asked for with `dd`, running in the background.
    cancels: Cancels,
    /// What the footer says instead of the keys, when a cancellation has
    /// something to ask or to report.
    prompt: Option<Prompt>,
    /// The floating log window, when `p` has opened one.
    preview: Option<Preview>,
    /// Lines visible in that window, measured at draw time for ctrl-d/ctrl-u.
    preview_page: usize,
    /// The lines behind it, re-read while it is open. One feed rather than one
    /// per window: only ever one is open.
    tail: Feed<Option<String>, String>,
    should_quit: bool,
}

impl App {
    fn new(check_update: bool) -> Self {
        let only_me = true;

        Self {
            wallet: Feed::new(wallet::poll()),
            queue: Pane::new(squeue::poll(only_me)),
            recent: Pane::new(sacct::poll()),
            costs: [0.0; cost::WINDOWS.len()],
            focus: Focus::Queue,
            only_me,
            update: if check_update {
                update::Check::start()
            } else {
                update::Check::idle()
            },
            cancels: Cancels::new(),
            prompt: None,
            preview: None,
            preview_page: 0,
            tail: Feed::new(logtail::poll()),
            should_quit: false,
        }
    }

    /// Draw, then wait for something to happen; repeat.
    ///
    /// Only a change earns a frame. Drawing every tick regardless spent a
    /// whole frame ten times a second whether or not anything had moved,
    /// which on the longer lists was more work than a tick is long.
    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        // Nothing is on screen yet.
        let mut dirty = true;

        while !self.should_quit {
            // Drained every pass, dirty or not: a result left sitting in a
            // channel is a pane showing something stale.
            dirty |= self.update.apply_update();
            dirty |= self.expire_prompt();
            dirty |= self.apply_cancels();
            dirty |= self.wallet.apply_updates(&());
            dirty |= self.queue.apply_updates(self.only_me);
            if self.recent.apply_updates(()) {
                self.costs = cost::totals(&self.recent.feed.items, Local::now().naive_local());
                dirty = true;
            }
            dirty |= self.apply_tail();

            if dirty {
                terminal.draw(|frame| self.draw(frame))?;
            }

            dirty = self.handle_events()?;
        }

        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        // An even split: the queue is usually the shorter list, but it is also
        // the one worth watching, so neither side earns the extra rows.
        let [header, queue, recent, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        self.draw_header(frame, header);

        let focus = self.focus;
        // Only the queue has a scope to name; the other pane is always yours.
        let scope = if self.only_me {
            "queue · mine"
        } else {
            "queue · all"
        };
        self.queue.draw(frame, queue, scope, focus == Focus::Queue);
        self.recent
            .draw(frame, recent, "last 30d", focus == Focus::Recent);
        self.draw_footer(frame, footer);

        // Last, and over the whole screen rather than one pane: it is a window
        // on top of the interface, not a third pane in it.
        self.draw_preview(frame, frame.area());
    }

    /// How much of the screen the log window takes. Wide, because a log line
    /// is long and the window does not wrap them; short of the full screen,
    /// because the panes behind it are the context for what it is showing.
    const PREVIEW: (u16, u16) = (90, 70);

    /// The floating window: the end of the selected job's log, re-read while
    /// it is open so a running job's window keeps up with it.
    fn draw_preview(&mut self, frame: &mut Frame, screen: Rect) {
        let Some(preview) = &self.preview else {
            return;
        };

        let (width, height) = Self::PREVIEW;
        let area = centred(screen, width, height);

        // The block's own borders are not room for lines.
        let body = area.height.saturating_sub(2) as usize;
        self.preview_page = body;

        let lines = &self.tail.items;
        // Stop scrolling with the first line we hold at the top: there is
        // nothing above it to reach.
        let back = preview.back.min(lines.len().saturating_sub(body));
        let end = lines.len() - back;
        let start = end.saturating_sub(body);

        let body_text: Vec<Line> = match (&self.tail.error, self.tail.loading) {
            (Some(err), _) => vec![Line::styled(err.clone(), Color::Red)],
            (None, true) => vec![Line::styled("reading…", Color::DarkGray)],
            // A file Slurm has named but the job has not written to yet.
            (None, false) if lines.is_empty() => {
                vec![Line::styled("empty", Color::DarkGray)]
            }
            (None, false) => lines[start..end].iter().map(Line::raw).collect(),
        };

        let title = Line::from(vec![
            Span::raw(format!(" {} · ", preview.id)),
            Span::styled(preview.path.clone(), Color::DarkGray),
            Span::raw(" "),
        ]);

        // Which end of the file is on screen. A running job writes to the
        // bottom, so saying when you are *not* there is the useful half.
        let footer = Line::from(if back == 0 {
            Span::styled(" following ", Color::Green)
        } else {
            Span::styled(
                format!(" {back} lines back · paused · G to follow "),
                Color::Yellow,
            )
        })
        .right_aligned();

        let block = Block::bordered()
            .border_style(Style::new().fg(Color::Cyan))
            .title(title)
            .title_bottom(footer);

        // The panes underneath would otherwise show through the gaps.
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(body_text).block(block), area);
    }

    /// The footer: the keys on the left, what the update check came to on the
    /// right. The note is dropped rather than shortened when the terminal is
    /// narrow, because the keys are what you are down here to read.
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let keys = self.footer();
        let note = self.update_note();

        let room = usize::from(area.width).saturating_sub(keys.width());
        let width = if note.width() <= room {
            u16::try_from(note.width()).unwrap_or(0)
        } else {
            0
        };

        let [keys_area, note_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(width)]).areas(area);

        frame.render_widget(keys, keys_area);
        frame.render_widget(note.right_aligned(), note_area);
    }

    /// What the update check came to, once it has anything to say. A finished
    /// update is a note and not a prompt: the binary on disk is already the
    /// new one, and this session carries on as the old one.
    fn update_note(&self) -> Line<'static> {
        match self.update.outcome() {
            // Nothing yet, or nothing worth a line: being current is the
            // usual case, and saying so every start is noise.
            None | Some(Outcome::Current) => Line::default(),
            Some(Outcome::Installed(version)) => Line::from(vec![
                Span::styled(format!("updated to {version}"), Color::Green),
                Span::styled(" · restart to use it ", Color::DarkGray),
            ]),
            Some(Outcome::Failed(reason)) => {
                Line::styled(format!("update · {reason} "), Color::DarkGray)
            }
        }
    }

    /// The header: balances on the left, what they have been spent on to the
    /// right. One line for both, so the panes keep the row.
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let wallet = self.wallet_line();

        // The balance is what you would still want to see on a terminal too
        // narrow for both, so it is measured out first and the cost takes
        // whatever is left over.
        let width = u16::try_from(wallet.width()).unwrap_or(u16::MAX);
        let [balances, costs] =
            Layout::horizontal([Constraint::Length(width), Constraint::Fill(1)]).areas(area);

        frame.render_widget(wallet, balances);

        // A right-aligned line that does not fit is truncated from its left,
        // which would leave a fragment of a total sitting against the
        // balances. Better to show one window fewer, or none at all.
        let room = usize::from(costs.width);
        let cost = self.cost_line(room);
        if cost.width() <= room {
            frame.render_widget(cost.right_aligned(), costs);
        }
    }

    /// The SU balance of every project you belong to, which is the number worth
    /// seeing before you submit anything.
    fn wallet_line(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(" wallet", Color::DarkGray)];

        if self.wallet.items.is_empty() {
            spans.push(Span::styled(" · ", Color::DarkGray));
            spans.push(self.wallet.status("projects"));
            return Line::from(spans);
        }

        for project in &self.wallet.items {
            spans.push(Span::styled(format!(" · {} ", project.id), Color::DarkGray));
            spans.push(Span::styled(
                format!("{:.1} SU", project.balance),
                // An overdrawn project is the whole reason to show this line.
                if project.balance < 0.0 {
                    Color::Red
                } else {
                    Color::Green
                },
            ));
        }

        Line::from(spans)
    }

    /// What the recent runs have cost, over as many of the windows as `width`
    /// has room for. The balances say what is left; this says how fast it is
    /// going.
    fn cost_line(&self, width: usize) -> Line<'static> {
        const LABEL: &str = "cost";

        let mut spans = vec![Span::styled(LABEL, Color::DarkGray)];

        if self.recent.feed.loading {
            // Zeroes before the first fetch would read as a month of free
            // compute.
            spans.push(Span::styled(" · loading", Color::DarkGray));
        } else {
            // One column of it goes to the margin below.
            let mut room = width.saturating_sub(LABEL.len() + 1);
            let mut windows = Vec::new();

            for (days, total) in cost::WINDOWS.iter().zip(self.costs) {
                let window = format!(" · {days}d ");
                let amount = format!("{total:.1} SU");

                // A window that does not fit is dropped whole, longest first:
                // half a number still reads as a number, and every window left
                // is still labelled with the days it covers.
                let Some(left) = room.checked_sub(window.len() + amount.len()) else {
                    break;
                };
                room = left;

                windows.push(Span::styled(window, Color::DarkGray));
                windows.push(Span::raw(amount));
            }

            // Not even one window fits, and a label with no total beside it is
            // not worth the columns it would take from the balances.
            if windows.is_empty() {
                return Line::default();
            }

            spans.extend(windows);
        }

        // Right-aligned, so the margin that keeps this off the edge goes on the
        // opposite end from the wallet's.
        spans.push(Span::raw(" "));
        Line::from(spans)
    }

    fn footer(&self) -> Line<'static> {
        // A cancellation has the row while it has something to ask or report:
        // the keys are always the same, and this is not.
        if let Some(prompt) = &self.prompt {
            return prompt.line();
        }

        // An open window has the keys, so it has the row that lists them.
        if self.preview.is_some() {
            return Line::from(" j/k scroll · ^d/^u page · G follow · p/esc close")
                .fg(Color::DarkGray);
        }

        Line::from(format!(
            " tab focus · j/k move · h/l cols · ^d/^u page · p log · {}m queue {} · r refresh · q quit",
            // Only the queue holds jobs there is still anything to cancel.
            if self.focus == Focus::Queue {
                "dd cancel · "
            } else {
                ""
            },
            // What a press would switch to; which scope the queue is on now is
            // in its own title, next to the rows it decides.
            if self.only_me { "all" } else { "mine" },
        ))
        .fg(Color::DarkGray)
    }

    /// Arm a cancellation, or carry out the one already armed.
    ///
    /// Only the queue: the other pane is jobs that have already ended, and
    /// `scancel` has nothing to say to those.
    fn cancel_selected(&mut self, prompt: Option<Prompt>) -> bool {
        if self.focus != Focus::Queue {
            return false;
        }

        match prompt {
            // The second press, in time: cancel the job the first one named.
            Some(Prompt::Confirm(id, armed)) if armed.elapsed() < CONFIRM => {
                self.cancels.request(id.clone());
                self.prompt = Some(Prompt::Sent(id));
            }
            // The first press, or one so late the prompt it answers is gone.
            _ => {
                let Some(job) = self.queue.feed.items.get(self.queue.selected) else {
                    return false; // An empty queue has nothing to ask about.
                };
                self.prompt = Some(Prompt::Confirm(job.id.clone(), Instant::now()));
            }
        }

        true
    }

    /// Drop a confirmation nobody answered in time, so the footer stops
    /// offering what a `d` would no longer do.
    fn expire_prompt(&mut self) -> bool {
        if matches!(&self.prompt, Some(Prompt::Confirm(_, armed)) if armed.elapsed() >= CONFIRM) {
            self.prompt = None;
            return true;
        }

        false
    }

    /// Take whatever cancellations have come back and report the last of them.
    /// Several at once is a footer's worth of one line, and the one still to be
    /// told about is the one that just landed.
    fn apply_cancels(&mut self) -> bool {
        // Collected up front, so the loop can borrow self mutably.
        let finished: Vec<_> = self.cancels.drain().collect();
        if finished.is_empty() {
            return false;
        }

        let mut cancelled = false;

        for (id, result) in finished {
            self.prompt = Some(match result {
                Ok(()) => {
                    cancelled = true;
                    Prompt::Done(id)
                }
                Err(err) => Prompt::Failed(err.to_string()),
            });
        }

        // The queue is a second out of date at worst, but the pane is the only
        // confirmation that matters, so ask for it now rather than at the next
        // tick.
        if cancelled {
            self.queue.feed.refresh(self.only_me);
        }

        true
    }

    /// Switch the queue between the current user's jobs and everyone's.
    ///
    /// The queue alone: the recent pane answers what *you* have been running
    /// and what it cost, so a month of the whole cluster is never what is
    /// wanted there, and the costs beside it are read against your own balance.
    fn toggle_scope(&mut self) {
        self.only_me = !self.only_me;
        // The list is about to be replaced wholesale, so start at the top
        // rather than partway down whatever arrives.
        self.queue.selected = 0;
        self.queue.feed.refresh(self.only_me);
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Queue => Focus::Recent,
            Focus::Recent => Focus::Queue,
        };
    }

    /// Refresh everything on screen; it is all shown together, so it should
    /// agree.
    fn refresh(&self) {
        self.wallet.refresh(());
        self.queue.feed.refresh(self.only_me);
        self.recent.feed.refresh(());
    }

    /// Wait up to a tick for input, then take everything else already queued.
    /// Reports whether any of it changed what is on screen.
    ///
    /// The whole burst is handled before the next frame, so holding j or k
    /// costs one draw rather than one per key repeat, and the selection comes
    /// to rest where the key was let go instead of running on through a
    /// backlog of frames.
    fn handle_events(&mut self) -> io::Result<bool> {
        if !event::poll(TICK)? {
            return Ok(false);
        }

        let mut changed = false;

        // Polling with no timeout is how to ask whether another event is
        // already waiting; `read` alone would block once the burst ran out.
        while event::poll(Duration::ZERO)? {
            changed |= self.handle_event(event::read()?);
            if self.should_quit {
                break;
            }
        }

        Ok(changed)
    }

    /// Reports whether the event changed what is on screen, so a key we do
    /// nothing with does not cost a frame.
    fn handle_event(&mut self, event: Event) -> bool {
        let key = match event {
            // Everything has to be laid out again at the new size.
            Event::Resize(..) => return true,
            Event::Key(key) if key.kind == KeyEventKind::Press => key,
            _ => return false,
        };

        // Every press answers whatever the footer was saying: a `d` waiting on
        // its pair is answered by anything else as no, and a result that has
        // been read has had its row.
        let prompt = self.prompt.take();
        let changed = prompt.is_some();

        // An open window is modal: it is one screenful of one file, and the
        // keys that move it are the same ones that move a pane.
        if self.preview.is_some() {
            return self.handle_preview_key(key) || changed;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            // Raw mode swallows SIGINT, so quit on ctrl-c ourselves.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.focused().scroll_half(true)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.focused().scroll_half(false)
            }
            KeyCode::Char('j') | KeyCode::Down => self.focused().select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.focused().select_previous(),
            KeyCode::Char('l') | KeyCode::Right => self.focused().scroll_column(true),
            KeyCode::Char('h') | KeyCode::Left => self.focused().scroll_column(false),
            KeyCode::Char('d') => return self.cancel_selected(prompt) || changed,
            KeyCode::Char('p') => return self.open_preview() || changed,
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Char('m') => self.toggle_scope(),
            KeyCode::Char('r') => self.refresh(),
            // A key we do nothing with still clears the footer, if it had
            // anything on it.
            _ => return changed,
        }

        true
    }

    /// The path the open window is reading, which is also the reader's
    /// request. `None` leaves the reader idle: no window is open, or the one
    /// that is has been scrolled back off the end of the file.
    fn previewing(&self) -> Option<String> {
        match &self.preview {
            Some(preview) if preview.back == 0 => Some(preview.path.clone()),
            _ => None,
        }
    }

    /// Whether the window is sitting at the live end of the file, which is
    /// the only place new lines can be added without moving what is on screen.
    fn following(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|preview| preview.back == 0)
    }

    /// Take the reader's latest lines, if the window is in a state to accept
    /// them.
    ///
    /// A window scrolled back is reading history: lines arriving at the end
    /// would slide it down by however many the job just wrote, out from under
    /// whoever is reading it. So it keeps what it has until `G` returns it to
    /// the end. [`App::previewing`] idles the reader to match, so this is not
    /// a queue building up behind a closed door.
    fn apply_tail(&mut self) -> bool {
        if !self.following() {
            return false;
        }

        self.tail.apply_updates(&self.previewing())
    }

    /// Open a window on the selected job's log.
    ///
    /// Reports whether anything changed, so a `p` on a job with no log leaves
    /// the screen alone rather than flashing an empty window at it.
    fn open_preview(&mut self) -> bool {
        let Some((id, path)) = self.focused().selected_log() else {
            return false;
        };

        self.preview = Some(Preview {
            id,
            path: path.clone(),
            back: 0,
        });
        // Nothing read yet: the window says so rather than showing the last
        // job's lines while this one's are on their way.
        self.tail.items.clear();
        self.tail.error = None;
        self.tail.loading = true;
        self.tail.refresh(Some(path));
        true
    }

    /// Close the window, and put the reader back to sleep.
    ///
    /// The poller holds its last request until it is given another, so leaving
    /// the path behind would have it re-reading a file nobody is looking at
    /// for the rest of the session.
    fn close_preview(&mut self) {
        self.preview = None;
        self.tail.items = Vec::new();
        self.tail.refresh(None);
    }

    /// The keys an open window answers. Movement is `j`/`k` and
    /// `ctrl-d`/`ctrl-u`, the same as a pane, so there is one set to know.
    fn handle_preview_key(&mut self, key: KeyEvent) -> bool {
        // Measured at the last draw, so a page is a page of the window that is
        // actually on screen.
        let page = (self.preview_page / 2).max(1);
        let control = key.modifiers.contains(KeyModifiers::CONTROL);

        let Some(preview) = &mut self.preview else {
            return false;
        };

        match key.code {
            // ctrl-c still quits, from in here as much as from outside.
            KeyCode::Char('c') if control => self.should_quit = true,
            KeyCode::Char('q' | 'p') | KeyCode::Esc => self.close_preview(),
            KeyCode::Char('d') if control => preview.back = preview.back.saturating_sub(page),
            KeyCode::Char('u') if control => preview.back += page,
            KeyCode::Char('j') | KeyCode::Down => preview.back = preview.back.saturating_sub(1),
            KeyCode::Char('k') | KeyCode::Up => preview.back += 1,
            // Back to the live end, which is where a running job is writing.
            KeyCode::Char('G') => preview.back = 0,
            _ => return false,
        }

        // Stepping off the end pauses the reading, and returning to it starts
        // again — with a fetch straight away, so `G` is not a wait.
        self.tail.refresh(self.previewing());
        true
    }

    /// The pane the movement keys act on.
    fn focused(&mut self) -> &mut dyn Scroll {
        match self.focus {
            Focus::Queue => &mut self.queue,
            Focus::Recent => &mut self.recent,
        }
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Update { force }) = cli.command {
        // The installer has already said how it went, so all that is left to
        // pass on is whether it worked.
        std::process::exit(i32::from(!update::run(force)?));
    }

    ratatui::run(|terminal| App::new(!cli.no_update).run(terminal))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane of `count` rows over a body `page` rows tall. Nothing polls
    /// behind it, and the rows are empty: scrolling never looks at what a row
    /// holds, only at how many there are.
    fn pane(count: usize, page: usize) -> Pane<bool, ()> {
        let idle = Duration::from_secs(3600);

        Pane {
            feed: Feed {
                items: vec![(); count],
                error: None,
                loading: false,
                poller: Poller::spawn(idle, true, |_| Ok(Vec::new())),
            },
            selected: 0,
            offset: 0,
            column: 0,
            page,
        }
    }

    /// A list that fits has nothing to scroll.
    #[test]
    fn takes_a_short_list_whole() {
        let mut pane = pane(5, 20);

        assert_eq!(pane.window(), 0..5);
        assert_eq!(pane.offset, 0);
    }

    /// The point of the whole exercise: a month of everyone's jobs still costs
    /// one screenful to draw.
    #[test]
    fn takes_only_a_screenful_of_a_long_list() {
        let mut pane = pane(84_052, 20);

        assert_eq!(pane.window(), 0..20);
    }

    #[test]
    fn scrolls_down_to_keep_the_selection_in_view() {
        let mut pane = pane(100, 10);

        pane.selected = 9;
        assert_eq!(pane.window(), 0..10, "the last row that already fits");

        pane.selected = 10;
        assert_eq!(pane.window(), 1..11, "one row further on, so one row moves");
    }

    #[test]
    fn scrolls_back_up_to_the_selection() {
        let mut pane = pane(100, 10);

        pane.selected = 50;
        assert_eq!(pane.window(), 41..51);

        pane.selected = 20;
        assert_eq!(pane.window(), 20..30, "the selection leads at the top edge");
    }

    /// The table only ever sees a window, so it cannot clamp on our behalf the
    /// way it did when it held the whole list.
    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut pane = pane(3, 10);

        for _ in 0..10 {
            pane.move_selection(1, true);
        }
        assert_eq!(pane.selected, 2);

        for _ in 0..10 {
            pane.move_selection(1, false);
        }
        assert_eq!(pane.selected, 0);
    }

    #[test]
    fn moving_within_an_empty_list_selects_nothing() {
        let mut pane = pane(0, 10);
        pane.move_selection(1, true);

        assert_eq!(pane.selected, 0);
        assert_eq!(pane.window(), 0..0);
    }

    /// A list can shrink under a window scrolled far down it — `m` back to
    /// your own jobs does exactly that. Both the window and the selection have
    /// to come back with it, or `draw` would slice past the end.
    #[test]
    fn a_shrinking_list_pulls_the_window_back() {
        let mut pane = pane(100, 10);
        pane.selected = 99;
        assert_eq!(pane.window(), 90..100);

        pane.feed.items.truncate(15);
        let window = pane.window();

        assert_eq!(window, 5..15);
        assert!(window.contains(&pane.selected));
    }

    /// Before the first draw the body height is unmeasured, so there is no
    /// window to take either.
    #[test]
    fn takes_no_rows_before_the_first_draw() {
        let mut pane = pane(100, 0);

        assert_eq!(pane.window(), 0..0);
    }

    /// A queue row with nothing filled in but the id: cancelling only ever
    /// looks at that.
    fn job(id: &str) -> Job {
        Job {
            id: id.to_string(),
            partition: String::new(),
            name: String::new(),
            user: String::new(),
            state: String::new(),
            cpus: String::new(),
            gpus: String::new(),
            mem: String::new(),
            time: String::new(),
            nodes: String::new(),
            reason: String::new(),
            log: String::new(),
        }
    }

    /// An app holding `jobs` in the queue. Nothing polls behind it, and a
    /// cancellation it sends runs a closure rather than `scancel`.
    fn app(jobs: &[&str]) -> App {
        let idle = Duration::from_secs(3600);
        let mut queue = Pane::new(Poller::spawn(idle, true, |_| Ok(Vec::new())));
        queue.feed.items = jobs.iter().copied().map(job).collect();
        queue.feed.loading = false;

        App {
            wallet: Feed::new(Poller::spawn(idle, (), |()| Ok(Vec::new()))),
            queue,
            recent: Pane::new(Poller::spawn(idle, (), |()| Ok(Vec::new()))),
            costs: [0.0; cost::WINDOWS.len()],
            focus: Focus::Queue,
            only_me: true,
            update: update::Check::idle(),
            cancels: Cancels::running(|_| Ok(())),
            prompt: None,
            preview: None,
            preview_page: 0,
            tail: Feed::new(logtail::poll()),
            should_quit: false,
        }
    }

    /// A finished run, for a recent pane that only has to have rows in it.
    fn run() -> Run {
        Run::parse(
            "310011|dev|train-a|alice|FAILED|billing=12,cpu=12,gres/gpu=1,mem=200G,node=1|00:00:14|2026-08-28T19:56:24|1:0|2026-08-28T19:56:10|/work/alice|slurm-%j.out",
        )
        .expect("parsed")
    }

    fn press(app: &mut App, code: KeyCode) -> bool {
        app.handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    /// `l` walks the table right, `h` walks it back.
    #[test]
    fn the_columns_move_sideways() {
        let mut app = app(&["308208"]);

        assert!(press(&mut app, KeyCode::Char('l')));
        assert_eq!(app.queue.column, 1);

        assert!(press(&mut app, KeyCode::Char('h')));
        assert_eq!(app.queue.column, 0);
    }

    /// Rightwards stops with the last column still on screen, rather than
    /// scrolling into an empty pane with no sign of the way back.
    #[test]
    fn the_columns_stop_at_both_ends() {
        let mut app = app(&["308208"]);
        let columns = <Job as Rows>::COLUMNS.len();

        for _ in 0..columns * 2 {
            press(&mut app, KeyCode::Char('l'));
        }
        assert_eq!(app.queue.column, columns - 1);

        for _ in 0..columns * 2 {
            press(&mut app, KeyCode::Char('h'));
        }
        assert_eq!(app.queue.column, 0);
    }

    /// Sideways is per pane, the way `j`/`k` already are: the two have
    /// different columns, and reading one is not a reason to move the other.
    #[test]
    fn each_pane_keeps_its_own_column() {
        let mut app = app(&["308208"]);

        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Tab);

        assert_eq!(app.queue.column, 2);
        assert_eq!(app.recent.column, 0);
    }

    /// A queue row carrying a log path, which is what `p` opens.
    fn job_with_log(id: &str, log: &str) -> Job {
        let mut job = job(id);
        job.log = log.to_string();
        job
    }

    /// `p` opens a window on the selected job's log, and names the file the
    /// reader is to read.
    #[test]
    fn opens_a_window_on_the_selected_log() {
        let mut app = app(&["319141"]);
        app.queue.feed.items = vec![job_with_log("319141", "/work/alice/slurm-319141.out")];

        assert!(press(&mut app, KeyCode::Char('p')));
        assert_eq!(
            app.previewing(),
            Some("/work/alice/slurm-319141.out".to_string())
        );
    }

    /// A job launched with `srun` has no file to show, so `p` does nothing —
    /// rather than opening a window on an error.
    #[test]
    fn opens_nothing_for_a_job_with_no_log() {
        let mut app = app(&["319141"]);
        app.queue.feed.items = vec![job_with_log("319141", "-")];

        assert!(!press(&mut app, KeyCode::Char('p')));
        assert_eq!(app.previewing(), None);
    }

    /// `p` again closes it, and the reader goes back to reading nothing.
    #[test]
    fn closes_the_window_and_stops_reading() {
        let mut app = app(&["319141"]);
        app.queue.feed.items = vec![job_with_log("319141", "/work/alice/slurm-319141.out")];

        press(&mut app, KeyCode::Char('p'));
        assert!(press(&mut app, KeyCode::Char('p')));

        assert_eq!(app.previewing(), None);
        assert!(app.tail.items.is_empty());
    }

    /// An open window has the movement keys: `k` walks back up the log, and
    /// the queue underneath keeps its own selection.
    #[test]
    fn the_window_takes_the_movement_keys() {
        let mut app = app(&["319141", "319142"]);
        app.queue.feed.items = vec![
            job_with_log("319141", "/work/alice/a.out"),
            job_with_log("319142", "/work/alice/b.out"),
        ];

        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('k'));

        assert_eq!(app.preview.as_ref().expect("open").back, 2);
        assert_eq!(app.queue.selected, 0, "the pane moved under the window");

        // `j` walks back towards the end, and `G` returns to it outright.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.preview.as_ref().expect("open").back, 1);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.preview.as_ref().expect("open").back, 0);
    }

    /// Scrolling back stops the window taking new lines, so what you are
    /// reading does not slide down as the job writes more.
    #[test]
    fn scrolling_back_pauses_the_reading() {
        let mut app = app(&["319141"]);
        app.queue.feed.items = vec![job_with_log("319141", "/work/alice/a.out")];

        press(&mut app, KeyCode::Char('p'));
        assert!(app.following());
        assert_eq!(app.previewing(), Some("/work/alice/a.out".to_string()));

        press(&mut app, KeyCode::Char('k'));
        assert!(!app.following());
        // The reader is idled too, rather than reading into a channel nobody
        // is draining.
        assert_eq!(app.previewing(), None);
        assert!(!app.apply_tail(), "took lines while scrolled back");
    }

    /// `G` returns to the end, and the reading with it.
    #[test]
    fn following_again_starts_the_reading() {
        let mut app = app(&["319141"]);
        app.queue.feed.items = vec![job_with_log("319141", "/work/alice/a.out")];

        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('G'));

        assert!(app.following());
        assert_eq!(app.previewing(), Some("/work/alice/a.out".to_string()));
    }

    /// `dd` must not reach through an open window: the keys belong to it.
    #[test]
    fn the_window_swallows_a_cancellation() {
        let mut app = app(&["319141"]);
        app.queue.feed.items = vec![job_with_log("319141", "/work/alice/a.out")];

        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('d'));

        assert_eq!(armed(&app), None);
        assert_eq!(sent(&app), None);
    }

    /// The job a `d` has armed on, if it has.
    fn armed(app: &App) -> Option<&str> {
        match &app.prompt {
            Some(Prompt::Confirm(id, _)) => Some(id),
            _ => None,
        }
    }

    /// The job a cancellation has been sent for, if one has.
    fn sent(app: &App) -> Option<&str> {
        match &app.prompt {
            Some(Prompt::Sent(id)) => Some(id),
            _ => None,
        }
    }

    #[test]
    fn one_d_only_asks() {
        let mut app = app(&["308208", "311567"]);
        app.queue.selected = 1;

        assert!(press(&mut app, KeyCode::Char('d')));
        assert_eq!(armed(&app), Some("311567"), "the selected job");
    }

    #[test]
    fn a_second_d_cancels_it() {
        let mut app = app(&["308208"]);

        press(&mut app, KeyCode::Char('d'));
        assert!(press(&mut app, KeyCode::Char('d')));

        assert_eq!(sent(&app), Some("308208"));
    }

    /// The id is the one the prompt named, not whatever the cursor is over by
    /// the time it is answered: the queue refetches every second.
    #[test]
    fn cancels_the_job_it_asked_about() {
        let mut app = app(&["308208", "311567"]);

        press(&mut app, KeyCode::Char('d'));
        // A fetch lands between the two presses, with the job gone from it.
        app.queue.feed.items = vec![job("311567")];
        press(&mut app, KeyCode::Char('d'));

        assert_eq!(sent(&app), Some("308208"));
    }

    /// Any other key is an answer of no, and costs a frame because the footer
    /// has to go back to the keys.
    #[test]
    fn anything_else_answers_no() {
        let mut app = app(&["308208"]);

        press(&mut app, KeyCode::Char('d'));
        assert!(press(&mut app, KeyCode::Char('j')));

        assert!(app.prompt.is_none());
    }

    /// A `d` pressed a minute after another one is the first of its own pair,
    /// not the second of that one.
    #[test]
    fn a_late_d_asks_again() {
        let mut app = app(&["308208"]);
        app.prompt = Some(Prompt::Confirm(
            "308208".to_string(),
            Instant::now() - CONFIRM,
        ));

        press(&mut app, KeyCode::Char('d'));

        assert_eq!(armed(&app), Some("308208"));
        assert_eq!(sent(&app), None, "nothing was cancelled");
    }

    /// And the question stops being asked once it is too late to answer it,
    /// rather than sitting in the footer until the next key.
    #[test]
    fn the_question_expires() {
        let mut app = app(&["308208"]);

        press(&mut app, KeyCode::Char('d'));
        assert!(!app.expire_prompt(), "still in time");

        app.prompt = Some(Prompt::Confirm(
            "308208".to_string(),
            Instant::now() - CONFIRM,
        ));
        assert!(app.expire_prompt(), "redrawn without it");
        assert!(app.prompt.is_none());
    }

    /// A result is not a question, so it stays put until it is read.
    #[test]
    fn a_result_does_not_expire() {
        let mut app = app(&["308208"]);
        app.prompt = Some(Prompt::Done("308208".to_string()));

        assert!(!app.expire_prompt());
        assert!(app.prompt.is_some());
    }

    /// The other pane is jobs that have already ended.
    #[test]
    fn d_asks_nothing_of_the_recent_pane() {
        let mut app = app(&["308208"]);
        app.focus = Focus::Recent;

        assert!(!press(&mut app, KeyCode::Char('d')));
        assert!(app.prompt.is_none());
    }

    #[test]
    fn d_asks_nothing_of_an_empty_queue() {
        let mut app = app(&[]);

        assert!(!press(&mut app, KeyCode::Char('d')));
        assert!(app.prompt.is_none());
    }

    /// ctrl-d still pages, rather than arming the pane it just scrolled.
    #[test]
    fn ctrl_d_still_pages() {
        let mut app = app(&["308208", "311567", "312000"]);
        app.queue.page = 4;

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));

        assert!(app.prompt.is_none());
        assert_eq!(app.queue.selected, 2);
    }

    /// `m` is the queue's key alone. The recent pane is always your own jobs,
    /// so it keeps both its rows and your place in them.
    #[test]
    fn m_switches_the_queue_alone() {
        let mut app = app(&["308208", "311567"]);
        app.recent.feed.items = vec![run(), run()];
        app.recent.selected = 1;
        app.queue.selected = 1;

        assert!(press(&mut app, KeyCode::Char('m')));

        assert!(!app.only_me, "the queue widened to every user");
        assert_eq!(app.queue.selected, 0, "a replaced queue starts at the top");
        assert_eq!(app.recent.selected, 1, "the recent pane did not move");
    }
}
