use alloy::{
    hex::{self, ToHexExt},
    primitives::Address,
    providers::ProviderBuilder,
};
use clap::Parser;

use std::str::FromStr;
use tls_codec::Deserialize;
use tokio::sync::mpsc;

use openmls::{framing::MlsMessageIn, prelude::TlsSerializeTrait};

use de_mls::{cli::*, user::User};
use promkit::preset::readline::Readline;

#[tokio::main]
async fn main() -> Result<(), CliError> {
    let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);
    let (res_msg_tx, mut res_msg_rx) = mpsc::channel::<String>(100);

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

    tokio::spawn(async move {
        loop {
            // let line = readline().unwrap();
            // let line = line.trim();
            // if line.is_empty() {
            //     continue;
            // }
            let line: String = Readline::default()
                .title("> ")
                .prompt()
                .unwrap()
                .run()
                .unwrap();

            let args = shlex::split(&line).ok_or(CliError::SplitLineError).unwrap();
            let cli = Cli::try_parse_from(args).unwrap();
            cli_tx.send(cli.command).await.unwrap();
        }
    });

    tokio::spawn(async move {
        let (redis_tx, mut redis_rx) = mpsc::channel::<Vec<u8>>(100);
        println!("Start loop");
        loop {
            tokio::select! {
                Some(val) = redis_rx.recv() =>{
                    let res = MlsMessageIn::tls_deserialize_bytes(val).unwrap();
                    println!("Get Message from Redis: {:#?}", res);
                    let msg = user.receive_msg(res).await.unwrap();
                    match msg {
                        Some(m) =>  res_msg_tx.send(m.to_string()).await.unwrap(),
                        None => break
                    }
                }
                Some(command) = cli_gr_rx.recv() => {
                    // res_msg_tx.send(format!("Get command: {:?}", command)).await.unwrap();
                    println!("Get command: {:?}", command);
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
                            res_msg_tx.send(string).await.unwrap();
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
                            println!("Exiting");
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

    println!("Start ui loop");
    loop {
        tokio::select! {
            // Some(val) = cli_rx.recv() => {
            //     if val == "exit" {
            //         break;
            //     }
            //     println!("Me: {:?}", val);
            // }
            Some(val) = res_msg_rx.recv() => {
                println!("Them: {:#?}", val);
            }
            else => {
                println!("Break UI");
                break
            }
        }
    }

    Ok(())
}
