//! `bench explorer`: an interactive TUI to explore a completed sweep.

use std::path::{Path, PathBuf};

use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::error::Error;
use crate::redaction::Redactor;
use crate::redaction::surface;
use crate::trajectory::Trajectory;

#[derive(Debug, Args)]
pub struct ExplorerCmd {
    /// Path to a completed sweep directory containing `*.traj.json` files.
    #[arg(long, value_name = "DIR")]
    pub sweep: PathBuf,
}

#[derive(Default)]
struct App {
    instances: Vec<InstanceEntry>,
    list_state: ListState,
    should_quit: bool,
    selected_trajectory: Option<Trajectory>,
    error_msg: Option<String>,
}

struct InstanceEntry {
    id: String,
    path: PathBuf,
    outcome: String,
}

impl App {
    fn new(sweep_dir: &Path) -> Self {
        let mut app = Self::default();
        let mut entries = Vec::new();

        if sweep_dir.exists() && sweep_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(sweep_dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        if name.ends_with(".traj.json") {
                            let id = name.trim_end_matches(".traj.json").to_string();
                            // Try to read just outcome to show in list.
                            let outcome = match std::fs::read_to_string(&path) {
                                Ok(content) => match serde_json::from_str::<Trajectory>(&content) {
                                    Ok(t) => {
                                        t.info.outcome.unwrap_or_else(|| "unknown".to_string())
                                    }
                                    Err(_) => "parse error".to_string(),
                                },
                                Err(_) => "read error".to_string(),
                            };
                            entries.push(InstanceEntry { id, path, outcome });
                        }
                    }
                }
            }
        }

        entries.sort_by(|a, b| a.id.cmp(&b.id));
        app.instances = entries;

        if app.instances.is_empty() {
            app.error_msg = Some("No trajectories found.".to_string());
        } else {
            app.list_state.select(Some(0));
            app.load_selected();
        }

        app
    }

    fn next(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= self.instances.len().saturating_sub(1) {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.load_selected();
    }

    fn previous(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.instances.len().saturating_sub(1)
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.load_selected();
    }

    fn load_selected(&mut self) {
        if let Some(idx) = self.list_state.selected() {
            if let Some(entry) = self.instances.get(idx) {
                match std::fs::read_to_string(&entry.path) {
                    Ok(content) => match serde_json::from_str::<Trajectory>(&content) {
                        Ok(t) => {
                            self.selected_trajectory = Some(t);
                            self.error_msg = None;
                        }
                        Err(e) => {
                            self.selected_trajectory = None;
                            self.error_msg = Some(format!("Parse error: {e}"));
                        }
                    },
                    Err(e) => {
                        self.selected_trajectory = None;
                        self.error_msg = Some(format!("Read error: {e}"));
                    }
                }
            }
        }
    }
}

pub fn run(args: &ExplorerCmd) -> Result<(), Error> {
    if !args.sweep.exists() || !args.sweep.is_dir() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "explorer: --sweep `{}` does not exist or is not a directory",
            args.sweep.display()
        ))));
    }

    // Setup terminal
    enable_raw_mode().map_err(Error::Io)?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(Error::Io)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(Error::Io)?;

    // App state
    let mut app = App::new(&args.sweep);

    let res = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode().map_err(Error::Io)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(Error::Io)?;
    terminal.show_cursor().map_err(Error::Io)?;

    if let Err(err) = res {
        eprintln!("{err:?}");
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> std::io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    _ => {}
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref())
        .split(f.area());

    // Left pane: list of trajectories
    let items: Vec<ListItem> = app
        .instances
        .iter()
        .map(|i| {
            let color = match i.outcome.as_str() {
                "submitted" => Color::Green,
                "error" => Color::Red,
                _ => Color::Yellow,
            };
            let content = Line::from(vec![
                Span::styled(i.id.clone(), Style::default().fg(Color::White)),
                Span::raw(" ("),
                Span::styled(i.outcome.clone(), Style::default().fg(color)),
                Span::raw(")"),
            ]);
            ListItem::new(content)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(" Instances ").borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    f.render_stateful_widget(list, chunks[0], &mut app.list_state);

    // Right pane: details
    let detail_block = Block::default().title(" Details ").borders(Borders::ALL);
    let detail_area = detail_block.inner(chunks[1]);
    f.render_widget(detail_block, chunks[1]);

    if let Some(err) = &app.error_msg {
        let p = Paragraph::new(err.clone())
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: true });
        f.render_widget(p, detail_area);
        return;
    }

    if let Some(t) = &app.selected_trajectory {
        let redactor = Redactor::default_enabled();

        let mut text = String::new();
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!(
                "Outcome: {}\n",
                t.info.outcome.as_deref().unwrap_or("unknown")
            ),
        );
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!(
                "Model: {}\n",
                t.info.model_name.as_deref().unwrap_or("unknown")
            ),
        );

        if let Some(task) = &t.info.task {
            let _ = std::fmt::Write::write_fmt(
                &mut text,
                format_args!(
                    "Task:\n{}\n",
                    redactor.redact_text(task, surface::EXPORT).text
                ),
            );
        }

        text.push_str("\n--- Messages ---\n\n");
        for msg in t.messages.iter().take(20) {
            let content = redactor.redact_text(&msg.content, surface::EXPORT).text;
            let role = &msg.role;
            let _ = std::fmt::Write::write_fmt(&mut text, format_args!("[{role}]\n{content}\n\n"));
        }

        if t.messages.len() > 20 {
            text.push_str("... (truncated to first 20 messages) ...\n");
        }

        let p = Paragraph::new(text).wrap(Wrap { trim: true });
        f.render_widget(p, detail_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explorer_invalid_sweep() {
        let args = ExplorerCmd { sweep: PathBuf::from("does_not_exist") };
        let res = run(&args);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("does not exist or is not a directory"));
    }

    #[test]
    fn test_explorer_app_init() {
        let temp = tempfile::tempdir().unwrap();
        let app = App::new(temp.path());
        assert!(app.instances.is_empty());
        assert_eq!(app.error_msg, Some("No trajectories found.".to_string()));
    }
}
