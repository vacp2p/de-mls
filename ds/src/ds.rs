use fred::{
    clients::{RedisClient, SubscriberClient},
    error::RedisError,
    prelude::*,
    types::Message,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{
    broadcast::Receiver,
    mpsc::{self, error::SendError, UnboundedReceiver, UnboundedSender},
};
use tokio_util::sync::CancellationToken;

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

pub struct RClientPrivate {
    chat_id: String,
    sender_id: String,
    token: CancellationToken,
    sender: UnboundedSender<Vec<u8>>,
    reciever: UnboundedReceiver<Vec<u8>>,
}

impl RClientPrivate {
    pub async fn new_for_users(
        chat_id: String,
        sender_id: String,
    ) -> Result<Self, DeliveryServiceError> {
        let redis_client = RedisClient::default();
        let subscriber: SubscriberClient =
            Builder::default_centralized().build_subscriber_client()?;
        redis_client.init().await?;
        subscriber.init().await?;
        subscriber.subscribe(chat_id.clone()).await?;

        let (sender_publish, mut receiver_publish) = mpsc::unbounded_channel::<Vec<u8>>();
        let (sender_subscriber, receiver_subscriber) = mpsc::unbounded_channel::<Vec<u8>>();

        let chat_id_c = chat_id.clone();
        let redis_client_c = redis_client.clone();
        tokio::spawn(async move {
            while let Some(msg) = receiver_publish.recv().await {
                redis_client_c
                    .publish(chat_id_c.to_owned(), msg.as_slice())
                    .await?;
            }
            Ok::<_, DeliveryServiceError>(())
        });
        let subscriber_c = subscriber.clone();
        tokio::spawn(async move {
            let mut ss = subscriber_c.message_rx();
            while let Ok(msg) = ss.recv().await {
                let bytes: Vec<u8> = msg.value.convert()?;
                if let Err(e) = sender_subscriber.send(bytes) {
                    return Err(DeliveryServiceError::TokioSendError(e));
                }
            }
            Ok(())
        });

        let token = CancellationToken::new();

        let cloned_token = token.clone();
        let chat_id_c = chat_id.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            subscriber.unsubscribe(chat_id_c).await?;
            subscriber.quit().await?;
            redis_client.quit().await?;
            Ok::<_, DeliveryServiceError>(())
        });

        Ok(RClientPrivate {
            chat_id: chat_id.clone(),
            sender_id,
            token: cloned_token,
            sender: sender_publish,
            reciever: receiver_subscriber,
        })
    }

    pub fn msg_send(&mut self, msg: Vec<u8>) -> Result<(), DeliveryServiceError> {
        let json_value = SenderStruct {
            sender: self.sender_id.clone(),
            msg,
        };
        let bytes = serde_json::to_vec(&json_value)?;
        self.sender.send(bytes)?;
        Ok(())
    }

    pub async fn msg_recv(&mut self) -> Result<Vec<u8>, DeliveryServiceError> {
        while let Some(msg) = self.reciever.recv().await {
            let m: SenderStruct = serde_json::from_slice(&msg)?;
            if m.sender == self.sender_id {
                continue;
            }
            return Ok(m.msg);
        }
        Err(DeliveryServiceError::EmptyMsgError)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Empty msg")]
    EmptyMsgError,
    #[error("Json error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Redis error: {0}")]
    RedisError(#[from] RedisError),
    #[error("Channel sender error: {0}")]
    TokioSendError(#[from] SendError<Vec<u8>>),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}

#[tokio::test]
async fn private_test() {
    let mut alice = RClientPrivate::new_for_users("alicebob".to_string(), "alice".to_string())
        .await
        .unwrap();
    let mut bob = RClientPrivate::new_for_users("alicebob".to_string(), "bob".to_string())
        .await
        .unwrap();

    alice.msg_send("Hi bob".as_bytes().to_vec()).unwrap();
    let msg = bob.msg_recv().await.unwrap();
    assert_eq!("Hi bob".to_string(), String::from_utf8(msg).unwrap());

    bob.msg_send("Hi alice".as_bytes().to_vec()).unwrap();
    let msg = alice.msg_recv().await.unwrap();
    assert_eq!("Hi alice".to_string(), String::from_utf8(msg).unwrap());
}
