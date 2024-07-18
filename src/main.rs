use alloy::providers::ProviderBuilder;
use clap::Parser;
use openmls::framing::MlsMessageIn;
use std::{error::Error, fs::File, io::Read};
use tls_codec::Deserialize;
use tokio::sync::mpsc;
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
    let (user_address, wallet, storage_address) = get_user_data(&args)?;

    let client_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(args.storage_url);

    //// Create user
    let mut user = User::new(
        user_address.as_slice(),
        client_provider.clone(),
        storage_address,
    )
    .await?;

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
                        Commands::CreateGroup { group_name } => {
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
                        Commands::Invite { group_name, user_wallet } => {
                            let res = user.invite(user_wallet, group_name.clone()).await;
                            match res {
                                Ok(_) => {
                                    let msg = format!("Invite {:} to the group {:}\n",
                                        user_address, group_name
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
                        Commands::JoinGroup { group_name } => {
                            let mut file = File::open(format!("invite_{group_name}.txt"))?;
                            let mut welcome = String::new();
                            file.read_to_string(&mut welcome).unwrap();

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
