use alloy::{primitives::Address, providers::ProviderBuilder, signers::local::PrivateKeySigner};
use chrono::Utc;
use clap::Parser;
use sc_key_store::{sc_ks::ScKeyStorage, SCKeyStoreService};
use std::{clone, error::Error, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use waku_bindings::WakuMessage;

use de_mls::{
    cli::*,
    user::{AdminTrait, User},
    CliError,
};
use ds::ds_waku::setup_node_handle;

// #[tokio::main]
// async fn main() -> Result<(), Box<dyn Error>> {
//     let args = Args::parse();
//
//     let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
//     let nodes_name: Vec<String> = vec![
//         "/ip4/143.110.189.47/tcp/60000/p2p/16Uiu2HAm2ksj1pyRwURu9cJkyAkkgiD7CQrCXPUoTEoez1PU3mQ1"
//             .to_string(),
//     ];
//     let user_address = signer.address().to_string();
//     let group_name: String = "new_group".to_string();
//     let node = setup_node_handle(nodes_name).unwrap();
//
//     // Create user
//     let user = User::new(&args.user_priv_key, node).await?;
//     let user_arc = Arc::new(Mutex::new(user));
//
//     let receiver = user_arc
//         .as_ref()
//         .lock()
//         .await
//         .create_group(group_name.clone())?;
//
//     let user_recv_clone = user_arc.clone();
//     let h1 = tokio::spawn(async move {
//         while let Ok(msg) = receiver.recv() {
//             let mut user = user_recv_clone.as_ref().lock().await;
//             let res = user.process_waku_msg(msg).await;
//             match res {
//                 Ok(_) => println!("Successfully process message from receiver"),
//                 Err(e) => println!("Failed to process message: {:?}", e),
//             }
//         }
//     });
//
//     let user_clone = user_arc.clone();
//     let group_name_clone = group_name.clone();
//     let h2 = tokio::spawn(async move {
//         let mut interval = tokio::time::interval(Duration::from_secs(15));
//         loop {
//             interval.tick().await;
//             let mut user = user_clone.as_ref().lock().await;
//             let group = user.groups.get_mut(&group_name_clone.clone()).unwrap();
//             let res = group.admin.as_mut().unwrap().generate_new_key_pair();
//             match res {
//                 Ok(_) => println!("Successfully generate key pair"),
//                 Err(e) => println!("Failed to generate key pair: {:?}", e),
//             }
//             let admin_msg = group.admin.as_mut().unwrap().generate_admin_message();
//             let res = user
//                 .send_admin_msg(admin_msg, group_name_clone.clone())
//                 .await;
//             match res {
//                 Ok(msg_id) => println!("Successfully publish message with id: {}", msg_id),
//                 Err(e) => println!("Failed to publish message: {:?}", e),
//             }
//         }
//     });
//     let handler_res = tokio::join!(h1, h2);
//     handler_res.0.unwrap();
//     handler_res.1.unwrap();
//
//     Ok(())
// }




#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let token = CancellationToken::new();

    let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);

    let args = Args::parse();
    let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
    let nodes_name: Vec<String> = vec![
        "/ip4/143.110.189.47/tcp/60000/p2p/16Uiu2HAkufuEruSFrepFCtPS2sBuKwTCMPCrBMT188JHs4hTtFDY"
            .to_string(),
    ];
    let user_address = signer.address().to_string();
    let group_name: String = "new_group".to_string();
    let node = setup_node_handle(nodes_name).unwrap();

    // Create user
    let user = User::new(&args.user_priv_key, node).await?;
    let user_arc = Arc::new(Mutex::new(user));

    let (messages_tx, messages_rx) = mpsc::channel::<Msg>(100);
    messages_tx
        .send(Msg::Input(Message::System(format!(
            "Hello, {:}",
            user_address.clone()
        ))))
        .await?;

    let messages_tx2 = messages_tx.clone();
    let event_token = token.clone();
    let h1 = tokio::spawn(async move { event_handler(messages_tx2, cli_tx, event_token).await });

    let res_msg_tx = messages_tx.clone();
    let main_token = token.clone();
    let user = user_arc.clone();

    let h2 = tokio::spawn(async move {
        let (waku_cg_to_cli_tx, mut waku_cg_to_cli_rx) = mpsc::channel::<WakuMessage>(100);
        let (waku_sg_to_cli_tx, mut waku_sg_to_cli_rx) = mpsc::channel::<WakuMessage>(100);
        loop {
            tokio::select! {
                // Some(val) = waku_sg_to_cli_rx.recv() =>{
                //     println!("HERE SG TO CLI !!!");
                //     res_msg_tx.send(Msg::Input(Message::System("Get message from waku".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                //     let res = user.as_ref().lock().await.process_waku_msg(val).await;
                //     match res {
                //         Ok(msg) => {
                //             match msg {
                //                 Some(m) => res_msg_tx.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
                //                 None => continue
                //             }
                //         },
                //         Err(err) => {
                //             res_msg_tx
                //                 .send(Msg::Input(Message::Error(err.to_string())))
                //                 .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                //         },
                //     };
                // }
                // Some(val) = waku_cg_to_cli_rx.recv() =>{
                //     println!("HERE CG TO CLI !!!");
                //     res_msg_tx.send(Msg::Input(Message::System("Get message from waku".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                //     let res = user.as_ref().lock().await.process_waku_msg(val).await;
                //     match res {
                //         Ok(msg) => {
                //             match msg {
                //                 Some(m) => res_msg_tx.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
                //                 None => continue
                //             }
                //         },
                //         Err(err) => {
                //             res_msg_tx
                //                 .send(Msg::Input(Message::Error(err.to_string())))
                //                 .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                //         },
                //     };
                // }
                Some(command) = cli_gr_rx.recv() => {
                    res_msg_tx.send(Msg::Input(Message::System(format!("Get command: {:?}", command)))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                    match command {
                        Commands::CreateGroup {group_name} => {
                            let res = user.as_ref().lock().await.create_group(group_name.clone());
                            match res {
                                Ok(br) => {
                                    let msg = format!("Successfully create group: {:?}", group_name.clone());
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;

                                    let res_msg_tx_clone = res_msg_tx.clone();
                                    let user_clone = user_arc.clone();
                                    tokio::spawn(async move {
                                        while let Ok(msg) = br.recv() {
                                            let res = user_clone.as_ref().lock().await.process_waku_msg(msg).await;
                                            match res {
                                                Ok(msg) => {
                                                    match msg {
                                                        Some(m) => res_msg_tx_clone.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
                                                        None => continue
                                                    }
                                                },
                                                Err(err) => {
                                                    res_msg_tx_clone
                                                        .send(Msg::Input(Message::Error(err.to_string())))
                                                        .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                                },
                                            }
                                            // println!("SEND TO CLI");
                                            // waku_cg_to_cli_tx_clone.send(msg).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                        }
                                        Ok::<_, CliError>(())
                                    });

                                    // after create group run thread to send admin message each interval
                                    let user_clone = user_arc.clone();
                                    let group_name_clone = group_name.clone();
                                    let res_msg_tx_clone = res_msg_tx.clone();
                                    tokio::spawn(async move {
                                        let mut interval = tokio::time::interval(Duration::from_secs(20));
                                        loop {
                                            interval.tick().await;
                                            let res = send_admin_msg(&user_clone, group_name_clone.clone()).await.map_err(|err| CliError::SenderError(err.to_string()));
                                            match res {
                                                Ok(msg) => {
                                                    res_msg_tx_clone.send(Msg::Input(Message::System(format!("Successfully publish message with id: {}", msg)))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                                },
                                                Err(err) => {
                                                    res_msg_tx_clone.send(Msg::Input(Message::Error(err.to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                                },
                                            }
                                        }
                                        Ok::<_, CliError>(())
                                    });
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                },
                            };
                        },
                        Commands::SendMessage { group_name, msg } => {
                            let message = msg.join(" ");
                            let res = user.as_ref().lock().await.send_msg(&message, group_name.clone()).await;
                            match res {
                                Ok(_) => {
                                    res_msg_tx.send(Msg::Input(Message::Mine(group_name, user_address.clone(), message ))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                },
                            };
                        },
                        Commands::Subscribe { group_name } => {
                            let res = user.as_ref().lock().await.subscribe_to_group(group_name.clone()).await;
                            match res {
                                Ok(receiver) => {
                                    let msg = format!("Successfully subscribe to group: {:?}", group_name.clone());
                                    res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;

                                    // let waku_sg_to_cli_tx_clone = waku_sg_to_cli_tx.clone();
                                    let user_clone = user_arc.clone();
                                    let res_msg_tx_clone = res_msg_tx.clone();
                                    tokio::spawn(async move {
                                        while let Ok(msg) = receiver.recv() {
                                            let res = user_clone.as_ref().lock().await.process_waku_msg(msg).await;
                                            match res {
                                                Ok(msg) => {
                                                    match msg {
                                                        Some(m) => res_msg_tx_clone.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
                                                        None => continue
                                                    }
                                                },
                                                Err(err) => {
                                                    res_msg_tx_clone
                                                        .send(Msg::Input(Message::Error(err.to_string())))
                                                        .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                                },
                                            }
                                        }
                                        Ok::<_, CliError>(())
                                    });
                                },
                                Err(err) => {
                                    res_msg_tx
                                        .send(Msg::Input(Message::Error(err.to_string())))
                                        .await.map_err(|err| CliError::SenderError(err.to_string()))?;
                                },
                            };
                        },
                        Commands::Exit => {
                            res_msg_tx.send(Msg::Input(Message::System("Bye!".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
                            break
                        },
                    }
                }
                _ = main_token.cancelled() => {
                    break;
                }
                else => {
                    res_msg_tx.send(Msg::Input(Message::System("Something went wrong".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
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

async fn send_admin_msg(
    user: &Arc<Mutex<User>>,
    group_name: String,
) -> Result<String, Box<dyn Error>> {
    let mut user = user.as_ref().lock().await;
    let group = user.groups.get_mut(&group_name.clone()).unwrap();
    group.admin.as_mut().unwrap().generate_new_key_pair()?;
    let admin_msg = group.admin.as_mut().unwrap().generate_admin_message();
    let res = user.send_admin_msg(admin_msg, group_name.clone()).await;
    match res {
        Ok(msg_id) => return Ok(msg_id),
        Err(e) => return Err(e.to_string().into()),
    }
}
