use alloy::{hex::ToHexExt, primitives::SignatureError, signers::Signature};
use ds::{
    chat_client::{
        ChatClient, ChatMessages, ReqMessageType, RequestMLSPayload, ResponseMLSPayload,
    },
    chat_server::ServerMessage,
    ChatServiceError,
};
// use waku_bindings::*;

use openmls::prelude::MlsMessageOut;
use std::{collections::HashMap, sync::Arc};
use tls_codec::Serialize;
use tokio::sync::Mutex;

pub const CHAT_SERVER_ADDR: &str = "ws://127.0.0.1:8080";

pub struct ContactsList {
    contacts: Arc<Mutex<HashMap<String, Contact>>>,
    pub chat_client: ChatClient,
}

pub struct Contact {
    user_address: String,
    // map group_name to key_package bytes
    user_kp2group: HashMap<String, Vec<u8>>,
    // user_p2p_addr: WakuPeers,
}

impl Contact {
    pub fn get_relevant_kp(&mut self, group_name: String) -> Result<Vec<u8>, ContactError> {
        match self.user_kp2group.remove(&group_name) {
            Some(kp) => Ok(kp.clone()),
            None => Err(ContactError::UnknownKeyPackageForGroupError),
        }
    }

    pub fn add_key_package(
        &mut self,
        key_package: Vec<u8>,
        group_name: String,
    ) -> Result<(), ContactError> {
        match self.user_kp2group.insert(group_name, key_package) {
            Some(_) => Err(ContactError::DoubleUserError),
            None => Ok(()),
        }
    }
}

impl ContactsList {
    pub async fn new(chat_client: ChatClient) -> Result<Self, ContactError> {
        Ok(ContactsList {
            contacts: Arc::new(Mutex::new(HashMap::new())),
            chat_client,
        })
    }

    pub fn send_welcome_msg_to_users(
        &mut self,
        self_address: String,
        users_address: Vec<String>,
        welcome: MlsMessageOut,
    ) -> Result<(), ContactError> {
        let bytes = welcome.tls_serialize_detached()?;
        let welcome_str: String = bytes.encode_hex();

        let msg = ChatMessages::Welcome(welcome_str);
        self.chat_client
            .send_message_to_server(ServerMessage::InMessage {
                from: self_address,
                to: users_address,
                msg: serde_json::to_string(&msg)?,
            })?;

        Ok(())
    }

    pub async fn send_req_msg_to_user(
        &mut self,
        self_address: String,
        user_address: &str,
        sc_address: String,
        msg_type: ReqMessageType,
    ) -> Result<(), ContactError> {
        let req = ChatMessages::Request(RequestMLSPayload::new(sc_address, msg_type));
        self.chat_client
            .send_request(ServerMessage::InMessage {
                from: self_address,
                to: vec![user_address.to_string()],
                msg: serde_json::to_string(&req)?,
            })
            .await?;
        Ok(())
    }

    pub fn send_resp_msg_to_user(
        &mut self,
        self_address: String,
        user_address: &str,
        resp: ResponseMLSPayload,
    ) -> Result<(), ContactError> {
        let resp_j = ChatMessages::Response(resp);
        self.chat_client
            .send_message_to_server(ServerMessage::InMessage {
                from: self_address,
                to: vec![user_address.to_string()],
                msg: serde_json::to_string(&resp_j)?,
            })?;
        Ok(())
    }

    // pub fn recive_resp_msg(
    //     &mut self,
    //     msg_bytes: &str,
    //     sc_address: &str,
    // ) -> Result<(), ContactError> {
    //     let resp: ResponseMLSPayload = serde_json::from_str(msg_bytes)?;
    //     let check_msg = REQUEST.to_owned() + sc_address;

    //     if !self.contacts.contains_key(&resp.user_address) {
    //         return Err(ContactError::UnknownUserError);
    //     }

    //     // Verify msg: https://alloy.rs/examples/wallets/verify_message.html
    //     let signature = Signature::from_str(&resp.signature)?;
    //     let addr = signature.recover_address_from_msg(&check_msg)?;
    //     if addr.to_string() != resp.user_address {
    //         return Err(ContactError::InvalidSignature);
    //     }

    //     Ok(())
    // }

    pub async fn add_new_contact(&mut self, user_address: &str) -> Result<(), ContactError> {
        let mut contacts = self.contacts.lock().await;
        if contacts.contains_key(user_address) {
            return Err(ContactError::DoubleUserError);
        }

        match contacts.insert(
            user_address.to_string(),
            Contact {
                user_address: user_address.to_string(),
                user_kp2group: HashMap::new(),
            },
        ) {
            Some(_) => Err(ContactError::DoubleUserError),
            None => Ok(()),
        }
    }

    pub async fn add_key_package_to_contact(
        &mut self,
        user_wallet: &str,
        key_package: Vec<u8>,
        group_name: String,
    ) -> Result<(), ContactError> {
        let mut contacts = self.contacts.lock().await;
        match contacts.get_mut(user_wallet) {
            Some(user) => user.add_key_package(key_package, group_name)?,
            None => return Err(ContactError::UnknownUserError),
        }
        Ok(())
    }

    pub async fn does_user_in_contacts(&self, user_wallet: &str) -> bool {
        let contacts = self.contacts.lock().await;
        contacts.get(user_wallet).is_some()
    }

    pub async fn prepare_joiners(
        &mut self,
        user_wallets: Vec<String>,
        group_name: String,
    ) -> Result<HashMap<String, Vec<u8>>, ContactError> {
        let mut joiners_kp = HashMap::with_capacity(user_wallets.len());

        for user_wallet in user_wallets {
            if joiners_kp.contains_key(&user_wallet) {
                return Err(ContactError::DoubleUserError);
            }

            let mut contacts = self.contacts.lock().await;
            match contacts.get_mut(&user_wallet) {
                Some(contact) => match contact.get_relevant_kp(group_name.clone()) {
                    Ok(kp) => match joiners_kp.insert(user_wallet, kp) {
                        Some(_) => return Err(ContactError::DoubleUserError),
                        None => continue,
                    },
                    Err(err) => return Err(err),
                },
                None => return Err(ContactError::UnknownUserError),
            }
        }

        Ok(joiners_kp)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContactError {
    #[error("Key package for given group doesnt exist")]
    UnknownKeyPackageForGroupError,
    #[error("Unknown user")]
    UnknownUserError,
    #[error("Double user in joiners users list")]
    DoubleUserError,
    #[error("Invalid user address inside signature")]
    InvalidSignature,

    #[error(transparent)]
    ChatServiceError(#[from] ChatServiceError),

    #[error("Can't parce signature: {0}")]
    AlloySignatureError(#[from] SignatureError),
    #[error("Json error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
}
