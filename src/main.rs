mod cost;
mod poller;
mod sacct;
mod squeue;
mod tres;
mod update;
mod wallet;

use std::io;
use std::ops::Range;
use std::time::Duration;

use chrono::Local;
use clap::{Parser, Subcommand};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::poller::Poller;
use crate::sacct::Run;
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
    Update,
}

/// How long to wait for input before looking at the pollers again. Also the
/// longest a finished fetch can sit in a channel before it reaches the screen.
const TICK: Duration = Duration::from_millis(100);

/// Something that renders as a table row.
trait Rows {
    /// Column titles and widths, in render order.
    const COLUMNS: &'static [(&'static str, Constraint)];

    /// What to call these in the pane title.
    const NOUN: &'static str;

    fn row(&self) -> Row<'_>;
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
    ];

    const NOUN: &'static str = "jobs";

    fn row(&self) -> Row<'_> {
        Row::new([
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
        ])
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
    ];

    const NOUN: &'static str = "jobs";

    fn row(&self) -> Row<'_> {
        Row::new([
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
        ])
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
struct Pane<T> {
    feed: Feed<bool, T>,
    /// Index into `feed.items`, not into what is on screen.
    selected: usize,
    /// The first of `feed.items` on screen.
    offset: usize,
    /// Rows visible in this pane's body, measured at draw time for ctrl-d/u.
    page: usize,
}

impl<T: Rows> Pane<T> {
    fn new(poller: Poller<bool, Vec<T>>) -> Self {
        Self {
            feed: Feed::new(poller),
            selected: 0,
            offset: 0,
            page: 0,
        }
    }

    /// Reports whether anything arrived, so the caller knows to redraw.
    fn apply_updates(&mut self, scope: bool) -> bool {
        if !self.feed.apply_updates(&scope) {
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

        let header = Row::new(T::COLUMNS.iter().map(|(title, _)| *title))
            .style(Style::new().add_modifier(Modifier::BOLD));
        let window = self.window();
        let rows = self.feed.items[window.clone()].iter().map(T::row);
        let table = Table::new(rows, T::COLUMNS.iter().map(|(_, width)| *width))
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
}

impl<T> Pane<T> {
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

impl<T> Scroll for Pane<T> {
    fn select_next(&mut self) {
        self.move_selection(1, true);
    }

    fn select_previous(&mut self) {
        self.move_selection(1, false);
    }

    fn scroll_half(&mut self, down: bool) {
        self.move_selection((self.page / 2).max(1), down);
    }
}

/// Which pane the movement keys act on. Both stay on screen and both keep
/// polling either way.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Queue,
    Recent,
}

struct App {
    wallet: Feed<(), Project>,
    queue: Pane<Job>,
    recent: Pane<Run>,
    /// What the recent runs cost, over each of [`cost::WINDOWS`]. Totalled when
    /// a fetch lands rather than when the line is drawn: a month of everyone's
    /// jobs is tens of thousands of rows, and holding `j` must not re-add them
    /// all for every frame.
    costs: [f64; cost::WINDOWS.len()],
    focus: Focus,
    /// Whether to ask both commands for only the current user's jobs.
    only_me: bool,
    /// The startup update check, which reports into the footer.
    update: update::Check,
    should_quit: bool,
}

impl App {
    fn new(check_update: bool) -> Self {
        let only_me = true;

        Self {
            wallet: Feed::new(wallet::poll()),
            queue: Pane::new(squeue::poll(only_me)),
            recent: Pane::new(sacct::poll(only_me)),
            costs: [0.0; cost::WINDOWS.len()],
            focus: Focus::Queue,
            only_me,
            update: if check_update {
                update::Check::start()
            } else {
                update::Check::idle()
            },
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
            dirty |= self.wallet.apply_updates(&());
            dirty |= self.queue.apply_updates(self.only_me);
            if self.recent.apply_updates(self.only_me) {
                self.costs = cost::totals(&self.recent.feed.items, Local::now().naive_local());
                dirty = true;
            }

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
        self.queue
            .draw(frame, queue, "queue", focus == Focus::Queue);
        self.recent
            .draw(frame, recent, "last 30d", focus == Focus::Recent);
        self.draw_footer(frame, footer);
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
        Line::from(format!(
            " {} · tab focus · j/k move · ^d/^u page · m {} · r refresh · q quit",
            if self.only_me { "mine" } else { "all" },
            if self.only_me { "all" } else { "mine" },
        ))
        .fg(Color::DarkGray)
    }

    /// Switch between the current user's jobs and everyone's, in both panes.
    fn toggle_scope(&mut self) {
        self.only_me = !self.only_me;
        // Both lists are about to be replaced wholesale, so start at the top
        // rather than partway down whatever arrives.
        self.queue.selected = 0;
        self.recent.selected = 0;
        self.refresh();
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
        self.recent.feed.refresh(self.only_me);
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
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Char('m') => self.toggle_scope(),
            KeyCode::Char('r') => self.refresh(),
            _ => return false,
        }

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

    if let Some(Command::Update) = cli.command {
        // The installer has already said how it went, so all that is left to
        // pass on is whether it worked.
        std::process::exit(i32::from(!update::run()?));
    }

    ratatui::run(|terminal| App::new(!cli.no_update).run(terminal))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane of `count` rows over a body `page` rows tall. Nothing polls
    /// behind it, and the rows are empty: scrolling never looks at what a row
    /// holds, only at how many there are.
    fn pane(count: usize, page: usize) -> Pane<()> {
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
}
