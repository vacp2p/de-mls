use clap::{arg, command, Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    io::{stdout, Read, Write},
    sync::Arc,
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::CliError;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// User private key that correspond to Ethereum wallet
    #[arg(short = 'K', long)]
    pub user_priv_key: String,

    // /// Storage contract address
    // #[arg(short = 'S', long)]
    // pub storage_addr: String,
}

pub enum Msg {
    Input(Message),
    Refresh(String),
    Exit,
}

#[derive(Clone)]
pub enum Message {
    Incoming(String, String, String),
    Mine(String, String, String),
    System(String),
    Error(String),
}

#[derive(Debug, Parser)]
#[command(multicall = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    CreateGroup {
        group_name: String,
    },
    SendMessage {
        group_name: String,
        msg: Vec<String>,
    },
    Subscribe {
        group_name: String,
    },
    // RemoveUser { user_wallet: String },
    Exit,
}

pub fn readline() -> Result<String, CliError> {
    write!(std::io::stdout(), "$ ")?;
    std::io::stdout().flush()?;
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer)
}

pub async fn event_handler(
    messages_tx: Sender<Msg>,
    cli_tx: Sender<Commands>,
    token: CancellationToken,
) -> Result<(), CliError> {
    let mut input = String::new();
    loop {
        if let Event::Key(key) = tokio::task::spawn_blocking(event::read).await?? {
            match key.code {
                KeyCode::Char(c) => {
                    input.push(c);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Enter => {
                    let line: String = std::mem::take(&mut input);
                    let args = shlex::split(&line).ok_or(CliError::SplitLineError)?;
                    let cli = Cli::try_parse_from(args);
                    if cli.is_err() {
                        messages_tx
                            .send(Msg::Input(Message::System("Unknown command".to_string())))
                            .await
                            .map_err(|err| CliError::SenderError(err.to_string()))?;
                        continue;
                    }
                    cli_tx
                        .send(cli.unwrap().command)
                        .await
                        .map_err(|err| CliError::SenderError(err.to_string()))?;
                }
                KeyCode::Esc => {
                    messages_tx
                        .send(Msg::Exit)
                        .await
                        .map_err(|err| CliError::SenderError(err.to_string()))?;
                    token.cancel();
                    break;
                }
                _ => {}
            }
            messages_tx
                .send(Msg::Refresh(input.clone()))
                .await
                .map_err(|err| CliError::SenderError(err.to_string()))?;
        }
    }
    Ok::<_, CliError>(())
}

pub fn ui(f: &mut Frame, messages: &[Message], input: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)].as_ref())
        .split(f.size());

    let message_items: Vec<ListItem> = messages
        .iter()
        .map(|message| {
            let (content, style) = match message {
                Message::Incoming(group, from, msg) => (
                    format!("[0x{}]@{}: {}", from, group, msg),
                    Style::default().fg(Color::LightGreen),
                ),
                Message::Mine(group, from, msg) => (
                    format!("[0x{}]@{}: {}", from, group, msg),
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                ),
                Message::System(msg) => (format!("[System]: {}", msg), Style::default()),
                Message::Error(msg) => (msg.clone(), Style::default().fg(Color::LightRed)),
            };
            ListItem::new(content).style(style)
        })
        .collect();

    let messages_history = List::new(message_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("messages history"),
    );

    let input_line = Paragraph::new(Text::raw(input))
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL).title("input line"))
        .wrap(Wrap { trim: false });

    f.render_widget(messages_history, chunks[0]);
    f.render_widget(input_line, chunks[1]);
}

pub async fn terminal_handler(
    mut messages_rx: Receiver<Msg>,
    token: CancellationToken,
) -> Result<(), CliError> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Arc::new(Mutex::new(Terminal::new(backend)?));

    let messages = Arc::new(Mutex::new(vec![]));
    let input = Arc::new(Mutex::new(String::new()));

    let messages_clone = Arc::clone(&messages);
    let input_clone = Arc::clone(&input);
    while let Some(msg) = messages_rx.recv().await {
        match msg {
            Msg::Input(m) => {
                let mut messages = messages_clone.lock().await;
                messages.push(m);
                if messages.len() == 100 {
                    messages.remove(0);
                }
            }
            Msg::Refresh(i) => {
                let mut input = input_clone.lock().await;
                *input = i;
            }
            Msg::Exit => {
                token.cancel();
                break;
            }
        };

        let messages = Arc::clone(&messages_clone);
        let input = Arc::clone(&input_clone);
        let terminal = Arc::clone(&terminal);
        tokio::task::spawn_blocking(move || {
            let messages = messages.blocking_lock();
            let input = input.blocking_lock();
            terminal
                .blocking_lock()
                .draw(|f| ui(f, &messages, &input))
                .unwrap();
        })
        .await?;
    }

    // Restore terminal
    disable_raw_mode()?;
    let mut terminal_lock = terminal.lock().await;
    execute!(terminal_lock.backend_mut(), LeaveAlternateScreen)?;
    terminal_lock.show_cursor()?;
    Ok(())
}
