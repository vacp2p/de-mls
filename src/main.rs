use alloy::{
    hex::{self, ToHexExt},
    primitives::Address,
    providers::ProviderBuilder,
};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use openmls::{framing::MlsMessageIn, prelude::TlsSerializeTrait};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{io, str::FromStr, sync::Arc};
use tls_codec::Deserialize;
use tokio::sync::{mpsc, Mutex};

use de_mls::{cli::*, user::User};

#[tokio::main]
async fn main() -> Result<(), CliError> {
    let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);

    let args = Args::parse();
    let (user_address, wallet, storage_address) = get_user_data(&args)?;

    let client_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(args.storage_url);

    //// Create user
    println!("Start register user");
    let mut user = User::new(
        user_address.as_slice(),
        client_provider.clone(),
        storage_address,
    )
    .await?;
    user.register().await?;

    enum Msg {
        Input(String),
        Refresh(String),
        Exit,
    }
    let (messages_tx, mut messages_rx) = mpsc::channel::<Msg>(100);
    messages_tx.send(Msg::Refresh("".to_string())).await;

    let messages_tx2 = messages_tx.clone();
    let h1 = tokio::spawn(async move {
        let mut input = String::new();
        loop {
            if let Event::Key(key) = tokio::task::spawn_blocking(event::read)
                .await
                .unwrap()
                .unwrap()
            {
                match key.code {
                    KeyCode::Char(c) => {
                        input.push(c);
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Enter => {
                        let line: String = input.drain(..).collect();
                        let args = shlex::split(&line).ok_or(CliError::SplitLineError).unwrap();
                        let cli = Cli::try_parse_from(args).unwrap();

                        cli_tx.send(cli.command).await.unwrap();
                        messages_tx2.send(Msg::Input(line)).await.unwrap();
                    }
                    KeyCode::Esc => {
                        messages_tx2.send(Msg::Exit).await.unwrap();
                        break;
                    }
                    _ => {}
                }
                messages_tx2
                    .send(Msg::Refresh(input.clone()))
                    .await
                    .unwrap();
            }
        }
    });

    let res_msg_tx = messages_tx.clone();
    tokio::spawn(async move {
        let (redis_tx, mut redis_rx) = mpsc::channel::<Vec<u8>>(100);
        loop {
            tokio::select! {
                Some(val) = redis_rx.recv() =>{
                    let res = MlsMessageIn::tls_deserialize_bytes(val).unwrap();
                    let msg = user.receive_msg(res).await.unwrap();
                    match msg {
                        Some(m) =>  res_msg_tx.send(Msg::Input(m.to_string())).await.unwrap(),
                        None => break
                    }
                }
                Some(command) = cli_gr_rx.recv() => {
                    res_msg_tx.send(Msg::Input(format!("Get command: {:?}", command))).await.unwrap();
                    match command {
                        Commands::CreateGroup { group_name } => {
                            let mut br = user.create_group(group_name.clone()).await.unwrap();
                            let redis_tx = redis_tx.clone();
                            tokio::spawn(async move {
                                while let Ok(msg) = br.recv().await {
                                    let bytes: Vec<u8> = msg.value.convert().unwrap();
                                    redis_tx.send(bytes).await.unwrap();
                                }
                            });
                        },
                        Commands::Invite { group_name, user_wallet } => {
                            let user_address = Address::from_str(&user_wallet).unwrap();
                            let welcome: MlsMessageIn =
                                user.invite(user_address.as_slice(), group_name).await.unwrap();
                            let bytes = welcome.tls_serialize_detached().unwrap();
                            let string = bytes.encode_hex();
                            res_msg_tx.send(Msg::Input(string)).await.unwrap();
                        },
                        Commands::JoinGroup { welcome } => {
                            let wbytes = hex::decode(welcome).unwrap();
                            let welc = MlsMessageIn::tls_deserialize_bytes(wbytes).unwrap();
                            let welcome = welc.into_welcome();
                            if welcome.is_some() {
                                let mut br = user.join_group(welcome.unwrap()).await.unwrap();
                                let redis_tx = redis_tx.clone();
                                tokio::spawn(async move {
                                    while let Ok(msg) = br.recv().await {
                                        let bytes: Vec<u8> = msg.value.convert().unwrap();
                                        redis_tx.send(bytes).await.unwrap();
                                    }
                                });
                            }
                        },
                        Commands::SendMessage { group_name, msg } => {
                            user.send_msg(&msg, group_name).await.unwrap();
                        },
                        Commands::Exit => {
                            break
                        },
                    }
                }
                else => {
                    println!("Both channels closed");
                    break
                }
            }
        }
    });

    let h3 = tokio::spawn(async move {
        // Setup terminal
        enable_raw_mode().unwrap();
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).unwrap();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Arc::new(Mutex::new(Terminal::new(backend).unwrap()));

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
            .await
            .unwrap();
        }

        // Restore terminal
        disable_raw_mode().unwrap();
        execute!(terminal.blocking_lock().backend_mut(), LeaveAlternateScreen,).unwrap();
        terminal.blocking_lock().show_cursor().unwrap();
    });

    tokio::join!(h1, h3);

    Ok(())
}

fn ui(f: &mut Frame, messages: &[String], input: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)].as_ref())
        .split(f.size());

    let w = chunks[0].width;
    let message_items: Vec<ListItem> = messages
        .iter()
        .flat_map(|msg| textwrap::wrap(msg, w as usize))
        .map(|m| ListItem::new(Line::from(Span::raw(m))))
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
        .wrap(Wrap { trim: true });

    f.render_widget(messages_history, chunks[0]);
    f.render_widget(input_line, chunks[1]);
}
