use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use clap::Parser;
use ds::{
    chat_client::{ChatClient, ChatMessages, ReqMessageType},
    chat_server::ServerMessage,
};

use std::{borrow::BorrowMut, error::Error, str::FromStr, sync::Arc};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::protocol::Message as TokioMessage;
use tokio_util::sync::CancellationToken;

use de_mls::{
    cli::*,
    user::{User, UserError},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let token = CancellationToken::new();

    let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);

    let args = Args::parse();
    let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
    let user_address = signer.address();
    let (client, mut client_recv) =
        ChatClient::connect("ws://127.0.0.1:8080", &user_address.to_string()).await?;
    //// Create user
    let user_n = User::new(&args.user_priv_key, client).await?;
    let user_arc = Arc::new(Mutex::new(user_n));

    let (messages_tx, messages_rx) = mpsc::channel::<Msg>(100);
    messages_tx
        .send(Msg::Input(Message::System(format!(
            "Hello, {:}",
            user_address
        ))))
        .await?;

    let messages_tx2 = messages_tx.clone();
    let event_token = token.clone();
    let h1 = tokio::spawn(async move { event_handler(messages_tx2, cli_tx, event_token).await });

    let res_msg_tx = messages_tx.clone();
    let main_token = token.clone();
    let user = user_arc.clone();
    let h2 = tokio::spawn(async move {
        let (redis_tx, mut redis_rx) = mpsc::channel::<Vec<u8>>(100);
        loop {
            tokio::select! {
                Some(msg) = client_recv.recv() => {
                    if let TokioMessage::Text(text) = msg {
                        if let Ok(chat_message) = serde_json::from_str::<ServerMessage>(&text) {
                            if let ServerMessage::InMessage { from, to, msg } = chat_message {
                                if let Ok(chat_msg) = serde_json::from_str::<ChatMessages>(&msg) {
                                    match chat_msg {
                                        ChatMessages::Request(req) => {
                                            let res = user.as_ref().lock().await.send_responce_on_request(req, to[0].clone(), &from);
                                            if let Err(err) = res {
                                                res_msg_tx
                                                    .send(Msg::Input(Message::Error(err.to_string())))
                                                    .await?;
                                            }
                                        },
                                        ChatMessages::Response(resp) => {
                                            let res = user.as_ref().lock().await.parce_responce(resp, "test".to_string()).await;
                                            if let Err(err) = res {
                                                res_msg_tx
                                                    .send(Msg::Input(Message::Error(err.to_string())))
                                                    .await?;
                                            }
                                        },
                                        ChatMessages::Welcome(welcome) => {
                                            let res = user.as_ref().lock().await.join_group(welcome).await;
                                            match res {
                                                Ok(mut buf) => {
                                                    let msg = format!("Succesfully join to the group: {:#?}", buf.1);
                                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await?;

                                                    let redis_tx = redis_tx.clone();
                                                    tokio::spawn(async move {
                                                        while let Ok(msg) = buf.0.recv().await {
                                                            let bytes: Vec<u8> = msg.value.convert()?;
                                                            redis_tx.send(bytes).await?;
                                                        }
                                                        Ok::<_, CliError>(())
                                                    });
                                                },
                                                Err(err) => {
                                                    res_msg_tx
                                                        .send(Msg::Input(Message::Error(err.to_string())))
                                                        .await?;
                                                },
                                            };
                                        },
                                    }
                                } else {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(UserError::InvalidChatMessageError.to_string())))
                                        .await?;
                                }
                            };
                        } else {
                            res_msg_tx
                                .send(Msg::Input(Message::Error(UserError::InvalidServerMessageError.to_string())))
                                .await?;
                        }
                    }
                }
                Some(val) = redis_rx.recv() =>{
                    let res = user.as_ref().lock().await.receive_msg(val).await;
                    match res {
                        Ok(msg) => {
                            match msg {
                                Some(m) => res_msg_tx.send(Msg::Input(Message::Incoming(m.group, m.author, m.message))).await?,
                                None => continue
                            }
                        },
                        Err(err) => {
                            res_msg_tx
                                .send(Msg::Input(Message::Error(err.to_string())))
                                .await?;
                        },
                    };
                }
                Some(command) = cli_gr_rx.recv() => {
                    // res_msg_tx.send(Msg::Input(Message::System(format!("Get command: {:?}", command)))).await?;
                    match command {
                        Commands::CreateGroup { group_name, storage_address, storage_url } => {
                            let client_provider = ProviderBuilder::new()
                                .with_recommended_fillers()
                                .wallet(user.as_ref().lock().await.get_wallet())
                                .on_http(storage_url);
                            let res = user.as_ref().lock().await.connect_to_smart_contract(&storage_address, client_provider).await;
                            match res {
                                Ok(_) => {
                                    let msg = format!("Successfully connect to Smart Contract on address {:}\n", storage_address);
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await?;
                                },
                            };

                            let res = user.as_ref().lock().await.create_group(group_name.clone()).await;
                            match res {
                                Ok(mut br) => {
                                    let msg = format!("Successfully create group: {:?}", group_name.clone());
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await?;

                                    let redis_tx = redis_tx.clone();
                                    tokio::spawn(async move {
                                        while let Ok(msg) = br.recv().await {
                                            let bytes: Vec<u8> = msg.value.convert()?;
                                            redis_tx.send(bytes).await?;
                                        }
                                        Ok::<_, CliError>(())
                                    });
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await?;
                                },
                            };
                        },
                        Commands::Invite { group_name, users_wallet_addrs } => {
                            let u_c = user.clone();
                            let res_msg_tx_c = messages_tx.clone();
                            tokio::spawn(async move {
                                for user_wallet in users_wallet_addrs.iter() {
                                    let u_c_f = u_c.as_ref();
                                    let opt_token =
                                    {
                                        let mut u_c_ff = u_c_f.lock().await;
                                        if !u_c_ff.contacts.does_user_in_contacts(user_wallet).await {
                                            u_c_ff.contacts.add_new_contact(user_wallet).await.unwrap();
                                        }
                                        u_c_ff.contacts
                                            .send_req_msg_to_user(
                                                user_address.to_string(),
                                                user_wallet,
                                                "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512".to_string(),
                                                ReqMessageType::InviteToGroup,
                                            )
                                            .await.unwrap();

                                        u_c_ff.contacts.future_req.get(user_wallet).cloned()
                                    };

                                    match opt_token {
                                        Some(token) => token.cancelled().await,
                                        None => return Err(CliError::SplitLineError),
                                    };

                                    {
                                        let mut u_c = u_c.as_ref().lock().await;
                                        u_c.contacts.future_req.remove(user_wallet);
                                        u_c.add_user_to_acl(user_wallet).await.unwrap();
                                    }

                                }

                                let res = u_c.as_ref().lock().await.invite(users_wallet_addrs.clone(), group_name.clone()).await;
                                match res {
                                    Ok(_) => {
                                        let msg = format!("Invite {:?} to the group {:}\n",
                                            users_wallet_addrs, group_name
                                        );
                                        res_msg_tx_c.send(Msg::Input(Message::System(msg))).await?;
                                    },
                                    Err(err) => {
                                        res_msg_tx_c
                                            .send(Msg::Input(Message::Error(err.to_string())))
                                            .await?;
                                    },
                                };
                                Ok::<_, CliError>(())
                            });
                        },
                        Commands::SendMessage { group_name, msg } => {
                            let message = msg.join(" ");
                            let res = user.as_ref().lock().await.send_msg(&message, group_name.clone(), user_address.to_string()).await;
                            match res {
                                Ok(_) => {
                                    res_msg_tx.send(Msg::Input(Message::Mine(group_name, user_address.to_string(), message ))).await?;
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await?;
                                },
                            };
                        },
                        Commands::Exit => {
                            res_msg_tx.send(Msg::Input(Message::System("Bye!".to_string()))).await?;
                            break
                        },
                    }
                }
                _ = main_token.cancelled() => {
                    break;
                }
                else => {
                    res_msg_tx.send(Msg::Input(Message::System("Something went wrong".to_string()))).await?;
                    break
                }
            };
        }
        Ok::<_, CliError>(())
    });

    let h3 = tokio::spawn(async move { terminal_handler(messages_rx, token).await });

    let handler_res = tokio::join!(h1, h2, h3);
    handler_res.0??;
    handler_res.1??;
    handler_res.2??;

    Ok(())
}
