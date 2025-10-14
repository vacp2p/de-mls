use alloy::signers::local::PrivateKeySigner;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{str::FromStr, sync::Arc};

use crate::user::User;
use crate::user_actor::ConsensusEventMessage;
use crate::LocalSigner;
use crate::{error::UserError, AppState};

pub const STEWARD_EPOCH: u64 = 15;

pub async fn create_user_instance(
    eth_private_key: String,
    app_state: Arc<AppState>,
) -> Result<(ActorRef<User>, String), UserError> {
    let signer = PrivateKeySigner::from_str(&eth_private_key)?;
    let user_address = signer.address_string();
    // Create user
    let user = User::new(&eth_private_key)?;

    // Set up consensus event forwarding before spawning the actor
    let consensus_events = user.subscribe_to_consensus_events();

    let user_ref = kameo::spawn(user);
    let app_state_consensus = app_state.clone();
    let user_ref_consensus = user_ref.clone();
    let mut consensus_events_receiver = consensus_events;
    tokio::spawn(async move {
        info!("Starting consensus event forwarding loop (user-only)");
        while let Ok((group_name, event)) = consensus_events_receiver.recv().await {
            let result = user_ref_consensus
                .ask(ConsensusEventMessage {
                    group_name: group_name.clone(),
                    event,
                })
                .await;

            match result {
                Ok(commit_messages) => {
                    if !commit_messages.is_empty() {
                        info!(
                            "Sending {} commit messages to Waku for group {}",
                            commit_messages.len(),
                            group_name
                        );
                        for msg in commit_messages {
                            if let Err(e) = app_state_consensus.waku_node.send(msg).await {
                                error!("Error sending commit message to Waku: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error forwarding consensus event: {e}");
                }
            }
        }
        info!("Consensus forwarding loop ended");
    });

    Ok((user_ref, user_address))
}
