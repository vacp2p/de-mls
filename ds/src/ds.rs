use fred::{
    clients::{RedisClient, SubscriberClient},
    error::RedisError,
    prelude::*,
    types::Message,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{error::RecvError, Receiver};

use openmls::{framing::MlsMessageOut, prelude::TlsSerializeTrait};
// use waku_bindings::*;

pub struct RClient {
    group_id: String,
    client: RedisClient,
    sub_client: SubscriberClient,
    // broadcaster: Receiver<Message>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SenderStruct {
    pub sender: String,
    pub msg: Vec<u8>,
}

impl RClient {
    pub async fn new_for_group(
        group_id: String,
    ) -> Result<(Self, Receiver<Message>), DeliveryServiceError> {
        let redis_client = RedisClient::default();
        let subscriber: SubscriberClient =
            Builder::default_centralized().build_subscriber_client()?;
        redis_client.init().await?;
        subscriber.init().await?;
        subscriber.subscribe(group_id.clone()).await?;
        Ok((
            RClient {
                group_id,
                client: redis_client,
                sub_client: subscriber.clone(),
                // broadcaster: subscriber.message_rx(),
            },
            subscriber.message_rx(),
        ))
    }

    pub async fn remove_from_group(&mut self) -> Result<(), DeliveryServiceError> {
        self.sub_client.unsubscribe(self.group_id.clone()).await?;
        self.sub_client.quit().await?;
        self.client.quit().await?;
        Ok(())
    }

    pub async fn msg_send(
        &mut self,
        msg: MlsMessageOut,
        sender: String,
    ) -> Result<(), DeliveryServiceError> {
        let buf = msg.tls_serialize_detached()?;

        let json_value = SenderStruct { sender, msg: buf };
        let bytes = serde_json::to_vec(&json_value)?;
        self.client
            .publish(self.group_id.clone(), bytes.as_slice())
            .await?;

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Json error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Redis error: {0}")]
    RedisError(#[from] RedisError),
    #[error("Tokio error: {0}")]
    TokioReceiveError(#[from] RecvError),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
