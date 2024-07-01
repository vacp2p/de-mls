use fred::{
    clients::{RedisClient, SubscriberClient},
    error::RedisError,
    prelude::*,
    types::Message,
};

use tokio::sync::broadcast::{error::RecvError, Receiver};

use openmls::{
    framing::{MlsMessageIn, MlsMessageOut},
    prelude::{TlsDeserializeTrait, TlsSerializeTrait},
};
// use waku_bindings::*;

pub struct RClient {
    group_id: String,
    client: RedisClient,
    sub_client: SubscriberClient,
    broadcaster: Receiver<Message>,
}

impl RClient {
    pub async fn new_for_group(group_id: String) -> Result<Self, DeliveryServiceError> {
        let redis_client = RedisClient::default();
        let subscriber: SubscriberClient =
            Builder::default_centralized().build_subscriber_client()?;
        redis_client.init().await?;
        subscriber.init().await?;
        subscriber.subscribe(group_id.clone()).await?;
        Ok(RClient {
            group_id,
            client: redis_client,
            sub_client: subscriber.clone(),
            broadcaster: subscriber.message_rx(),
        })
    }

    pub async fn remove_from_group(&mut self) -> Result<(), DeliveryServiceError> {
        self.sub_client.unsubscribe(self.group_id.clone()).await?;
        self.sub_client.quit().await?;
        self.client.quit().await?;
        Ok(())
    }

    pub async fn msg_send(&mut self, msg: MlsMessageOut) -> Result<(), DeliveryServiceError> {
        let buf = msg.tls_serialize_detached().unwrap();
        self.client
            .publish(self.group_id.clone(), buf.as_slice())
            .await?;

        Ok(())
    }

    pub async fn msg_recv(&mut self) -> Result<MlsMessageIn, DeliveryServiceError> {
        // check only one message
        let msg = self.broadcaster.recv().await?;
        let bytes: Vec<u8> = msg.value.convert()?;
        let res = MlsMessageIn::tls_deserialize_bytes(bytes).unwrap();
        Ok(res)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Redis error: {0}")]
    RedisError(#[from] RedisError),
    #[error("Tokyo error: {0}")]
    TokyoRecieveError(#[from] RecvError),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
}
