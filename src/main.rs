use alloy::{primitives::Address, providers::ProviderBuilder, signers::local::PrivateKeySigner};
use chrono::Utc;
use clap::Parser;
use sc_key_store::{sc_ks::ScKeyStorage, SCKeyStoreService};
use std::{clone, error::Error, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, Mutex};

use de_mls::{
    cli::*,
    user::{AdminTrait, User},
};
use ds::ds_waku::setup_node_handle;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
    let nodes_name: Vec<String> = args.nodes.split(',').map(|s| s.to_string()).collect();
    let user_address = signer.address().to_string();
    let group_name: String = "new_group".to_string();
    let group_name_2: String = "new_group_2".to_string();


    let node = setup_node_handle(nodes_name).unwrap();

    // Create user
    let user_n = User::new(&args.user_priv_key, node).await?;
    let user_arc = Arc::new(Mutex::new(user_n));

    let receiver = user_arc
        .as_ref()
        .lock()
        .await
        .create_group(group_name.clone())?;
    let topics = user_arc
        .as_ref()
        .lock()
        .await
        .waku_node
        .relay_topics()
        .unwrap();
    println!("Topics: {:?}", topics);

    let receiver_2 = user_arc
        .as_ref()
        .lock()
        .await
        .create_group(group_name_2.clone())?;
    let topics = user_arc
        .as_ref()
        .lock()
        .await
        .waku_node
        .relay_topics()
        .unwrap();
    println!("Topics: {:?}", topics);

    let user_recv_clone = user_arc.clone();
    let group_name_clone = group_name.clone();
    let h1 = tokio::spawn(async move {
        while let Ok(msg) = receiver.recv() {
            let mut user = user_recv_clone.as_ref().lock().await;
            let res = user.process_waku_msg(group_name_clone.clone(), msg).await;
            match res {
                Ok(_) => println!("Successfully process message from receiver"),
                Err(e) => println!("Failed to process message: {:?}", e),
            }
        }
    });

    let user_clone = user_arc.clone();
    let group_name_clone = group_name.clone();
    let h2 = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let mut user = user_clone.as_ref().lock().await;
            let group = user.groups.get_mut(&group_name_clone.clone()).unwrap();
            let res = group.admin.as_mut().unwrap().generate_new_key_pair();
            match res {
                Ok(_) => println!("Successfully generate key pair"),
                Err(e) => println!("Failed to generate key pair: {:?}", e),
            }
            let admin_msg = group.admin.as_mut().unwrap().generate_admin_message();
            let res = user
                .send_admin_msg(admin_msg, group_name_clone.clone())
                .await;
            match res {
                Ok(msg_id) => println!("Successfully publish message with id: {}", msg_id),
                Err(e) => println!("Failed to publish message: {:?}", e),
            }
        }
    });

    let mut processing_interval = tokio::time::interval(Duration::from_secs(30));
    let user_clone = user_arc.clone();
    let group_name_clone = group_name.clone();
    let h3 = tokio::spawn(async move {
        println!("Processing messages");
        loop {
            processing_interval.tick().await;
            let mut user = user_clone.as_ref().lock().await;
            let group = user.groups.get_mut(&group_name_clone.clone()).unwrap();
            let res = group.admin.as_mut().unwrap().process_messages().await;
            match res {
                Ok(_) => println!("Successfully process messages from queue"),
                Err(e) => println!("Failed to process messages: {:?}", e),
            }
        }
    });
    let handler_res = tokio::join!(h1, h2, h3);
    handler_res.0.unwrap();
    handler_res.1.unwrap();
    handler_res.2.unwrap();

    Ok(())
}



// #[tokio::main]
// async fn main_old() -> Result<(), Box<dyn Error>> {
//     let token = CancellationToken::new();

//     let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);

//     let args = Args::parse();
//     let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
//     let nodes_name: Vec<String> = args.nodes.split(',').map(|s| s.to_string()).collect();
//     let user_address = signer.address().to_string();

//     let node = setup_node_handle(nodes_name).unwrap();

//     // Create user
//     let user_n = User::new(&args.user_priv_key, node, true).await?;
//     let user_arc = Arc::new(Mutex::new(user_n));

//     let (messages_tx, messages_rx) = mpsc::channel::<Msg>(100);
//     messages_tx
//         .send(Msg::Input(Message::System(format!(
//             "Hello, {:}",
//             user_address.clone()
//         ))))
//         .await?;

//     let messages_tx2 = messages_tx.clone();
//     let event_token = token.clone();
//     let h1 = tokio::spawn(async move { event_handler(messages_tx2, cli_tx, event_token).await });

//     let res_msg_tx = messages_tx.clone();
//     let main_token = token.clone();
//     let user = user_arc.clone();
//     let h2 = tokio::spawn(async move {
//         let (redis_tx, mut redis_rx) = mpsc::channel::<Vec<u8>>(100);

//         loop {
//             tokio::select! {
//                 Some(val) = redis_rx.recv() =>{
//                     let res = user.as_ref().lock().await.receive_msg(val).await;
//                     match res {
//                         Ok(msg) => {
//                             match msg {
//                                 Some(m) => res_msg_tx.send(Msg::Input(Message::System(m.message))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
//                                 None => continue
//                             }
//                         },
//                         Err(err) => {
//                             res_msg_tx
//                                 .send(Msg::Input(Message::Error(err.to_string())))
//                                 .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                         },
//                     };
//                 }
//                 Some(command) = cli_gr_rx.recv() => {
//                     // res_msg_tx.send(Msg::Input(Message::System(format!("Get command: {:?}", command)))).await?;
//                     match command {
//                         Commands::CreateGroup { group_name, storage_address, storage_url } => {
//                             let client_provider = ProviderBuilder::new()
//                                 .with_recommended_fillers()
//                                 .wallet(user.as_ref().lock().await.wallet())
//                                 .on_http(storage_url);
//                         let sc_storage_address = Address::from_str(&storage_address).unwrap();
//                         let mut sc_ks = ScKeyStorage::new(client_provider, sc_storage_address);
//                         let res = sc_ks.add_user(&user.as_ref().lock().await.identity.to_string()).await;
//                             match res {
//                                 Ok(_) => {
//                                     let msg = format!("Successfully connect to Smart Contract on address {:}\n", storage_address);
//                                     res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };

