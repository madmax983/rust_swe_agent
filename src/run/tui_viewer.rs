use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::cli::args::TuiCmd;
use crate::error::Error;
use crate::redaction::{Redactor, surface};
use crate::trajectory::Trajectory;

#[allow(clippy::too_many_lines)]
pub fn bench_tui(cmd: &TuiCmd) -> Result<(), Error> {
    let text = std::fs::read_to_string(&cmd.trajectory).map_err(Error::Io)?;
    let traj: Trajectory = serde_json::from_str(&text)
        .map_err(|e| Error::Trajectory(format!("failed to parse trajectory: {e}")))?;

    enable_raw_mode().map_err(Error::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(Error::Io)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(Error::Io)?;

    let res = run_app(&mut terminal, &traj);

    disable_raw_mode().map_err(Error::Io)?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(Error::Io)?;
    terminal.show_cursor().map_err(Error::Io)?;

    res
}

#[allow(clippy::too_many_lines)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    traj: &Trajectory,
) -> Result<(), Error> {
    let mut list_state = ListState::default();
    list_state.select(Some(0));
    let mut scroll_offset: u16 = 0;
    let redactor = Redactor::default_enabled();

    loop {
        terminal
            .draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref())
                    .split(f.area());

                let mut items = Vec::new();
                for (i, msg) in traj.messages.iter().enumerate() {
                    let role = msg.role.as_str();
                    let prefix = match role {
                        "user" => "👤",
                        "assistant" => "🤖",
                        "tool" => "🛠️",
                        "system" => "⚙️",
                        _ => "❓",
                    };
                    let content_redacted = redactor.redact_text(&msg.content, surface::EXPORT).text;
                    let content_preview = content_redacted
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(20)
                        .collect::<String>();
                    items.push(ListItem::new(format!("{i} {prefix} - {content_preview}")));
                }

                let list = List::new(items)
                    .block(
                        Block::default()
                            .title(format!(
                                "Messages (Task: {})",
                                traj.info.task.as_deref().unwrap_or("None")
                            ))
                            .borders(Borders::ALL),
                    )
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                    .highlight_symbol(">> ");

                f.render_stateful_widget(list, chunks[0], &mut list_state);

                let selected = list_state.selected().unwrap_or(0);
                if let Some(msg) = traj.messages.get(selected) {
                    let content_redacted = redactor.redact_text(&msg.content, surface::EXPORT).text;
                    let text = format!("Role: {}\n\n{}", msg.role.as_str(), content_redacted);
                    let paragraph = Paragraph::new(text)
                        .block(Block::default().title("Content").borders(Borders::ALL))
                        .wrap(Wrap { trim: false })
                        .scroll((scroll_offset, 0));
                    f.render_widget(paragraph, chunks[1]);
                }
            })
            .map_err(Error::Io)?;

        if event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
            if let Event::Key(key) = event::read().map_err(Error::Io)? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => {
                            let i = match list_state.selected() {
                                Some(i) => {
                                    if i >= traj.messages.len().saturating_sub(1) {
                                        i
                                    } else {
                                        i + 1
                                    }
                                }
                                None => 0,
                            };
                            list_state.select(Some(i));
                            scroll_offset = 0;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let i = match list_state.selected() {
                                Some(i) => {
                                    if i == 0 {
                                        0
                                    } else {
                                        i - 1
                                    }
                                }
                                None => 0,
                            };
                            list_state.select(Some(i));
                            scroll_offset = 0;
                        }
                        KeyCode::PageDown => {
                            scroll_offset = scroll_offset.saturating_add(10);
                        }
                        KeyCode::PageUp => {
                            scroll_offset = scroll_offset.saturating_sub(10);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_cmd_can_be_instantiated() {
        let cmd = TuiCmd {
            trajectory: std::path::PathBuf::from("test.traj.json"),
        };
        assert_eq!(cmd.trajectory.to_str().unwrap(), "test.traj.json");
    }
}
