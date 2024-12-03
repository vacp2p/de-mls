use alloy::hex::ToHexExt;

// use waku_bindings::*;

use openmls::prelude::MlsMessageOut;
use std::{collections::HashMap, sync::Arc};
use tls_codec::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::ContactError;

pub const CHAT_SERVER_ADDR: &str = "ws://127.0.0.1:8080";

pub struct ContactsList {
    contacts: Arc<Mutex<HashMap<String, Contact>>>,
    group_id2sc: HashMap<String, String>,
    pub future_req: HashMap<String, CancellationToken>,
    // pub chat_client: ChatClient,
}

pub struct Contact {
    // map group_name to key_package bytes
    group_id2user_kp: HashMap<String, Vec<u8>>,
    // user_p2p_addr: WakuPeers,
}

impl Contact {
    pub fn get_relevant_kp(&mut self, group_name: String) -> Result<Vec<u8>, ContactError> {
        match self.group_id2user_kp.remove(&group_name) {
            Some(kp) => Ok(kp.clone()),
            None => Err(ContactError::MissingKeyPackageForGroup),
        }
    }

    pub fn add_key_package(
        &mut self,
        key_package: Vec<u8>,
        group_name: String,
    ) -> Result<(), ContactError> {
        match self.group_id2user_kp.insert(group_name, key_package) {
            Some(_) => Err(ContactError::DuplicateUserError),
            None => Ok(()),
        }
    }
}

impl ContactsList {
    pub async fn new() -> Result<Self, ContactError> {
        Ok(ContactsList {
            contacts: Arc::new(Mutex::new(HashMap::new())),
            group_id2sc: HashMap::new(),
            future_req: HashMap::new(),
            // chat_client,
        })
    }

    // pub fn send_welcome_msg_to_users(
    //     &mut self,
    //     self_address: String,
    //     users_address: Vec<String>,
    //     welcome: MlsMessageOut,
    // ) -> Result<(), ContactError> {
    //     let bytes = welcome.tls_serialize_detached()?;
    //     let welcome_str: String = bytes.encode_hex();

    //     let msg = ChatMessages::Welcome(welcome_str);
    //     self.chat_client.send_message(ServerMessage::InMessage {
    //         from: self_address,
    //         to: users_address,
    //         msg: serde_json::to_string(&msg)?,
    //     })?;

    //     Ok(())
    // }

    // pub fn send_msg_req(
    //     &mut self,
    //     self_address: String,
    //     user_address: String,
    //     group_name: String,
    //     msg_type: ReqMessageType,
    // ) -> Result<(), ContactError> {
    //     self.future_req
    //         .insert(user_address.clone(), CancellationToken::new());

    //     let sc_address = match self.group_id2sc.get(&group_name).cloned() {
    //         Some(sc) => sc,
    //         None => return Err(ContactError::MissingSmartContractForGroup),
    //     };

    //     let req = ChatMessages::Request(RequestMLSPayload::new(sc_address, group_name, msg_type));
    //     // self.chat_client.send_message(ServerMessage::InMessage {
    //     //     from: self_address,
    //     //     to: vec![user_address],
    //     //     msg: serde_json::to_string(&req)?,
    //     // })?;

    //     Ok(())
    // }

    // pub fn send_resp_msg_to_user(
    //     &mut self,
    //     self_address: String,
    //     user_address: &str,
    //     resp: ResponseMLSPayload,
    // ) -> Result<(), ContactError> {
    //     let resp_j = ChatMessages::Response(resp);
    //     // self.chat_client.send_message(ServerMessage::InMessage {
    //     //     from: self_address,
    //     //     to: vec![user_address.to_string()],
    //     //     msg: serde_json::to_string(&resp_j)?,
    //     // })?;
    //     Ok(())
    // }

    pub async fn add_new_contact(&mut self, user_address: &str) -> Result<(), ContactError> {
        let mut contacts = self.contacts.lock().await;
        if contacts.contains_key(user_address) {
            return Err(ContactError::DuplicateUserError);
        }

        match contacts.insert(
            user_address.to_string(),
            Contact {
                group_id2user_kp: HashMap::new(),
            },
        ) {
            Some(_) => Err(ContactError::DuplicateUserError),
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
            None => return Err(ContactError::UserNotFoundError),
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
                return Err(ContactError::DuplicateUserError);
            }

            let mut contacts = self.contacts.lock().await;
            match contacts.get_mut(&user_wallet) {
                Some(contact) => match contact.get_relevant_kp(group_name.clone()) {
                    Ok(kp) => match joiners_kp.insert(user_wallet, kp) {
                        Some(_) => return Err(ContactError::DuplicateUserError),
                        None => continue,
                    },
                    Err(err) => return Err(err),
                },
                None => return Err(ContactError::UserNotFoundError),
            }
        }

        Ok(joiners_kp)
    }

    pub fn handle_response(&mut self, user_address: &str) -> Result<(), ContactError> {
        match self.future_req.get(user_address) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err(ContactError::UserNotFoundError),
        }
    }

    pub fn insert_group2sc(
        &mut self,
        group_name: String,
        sc_address: String,
    ) -> Result<(), ContactError> {
        match self.group_id2sc.insert(group_name, sc_address) {
            Some(_) => Err(ContactError::GroupAlreadyExistsError),
            None => Ok(()),
        }
    }

    pub fn group2sc(&self, group_name: String) -> Result<String, ContactError> {
        match self.group_id2sc.get(&group_name).cloned() {
            Some(addr) => Ok(addr),
            None => Err(ContactError::GroupNotFoundError(group_name)),
        }
    }
}