//                             let res = user.as_ref().lock().await.create_group(group_name.clone(), storage_address.clone());
//                             match res {
//                                 Ok(br) => {
//                                     let msg = format!("Successfully create group: {:?}", group_name.clone());
//                                     res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;

//                                     let redis_tx = redis_tx.clone();
//                                     tokio::spawn(async move {
//                                         while let Ok(msg) = br.recv() {
//                                             let bytes: Vec<u8> = msg.payload().to_vec();
//                                             redis_tx.send(bytes).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                         }
//                                         Ok::<_, CliError>(())
//                                     });
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };
//                         },
//                         Commands::Invite { group_name, users_wallet_addrs } => {
//                             let user_clone = user.clone();
//                             let res_msg_tx_c = messages_tx.clone();
//                             tokio::spawn(async move {
//                                 for user_wallet in users_wallet_addrs.iter() {
//                                     let user_clone_ref = user_clone.as_ref();
//                                     let opt_token =
//                                     {
//                                         let mut user_clone_ref_lock = user_clone_ref.lock().await;
//                                         let res = user_clone_ref_lock.handle_send_req(user_wallet, group_name.clone()).await;
//                                         match res {
//                                             Ok(token) => {
//                                                 token
//                                             },
//                                             Err(err) => {
//                                                 res_msg_tx_c
//                                                     .send(Msg::Input(Message::Error(err.to_string())))
//                                                     .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                                 None
//                                             },
//                                         }
//                                     };

//                                     match opt_token {
//                                         Some(token) => token.cancelled().await,
//                                         None => return Err(CliError::TokenCancellingError),
//                                     };

//                                     // {
//                                     //     let mut user_clone_ref_lock = user_clone.as_ref().lock().await;
//                                     //     user_clone_ref_lock.contacts.future_req.remove(user_wallet);
//                                     //     let res = user_clone_ref_lock.add_user_to_acl(user_wallet).await;
//                                     //     if let Err(err) = res {
//                                     //         res_msg_tx_c
//                                     //             .send(Msg::Input(Message::Error(err.to_string())))
//                                     //             .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                     //     };
//                                     // }

//                                 }

//                                 let res = user_clone.as_ref().lock().await.invite(users_wallet_addrs.clone(), group_name.clone()).await;
//                                 match res {
//                                     Ok(_) => {
//                                         let msg = format!("Invite {:?} to the group {:}\n",
//                                             users_wallet_addrs, group_name
//                                         );
//                                         res_msg_tx_c.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                     },
//                                     Err(err) => {
//                                         res_msg_tx_c
//                                             .send(Msg::Input(Message::Error(err.to_string())))
//                                             .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                     },
//                                 };
//                                 Ok::<_, CliError>(())
//                             });
//                         },
//                         Commands::SendMessage { group_name, msg } => {
//                             let message = msg.join(" ");
//                             let res = user.as_ref().lock().await.send_msg(&message, group_name.clone(), user_address.clone()).await;
//                             match res {
//                                 Ok(_) => {
//                                     res_msg_tx.send(Msg::Input(Message::Mine(group_name, user_address.clone(), message ))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };
//                         },
//                         Commands::SendToGroup { group_name, msg } => {
//                             let message = msg.join(" ");
//                             // let res = user.as_ref().lock().await.send_to_group(&message, group_name.clone()).await;
//                             // match res {
//                             //     Ok(_) => {
//                             //         res_msg_tx.send(Msg::Input(Message::Mine(group_name, user_address.clone(), message ))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                             //     },
//                             //     Err(err) => {
//                             //         res_msg_tx
//                             //             .send(Msg::Input(Message::Error(err.to_string())))
//                             //             .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                             //     },
//                             // };
//                         },
//                         Commands::Subscribe { group_name } => {
//                             let res = user.as_ref().lock().await.subscribe_to_group(group_name.clone()).await;
//                             match res {
//                                 Ok((receiver, topics)) => {
//                                     let msg = format!("Successfully subscribe to group: {:?}", group_name.clone());
//                                     res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                     res_msg_tx.send(Msg::Input(Message::System(format!("topics: {:?}", topics)))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                     let redis_tx = redis_tx.clone();
//                                     tokio::spawn(async move {

//                                         while let Ok(msg) = receiver.recv() {
//                                             let bytes: Vec<u8> = msg.payload().to_vec();
//                                             redis_tx.send(bytes).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                         }
//                                         Ok::<_, CliError>(())
//                                     });
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };
//                         },
//                         Commands::Exit => {
//                             res_msg_tx.send(Msg::Input(Message::System("Bye!".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                             break
//                         },
//                     }
//                 }
//                 _ = main_token.cancelled() => {
//                     break;
//                 }
//                 else => {
//                     res_msg_tx.send(Msg::Input(Message::System("Something went wrong".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                     break
//                 }
//             };
//         }
//         Ok::<_, CliError>(())
//     });

//     let h3 = tokio::spawn(async move { terminal_handler(messages_rx, token).await });

//     let handler_res = tokio::join!(h1, h2, h3);
//     handler_res.0??;
//     handler_res.1??;
//     handler_res.2??;

//     Ok(())
// }
