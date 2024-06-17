use std::collections::HashMap;

use bus::{Bus, BusReader};
// use chrono::Utc;
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
        msg: GroupMessage,
        // pubsub_topic: WakuPubSubTopic,
        // content_topic: WakuContentTopic,
    ) -> Result<(), String> {
        // let buff = self.msg.tls_serialize_detached().unwrap();
        // Message::encode(&self.msg, &mut buff).expect("Could not encode :(");

        // TODO: manage hanshake message
        // let protocol_msg: ProtocolMessage = group_msg.msg.clone().into();

        // Reject any handshake message that has an earlier epoch than the one we know
        // about.
        self.pub_node.broadcast(msg.msg);

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
        mut self,
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
        Ok(node.recv().unwrap())
    }
}

/// An core group message.
/// This is an `MLSMessage` plus the list of recipients as a vector of client
/// names
#[derive(Clone, PartialEq)]
pub struct GroupMessage {
    // #[prost(message, repeated, tag = "1")]
    pub recipients: Vec<Vec<u8>>,
    // #[prost(message,  tag = "2")]
    pub msg: MlsMessageIn,
}

impl GroupMessage {
    /// Create a new `GroupMessage` taking an `MlsMessageIn` and slice of
    /// recipient names.
    pub fn new(msg: MlsMessageIn, recipients: &[Vec<u8>]) -> Self {
        Self {
            msg,
            recipients: recipients.to_vec(),
        }
    }
}

// #[derive(Debug, thiserror::Error)]
// pub enum WakuHandlingError {
//     // #[error(transparent)]
//     // ParseUrlError(#[from] Pars),
//     #[error("Subscription error to the content topic. {}", .0)]
//     ContentTopicsError(String),
//     #[error("Unable to retrieve peers list. {}", .0)]
//     RetrievePeersError(String),
//     #[error("Unable to publish message to peer: {}", .0)]
//     PublishMessage(String),
//     #[error("Unable to validate a message from peer: {}", .0)]
//     InvalidMessage(String),
//     // #[error(transparent)]
//     // ParsePortError(#[from] ParseIntError),
//     #[error("Unable to create waku node: {}", .0)]
//     CreateNodeError(String),
//     #[error("Unable to stop waku node: {}", .0)]
//     StopNodeError(String),
//     #[error("Unable to get peer information: {}", .0)]
//     PeerInfoError(String),
//     // #[error(transparent)]
//     // QueryResponseError(#[from] QueryError),
//     // #[error("Unknown error: {0}")]
//     // Other(anyhow::Error),
// }

// impl WakuHandlingError {
//     pub fn type_string(&self) -> &str {
//         match self {
//             // WakuHandlingError::ParseUrlError(_) => "ParseUrlError",
//             WakuHandlingError::ContentTopicsError(_) => "ContentTopicsError",
//             WakuHandlingError::RetrievePeersError(_) => "RetrievePeersError",
//             WakuHandlingError::PublishMessage(_) => "PublishMessage",
//             WakuHandlingError::InvalidMessage(_) => "InvalidMessage",
//             // WakuHandlingError::ParsePortError(_) => "ParsePortError",
//             WakuHandlingError::CreateNodeError(_) => "CreateNodeError",
//             WakuHandlingError::StopNodeError(_) => "StopNodeError",
//             WakuHandlingError::PeerInfoError(_) => "PeerInfoError",
//             // WakuHandlingError::QueryResponseError(_) => "QueryResponseError",
//             // WakuHandlingError::Other(_) => "Other",
//         }
//     }
// }
