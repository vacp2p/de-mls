use fred::{
    clients::{RedisClient, SubscriberClient},
    prelude::*,
    types::Message,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Receiver;

use crate::DeliveryServiceError;
// use waku_bindings::*;

pub struct RClient {
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
    pub async fn new_with_group(
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
                client: redis_client,
                sub_client: subscriber.clone(),
            },
            subscriber.message_rx(),
        ))
    }

    pub async fn remove_group(&mut self, group_id: String) -> Result<(), DeliveryServiceError> {
        self.sub_client.unsubscribe(group_id).await?;
        Ok(())
    }

    pub async fn msg_send(
        &mut self,
        msg: Vec<u8>,
        sender: String,
        group_id: String,
    ) -> Result<(), DeliveryServiceError> {
        let json_value = SenderStruct { sender, msg };
        let bytes = serde_json::to_vec(&json_value)?;
        self.client.publish(group_id, bytes.as_slice()).await?;

        Ok(())
    }
}
