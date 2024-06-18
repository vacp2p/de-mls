use bus::{Bus, BusReader};
// use chrono::Utc;
use std::collections::HashMap;

use openmls::framing::MlsMessageIn;
// use waku_bindings::*;

use crate::{keystore::PublicKeyStorage, AuthToken};

#[derive(Debug)]
pub struct DSClient {
    // node: WakuNodeHandle<Running>,
    pub pub_node: Bus<MlsMessageIn>,
    pub sub_node: HashMap<Vec<u8>, BusReader<MlsMessageIn>>,
}

impl DSClient {
    pub fn new_with_subscriber(id: Vec<u8>) -> Self {
        let mut ds = DSClient {
            pub_node: Bus::new(10),
            sub_node: HashMap::new(),
        };
        let _ = ds.add_subscriber(id);
        ds
    }

    pub fn add_subscriber(&mut self, id: Vec<u8>) -> Result<(), String> {
        let rx = self.pub_node.add_rx();
        self.sub_node.insert(id, rx);
        Ok(())
    }

    pub fn msg_send(
        &mut self,
        msg: MlsMessageIn,
        // pubsub_topic: WakuPubSubTopic,
        // content_topic: WakuContentTopic,
    ) -> Result<(), String> {
        // let buff = self.msg.tls_serialize_detached().unwrap();
        // Message::encode(&self.msg, &mut buff).expect("Could not encode :(");
        self.pub_node.broadcast(msg);

        // let waku_message = WakuMessage::new(
        //     buff,
        //     content_topic,
        //     2,
        //     Utc::now().timestamp() as usize,
        //     vec![],
        //     true,
        // );

        // node_handle
        //     .node
        //     .relay_publish_message(&waku_message, Some(pubsub_topic.clone()), None)
        //     .map_err(|e| {
        //         debug!(
        //             error = tracing::field::debug(&e),
        //             "Failed to relay publish the message"
        //         );
        //         WakuHandlingError::PublishMessage(e)
        //     });

        Ok(())
    }

    pub fn msg_recv(
        &mut self,
        id: Vec<u8>,
        auth_token: AuthToken,
        pks: &PublicKeyStorage,
    ) -> Result<MlsMessageIn, String> {
        let auth_t = match pks.get_user_auth_token(id.clone()) {
            Ok(c) => c,
            Err(c) => return Err(c),
        };

        if auth_token != auth_t {
            return Err("Unauthorized client".to_string());
        }

        let node: &mut BusReader<MlsMessageIn> = self.sub_node.get_mut(&id).unwrap();
        let msg = node.recv().unwrap();

        Ok(msg)
    }
}
