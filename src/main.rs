mod squeue;

use std::io;
use std::time::{Duration, Instant};

use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::squeue::Job;

/// A top-like view of Slurm jobs on the NCHC cluster.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

/// How long to wait for input before redrawing.
const TICK: Duration = Duration::from_millis(250);

/// How often to re-run squeue.
const REFRESH: Duration = Duration::from_secs(2);

const COLUMNS: [(&str, Constraint); 8] = [
    ("JOBID", Constraint::Length(8)),
    ("PARTITION", Constraint::Length(9)),
    ("NAME", Constraint::Min(16)),
    ("USER", Constraint::Length(10)),
    ("STATE", Constraint::Length(9)),
    ("TIME", Constraint::Length(10)),
    ("NODES", Constraint::Length(5)),
    ("NODELIST(REASON)", Constraint::Min(18)),
];

struct App {
    jobs: Vec<Job>,
    /// Last squeue failure, shown in the footer instead of the job count.
    error: Option<String>,
    /// Whether to pass `--me` to squeue, toggled with `m`.
    only_me: bool,
    table: TableState,
    /// Rows visible in the table body, measured at draw time for ctrl-d/ctrl-u.
    page: usize,
    last_refresh: Instant,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            jobs: Vec::new(),
            error: None,
            only_me: true,
            table: TableState::new().with_selected(0),
            page: 0,
            last_refresh: Instant::now(),
            should_quit: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        self.refresh();

        while !self.should_quit {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;

            if self.last_refresh.elapsed() >= REFRESH {
                self.refresh();
            }
        }

        Ok(())
    }

    /// Re-run squeue, keeping the previous rows on screen if it fails.
    fn refresh(&mut self) {
        match squeue::fetch(self.only_me) {
            Ok(jobs) => {
                self.jobs = jobs;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }

        // The list can shrink out from under the selection.
        let selected = self.table.selected().unwrap_or(0);
        self.table
            .select(Some(selected.min(self.jobs.len().saturating_sub(1))));
        self.last_refresh = Instant::now();
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
            .areas(frame.area());

        // Body height less the two borders and the header row.
        self.page = usize::from(body.height.saturating_sub(3));

        let header = Row::new(COLUMNS.map(|(title, _)| title))
            .style(Style::new().add_modifier(Modifier::BOLD));
        let rows = self.jobs.iter().map(job_row);
        let table = Table::new(rows, COLUMNS.map(|(_, width)| width))
            .header(header)
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .block(Block::bordered().title(" nchctop "));

        frame.render_stateful_widget(table, body, &mut self.table);
        frame.render_widget(self.footer(), footer);
    }

    fn footer(&self) -> Line<'_> {
        match &self.error {
            Some(err) => Line::from(format!(" {err}")).fg(Color::Red),
            None => Line::from(format!(
                " {} jobs · {} · j/k move · ^d/^u page · m {} · r refresh · q quit",
                self.jobs.len(),
                if self.only_me { "mine" } else { "all" },
                if self.only_me { "all" } else { "mine" },
            ))
            .fg(Color::DarkGray),
        }
    }

    /// Switch between the current user's jobs and the whole queue.
    fn toggle_scope(&mut self) {
        self.only_me = !self.only_me;
        self.table.select(Some(0));
        self.refresh();
    }

    /// Move the selection half a screen, the way vim's ctrl-d / ctrl-u do.
    fn scroll_half(&mut self, down: bool) {
        if self.jobs.is_empty() {
            return;
        }

        let half = (self.page / 2).max(1);
        let current = self.table.selected().unwrap_or(0);
        let target = if down {
            (current + half).min(self.jobs.len() - 1)
        } else {
            current.saturating_sub(half)
        };

        self.table.select(Some(target));
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
                    self.scroll_half(true)
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.scroll_half(false)
                }
                KeyCode::Char('j') | KeyCode::Down => self.table.select_next(),
                KeyCode::Char('k') | KeyCode::Up => self.table.select_previous(),
                KeyCode::Char('m') => self.toggle_scope(),
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            }
        }

        Ok(())
    }
}

fn job_row(job: &Job) -> Row<'_> {
    let state = match job.state.as_str() {
        "RUNNING" => Style::new().fg(Color::Green),
        "PENDING" => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    };

    Row::new([
        Line::raw(&job.id),
        Line::raw(&job.partition),
        Line::raw(&job.name),
        Line::raw(&job.user),
        Line::styled(&job.state, state),
        Line::raw(&job.time),
        Line::raw(&job.nodes),
        Line::raw(&job.reason),
    ])
}

fn main() -> io::Result<()> {
    let _cli = Cli::parse();

    ratatui::run(|terminal| App::new().run(terminal))
}
