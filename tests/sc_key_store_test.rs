use alloy::providers::ProviderBuilder;
use alloy::{network::EthereumWallet, primitives::Address, signers::local::PrivateKeySigner};
use de_mls::contact::CHAT_SERVER_ADDR;
use de_mls::user::User;
use ds::chat_client::{self, ChatClient, ChatMessages};
use ds::chat_server::ServerMessage;
use std::str::FromStr;
use std::time::Duration;
use tungstenite::Message;

use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;

use mls_crypto::openmls_provider::{MlsCryptoProvider, CIPHERSUITE};

pub fn alice_addr_test() -> (Address, EthereumWallet) {
    let alice_address = Address::from_str("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap(); // anvil default key 0
    let signer = PrivateKeySigner::from_str(
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    (alice_address, wallet)
}

pub fn bob_addr_test() -> (Address, EthereumWallet) {
    let bob_address = Address::from_str("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap(); // anvil default key 0
    let signer = PrivateKeySigner::from_str(
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    (bob_address, wallet)
}

pub fn carla_addr_test() -> (Address, EthereumWallet) {
    let carla_address = Address::from_str("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC").unwrap(); // anvil default key 0
    let signer = PrivateKeySigner::from_str(
        "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    )
    .unwrap();
    let wallet = EthereumWallet::from(signer);
    (carla_address, wallet)
}

pub(crate) fn test_identity(
    address: Address,
    crypto: &MlsCryptoProvider,
) -> (KeyPackage, SignatureKeyPair) {
    let ciphersuite = CIPHERSUITE;
    let signature_keys = SignatureKeyPair::new(ciphersuite.signature_algorithm()).unwrap();
    let credential = Credential::new(address.to_vec(), CredentialType::Basic).unwrap();

    let credential_with_key = CredentialWithKey {
        credential,
        signature_key: signature_keys.to_public_vec().into(),
    };
    signature_keys.store(crypto.key_store()).unwrap();

    let key_package = KeyPackage::builder()
        .build(
            CryptoConfig {
                ciphersuite,
                version: ProtocolVersion::default(),
            },
            crypto,
            &signature_keys,
            credential_with_key.clone(),
        )
        .unwrap();

    (key_package, signature_keys)
}

#[tokio::test]
async fn test_input_request() {
    let (chat_client, mut client_recv) = ChatClient::connect(
        CHAT_SERVER_ADDR,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    )
    .await
    .unwrap();
    let mut alice = User::new(
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        chat_client,
    )
    .await
    .unwrap();

    let client_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(alice.get_wallet())
        .on_http(url::Url::from_str("http://localhost:8545").unwrap());

    let res = alice
        .connect_to_smart_contract(
            "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512",
            client_provider,
        )
        .await;

    assert!(res.is_ok());

    let h2 = tokio::spawn(async move {
        while let Some(msg) = client_recv.recv().await {
            if let Message::Text(text) = msg {
                if let Ok(chat_message) = serde_json::from_str::<ServerMessage>(&text) {
                    match chat_message {
                        // ServerMessage::InMessage { from, to, msg } => {
                        //     println!("alice received TextMsg from {}: {}", from, msg);
                        // }
                        ServerMessage::SystemJoin { username } => {
                            println!("Client1 received SystemJoin message for user: {}", username);
                        }
                        ServerMessage::InMessage { from, to, msg } => {
                            println!("Client1 received inmessage message for user: {}", from);
                            if let Ok(chat_msg) = serde_json::from_str::<ChatMessages>(&msg) {
                                match chat_msg {
                                    ChatMessages::Request(req) => {
                                        let res = alice.send_responce_on_request(
                                            req,
                                            to[0].clone(),
                                            &from,
                                        );
                                        match res {
                                            Ok(_) => {
                                                println!("Succesfully create responce");
                                            }
                                            Err(err) => {
                                                eprintln!("Error: {}", err);
                                            }
                                        }
                                    }
                                    ChatMessages::Response(resp) => {
                                        let res =
                                            alice.parce_responce(resp, "test".to_string()).await;
                                        match res {
                                            Ok(_) => {
                                                println!("Succesfully parse responce");
                                            }
                                            Err(err) => {
                                                eprintln!("Error: {}", err);
                                            }
                                        }
                                    }
                                    ChatMessages::Welcome(welcome) => {
                                        let res = alice.join_group(welcome).await;
                                        match res {
                                            Ok(mut buf) => {
                                                let msg = format!(
                                                    "Succesfully join to the group: {:#?}",
                                                    buf.1
                                                );
                                            }
                                            Err(err) => {
                                                eprintln!("Error: {}", err);
                                            }
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let (chat_client_bob, mut client_recv_bo) = ChatClient::connect(
        CHAT_SERVER_ADDR,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    )
    .await
    .unwrap();
    // let mut bob = User::new(
    //     "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    //     chat_client_bob,
    // )
    // .await
    // .unwrap();
    //
    let client2_send_task = tokio::spawn(async move {
        loop {
            println!("send msg");
            tokio::time::sleep(Duration::from_secs(7)).await; // Delay between messages
            let message = ServerMessage::InMessage {
                from: "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".to_string(),
                to: vec!["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string()],
                msg: "Hello from user1!".to_string(),
            };
            if let Err(e) = chat_client_bob.send_message_to_server(message) {
                eprintln!("Client2 failed to send message: {}", e);
            }
        }
    });

    let _ = tokio::join!(h2, client2_send_task);

    // let bob_signer = PrivateKeySigner::from_str(
    //     "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    // )
    // .unwrap();
    // let bob_address = bob_signer.address();

    // let crypto = MlsCryptoProvider::default();
    // let (bob_key, bob_sign_pk) = test_identity(bob_address, &crypto);

    // let bytes_key = bob_key.tls_serialize_detached().unwrap();

    // let res = alice.add_user_to_acl(&bob_address.to_string()).await;

    // assert!(res.is_ok());

    // let res = alice.restore_key_package(&bytes_key).await;

    // assert!(res.is_ok())
}

#[tokio::test]
async fn bob_test() {

    // let client_provider_bob = ProviderBuilder::new()
    //     .with_recommended_fillers()
    //     .wallet(bob.get_wallet())
    //     .on_http(url::Url::from_str("http://localhost:8545").unwrap());

    // let res = bob
    //     .connect_to_smart_contract(
    //         "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512",
    //         client_provider_bob,
    //     )
    //     .await;

    // bob.contacts
    //     .send_req_msg_to_user(
    //         "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".to_string(),
    //         "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    //         "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512".to_string(),
    //         chat_client::ReqMessageType::InviteToGroup,
    //     )
    //     .unwrap();
}
