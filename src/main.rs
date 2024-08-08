use alloy::{network::NetworkWallet, providers::ProviderBuilder, signers::local::PrivateKeySigner};
use clap::Parser;
use ds::{
    chat_client::{ChatClient, ChatMessages, ReqMessageType},
    chat_server::{start_server, ServerMessage},
};
use openmls::framing::MlsMessageIn;
use std::{any::Any, error::Error, fs::File, io::Read, str::FromStr};
use tls_codec::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message as TokioMessage;
use tokio_util::sync::CancellationToken;

use de_mls::{
    cli::*,
    user::{User, UserError},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // console_subscriber::init();
    let token = CancellationToken::new();

    let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);

    let args = Args::parse();
    let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
    let user_address = signer.address();
    let (client, mut client_recv) =
        ChatClient::connect("ws://127.0.0.1:8080", &user_address.to_string()).await?;
    //// Create user
    let mut user = User::new(&args.user_priv_key, client).await?;

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
    let h2 = tokio::spawn(async move {
        let (redis_tx, mut redis_rx) = mpsc::channel::<Vec<u8>>(100);
        loop {
            tokio::select! {
                Some(msg) = client_recv.recv() => {
                    if let TokioMessage::Text(text) = msg {
                        if let Ok(chat_message) = serde_json::from_str::<ServerMessage>(&text) {
                            let m = format!("MSG:\n{:?}", chat_message);
                            res_msg_tx.send(Msg::Input(Message::System(m))).await?;
                            match chat_message {
                                ServerMessage::SystemJoin{username}=>{
                                    let msg = format!("Client1 received SystemJoin message for user: {}",username);
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                }
                                ServerMessage::InMessage { from, to, msg } => {
                                    let m = format!("S::InMessage:\n{:?}", msg);
                                    res_msg_tx.send(Msg::Input(Message::System(m))).await?;
                                    if let Ok(chat_msg) = serde_json::from_str::<ChatMessages>(&msg) {
                                        let m = format!("ChatMessage: {:?}", chat_msg);
                                        res_msg_tx.send(Msg::Input(Message::System(m))).await?;
                                        match chat_msg {
                                            ChatMessages::Request(req) => {
                                                let res = user.send_responce_on_request(req, &from);
                                                match res {
                                                    Ok(_) => {
                                                        let msg = format!("Succesfully create responce");
                                                        res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                                    },
                                                    Err(err) => {
                                                        res_msg_tx
                                                            .send(Msg::Input(Message::Error(err.to_string())))
                                                            .await?;
                                                    }
                                                }
                                            },
                                            ChatMessages::Response(resp) => {
                                                let res = user.parce_responce(resp, "test".to_string()).await;
                                                match res {
                                                    Ok(_) => {
                                                        let msg = format!("Succesfully parse responce");
                                                        res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                                    },
                                                    Err(err) => {
                                                        res_msg_tx
                                                            .send(Msg::Input(Message::Error(err.to_string())))
                                                            .await?;
                                                    }
                                                }
                                                let res = user.contacts.chat_client.handle_response().await;
                                                match res {
                                                    Ok(_) => {
                                                        let msg = format!("Succesfully handle responce");
                                                        res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                                    },
                                                    Err(err) => {
                                                        res_msg_tx
                                                            .send(Msg::Input(Message::Error(err.to_string())))
                                                            .await?;
                                                    }
                                                }
                                            },
                                            ChatMessages::Welcome(welcome) => {
                                                let wbytes = hex::decode(welcome).unwrap();
                                                let welc = MlsMessageIn::tls_deserialize_bytes(wbytes).unwrap();
                                                let welcome = welc.into_welcome();
                                                if welcome.is_some() {
                                                    let res = user.join_group(welcome.unwrap()).await;
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
                                                } else {
                                                    res_msg_tx
                                                        .send(Msg::Input(Message::Error(UserError::EmptyWelcomeMessageError.to_string())))
                                                        .await?;
                                                }
                                            },
                                        }
                                    } else {
                                        res_msg_tx
                                            .send(Msg::Input(Message::Error(UserError::InvalidChatMessageError.to_string())))
                                            .await?;
                                    }
                                },
                            }
                        } else {
                            res_msg_tx
                                .send(Msg::Input(Message::Error(UserError::InvalidServerMessageError.to_string())))
                                .await?;
                        }
                    }
                }
                Some(val) = redis_rx.recv() =>{
                    let res = user.receive_msg(val).await;
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
                                .wallet(user.get_wallet())
                                .on_http(storage_url);
                            let res = user.connect_to_smart_contract(&storage_address, client_provider).await;
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

                            let res = user.create_group(group_name.clone()).await;
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
                            let res = user.contacts.send_req_msg_to_user(
                                user.identity.to_string(),
                                &users_wallet_addrs[0],
                                "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512".to_string(),
                                ReqMessageType::InviteToGroup,
                            ).await;
                            match res {
                                Ok(_) => {
                                    let msg = format!("Send request {:?} to the group {:}\n",
                                        users_wallet_addrs, group_name
                                    );
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await?;
                                },
                            };

                            let _ = user.contacts.chat_client.request_in_progress.lock().await;

                            let res = user.add_user_to_acl(&users_wallet_addrs[0]).await;
                            match res {
                                Ok(_) => {
                                    let msg = format!("Add user to acl {} for group {}\n",
                                    &users_wallet_addrs[0], group_name
                                );
                                res_msg_tx.send(Msg::Input(Message::System(msg))).await?;},
                                Err(err) => {res_msg_tx
                                    .send(Msg::Input(Message::Error(err.to_string())))
                                    .await?;},
                            }

                            // for user_wallet in users_wallet_addrs.iter() {
                            //     if user.contacts.does_user_in_contacts(user_wallet) {
                            //         user.add_user_to_acl(user_wallet).await?;
                            //         user.contacts.send_req_msg_to_user(
                            //             user.identity.to_string(),
                            //             user_wallet,
                            //             user.sc_ks.as_mut().unwrap().get_sc_adsress(),
                            //             ReqMessageType::InviteToGroup,
                            //         )?;
                            //     } else {
                            //         user.contacts.send_req_msg_to_user(
                            //             user.identity.to_string(),
                            //             user_wallet,
                            //             user.sc_ks.as_mut().unwrap().get_sc_adsress(),
                            //             ReqMessageType::InviteToGroup,
                            //         )?;
                            //         user.add_user_to_acl(user_wallet).await?;
                            //     }
                            // }

                            let res = user.invite(users_wallet_addrs.clone(), group_name.clone()).await;
                            match res {
                                Ok(_) => {
                                    let msg = format!("Invite {:?} to the group {:}\n",
                                        users_wallet_addrs, group_name
                                    );
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await?;
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await?;
                                },
                            };
                        },
                        Commands::SendMessage { group_name, msg } => {
                            let message = msg.join(" ");
                            let res = user.send_msg(&message, group_name.clone(), user.identity.to_string()).await;
                            match res {
                                Ok(_) => {
                                    res_msg_tx.send(Msg::Input(Message::Mine(group_name, user.identity.to_string(), message ))).await?;
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
