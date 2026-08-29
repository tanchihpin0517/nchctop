use std::io;
use std::time::Duration;

use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};

/// A top-like view of Slurm jobs on the NCHC cluster.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

/// How long to wait for input before redrawing.
const TICK: Duration = Duration::from_millis(250);

struct App {
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self { should_quit: false }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        let block = Block::bordered().title(" nchctop ");
        let body = Paragraph::new("No jobs yet. Press q to quit.").block(block);
        frame.render_widget(body, frame.area());
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
                _ => {}
            }
        }

        Ok(())
    }
}

fn main() -> io::Result<()> {
    let _cli = Cli::parse();

    ratatui::run(|terminal| App::new().run(terminal))
}
