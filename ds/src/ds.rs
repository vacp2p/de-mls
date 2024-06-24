use bus::{Bus, BusReader};
// use chrono::Utc;
use std::collections::HashMap;

use openmls::framing::MlsMessageIn;
// use waku_bindings::*;

use sc_key_store::{pks::PublicKeyStorage, KeyStoreError};

#[derive(Debug)]
pub struct DSClient {
    // node: WakuNodeHandle<Running>,
    pub pub_node: Bus<MlsMessageIn>,
    pub sub_node: HashMap<Vec<u8>, BusReader<MlsMessageIn>>,
}

impl DSClient {
    pub fn new_with_subscriber(id: Vec<u8>) -> Result<DSClient, DeliveryServiceError> {
        let mut ds = DSClient {
            pub_node: Bus::new(10),
            sub_node: HashMap::new(),
        };
        ds.add_subscriber(id)?;
        Ok(ds)
    }

    pub fn add_subscriber(&mut self, id: Vec<u8>) -> Result<(), DeliveryServiceError> {
        let rx = self.pub_node.add_rx();
        self.sub_node.insert(id, rx);
        Ok(())
    }

    pub fn msg_send(
        &mut self,
        msg: MlsMessageIn,
        // pubsub_topic: WakuPubSubTopic,
        // content_topic: WakuContentTopic,
    ) -> Result<(), DeliveryServiceError> {
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
        id: &[u8],
        pks: &PublicKeyStorage,
    ) -> Result<MlsMessageIn, DeliveryServiceError> {
        let node: &mut BusReader<MlsMessageIn> = match self.sub_node.get_mut(id) {
            Some(node) => node,
            None => return Err(DeliveryServiceError::EmptySubNodeError),
        };
        node.recv().map_err(DeliveryServiceError::RecieveError)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Could not get data from Key Store: {0}")]
    KeyStoreError(KeyStoreError),
    #[error("Unauthorized User")]
    UnauthorizedUserError,
    #[error("Subscriber doesn't exist")]
    EmptySubNodeError,
    #[error("Reciever error: {0}")]
    RecieveError(#[from] std::sync::mpsc::RecvError),
}
