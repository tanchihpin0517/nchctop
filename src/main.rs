mod poller;
mod sacct;
mod squeue;

use std::io;
use std::time::Duration;

use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::poller::Poller;
use crate::sacct::Run;
use crate::squeue::Job;

/// A top-like view of Slurm jobs on the NCHC cluster.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

/// How long to wait for input before redrawing. Also the longest a finished
/// fetch can sit in a channel before it reaches the screen.
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
        ("NAME", Constraint::Min(16)),
        ("USER", Constraint::Length(10)),
        ("STATE", Constraint::Length(9)),
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
        ("NAME", Constraint::Min(16)),
        ("USER", Constraint::Length(10)),
        ("STATE", Constraint::Length(13)),
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

/// One list: its rows, the background fetch feeding them, and how it is
/// scrolled. Both panes keep their own, so moving focus does not lose your
/// place in the other.
struct Pane<T> {
    items: Vec<T>,
    /// Last failure, shown in the pane title instead of the row count.
    error: Option<String>,
    /// Cleared once the first fetch comes back, so an empty list and a list we
    /// have not read yet do not look the same.
    loading: bool,
    /// Dropped on quit, which stops the thread behind it.
    poller: Poller<bool, Vec<T>>,
    table: TableState,
    /// Rows visible in this pane's body, measured at draw time for ctrl-d/u.
    page: usize,
}

impl<T: Rows> Pane<T> {
    fn new(poller: Poller<bool, Vec<T>>) -> Self {
        Self {
            items: Vec::new(),
            error: None,
            loading: true,
            poller,
            table: TableState::new().with_selected(0),
            page: 0,
        }
    }

    /// Take whatever the poller has finished, keeping the previous rows on
    /// screen if the command failed.
    fn apply_updates(&mut self, scope: bool) {
        // Collected up front so the loop below can borrow self mutably, and
        // filtered so a fetch still in flight when `m` was pressed is ignored.
        let updates: Vec<_> = self
            .poller
            .drain()
            .filter(|(fetched, _)| *fetched == scope)
            .collect();
        if updates.is_empty() {
            return;
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

        // The list can shrink out from under the selection.
        let selected = self.table.selected().unwrap_or(0);
        self.table
            .select(Some(selected.min(self.items.len().saturating_sub(1))));
    }

    /// Ask for a fresh fetch now, rather than at the next tick.
    fn refresh(&self, scope: bool) {
        self.poller.request(scope);
    }

    /// How the last fetch went: an error, or how many rows came back. Lives in
    /// the title because with two panes on screen the footer cannot say which
    /// command a message came from.
    fn status(&self) -> Span<'static> {
        match (&self.error, self.loading) {
            (Some(err), _) => Span::styled(err.clone(), Color::Red),
            (None, true) => Span::styled("loading", Color::DarkGray),
            (None, false) => {
                Span::styled(format!("{} {}", self.items.len(), T::NOUN), Color::DarkGray)
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, name: &str, focused: bool) {
        // Body height less the two borders and the header row.
        self.page = usize::from(area.height.saturating_sub(3));

        let title = Line::from(vec![
            Span::raw(format!(" {name} · ")),
            self.status(),
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
        let rows = self.items.iter().map(T::row);
        let table = Table::new(rows, T::COLUMNS.iter().map(|(_, width)| *width))
            .header(header)
            .row_highlight_style(highlight)
            .block(Block::bordered().border_style(border).title(title));

        frame.render_stateful_widget(table, area, &mut self.table);
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

impl<T> Scroll for Pane<T> {
    fn select_next(&mut self) {
        self.table.select_next();
    }

    fn select_previous(&mut self) {
        self.table.select_previous();
    }

    fn scroll_half(&mut self, down: bool) {
        if self.items.is_empty() {
            return;
        }

        let half = (self.page / 2).max(1);
        let current = self.table.selected().unwrap_or(0);
        let target = if down {
            (current + half).min(self.items.len() - 1)
        } else {
            current.saturating_sub(half)
        };

        self.table.select(Some(target));
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
    queue: Pane<Job>,
    recent: Pane<Run>,
    focus: Focus,
    /// Whether to ask both commands for only the current user's jobs.
    only_me: bool,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        let only_me = true;

        Self {
            queue: Pane::new(squeue::poll(only_me)),
            recent: Pane::new(sacct::poll(only_me)),
            focus: Focus::Queue,
            only_me,
            should_quit: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            self.queue.apply_updates(self.only_me);
            self.recent.apply_updates(self.only_me);
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }

        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        // An even split: the queue is usually the shorter list, but it is also
        // the one worth watching, so neither side earns the extra rows.
        let [queue, recent, footer] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let focus = self.focus;
        self.queue
            .draw(frame, queue, "queue", focus == Focus::Queue);
        self.recent
            .draw(frame, recent, "last 24h", focus == Focus::Recent);
        frame.render_widget(self.footer(), footer);
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
        self.queue.table.select(Some(0));
        self.recent.table.select(Some(0));
        self.refresh();
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Queue => Focus::Recent,
            Focus::Recent => Focus::Queue,
        };
    }

    /// Refresh both lists; they are on screen together, so they should agree.
    fn refresh(&self) {
        self.queue.refresh(self.only_me);
        self.recent.refresh(self.only_me);
    }

    fn handle_events(&mut self) -> io::Result<()> {
        if !event::poll(TICK)? {
            return Ok(());
        }

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
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
                _ => {}
            }
        }

        Ok(())
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
    let _cli = Cli::parse();

    ratatui::run(|terminal| App::new().run(terminal))
}
