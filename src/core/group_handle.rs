//! GroupHandle - per-group state container.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::protos::de_mls::messages::v1::{RequestType, UpdateRequest};
use mls_crypto::{normalize_wallet_address_str, MlsGroupHandle};

use super::steward::Steward;
use super::types::GroupUpdateRequest;

/// Handle for a single MLS group.
///
/// The `GroupHandle` contains all state needed for group operations:
/// - MLS group handle for cryptographic operations
/// - Application ID for message routing
/// - Optional steward for proposal management
/// - Key package sharing status
///
/// # Thread Safety
///
/// The `GroupHandle` is designed to be used with external synchronization.
/// The MLS group handle is wrapped in an `Arc<Mutex>` for safe concurrent access.
#[derive(Clone, Debug)]
pub struct GroupHandle {
    /// The name of the group.
    group_name: String,
    /// The MLS group handle (None until group is joined/created).
    mls_handle: Option<Arc<Mutex<MlsGroupHandle>>>,
    /// Unique application instance ID for message deduplication.
    app_id: Vec<u8>,
    /// Optional steward for this group (per-group, can change dynamically).
    steward: Option<Steward>,
}

impl GroupHandle {
    /// Create a new group handle for an existing group (joining).
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    pub fn new_for_join(group_name: &str) -> Self {
        Self {
            group_name: group_name.to_string(),
            mls_handle: None,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: None,
        }
    }

    /// Create a new group handle for creating a new group (as steward).
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `mls_handle` - The MLS group handle
    pub fn new_as_creator(group_name: &str, mls_handle: MlsGroupHandle) -> Self {
        Self {
            group_name: group_name.to_string(),
            mls_handle: Some(Arc::new(Mutex::new(mls_handle))),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: Some(Steward::new()),
        }
    }

    /// Get the group name.
    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    /// Get the group name as bytes.
    pub fn group_name_bytes(&self) -> &[u8] {
        self.group_name.as_bytes()
    }

    /// Get the application ID.
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Check if this group has a steward.
    pub fn is_steward(&self) -> bool {
        self.steward.is_some()
    }

    /// Check if the MLS group is initialized.
    pub fn is_mls_initialized(&self) -> bool {
        self.mls_handle.is_some()
    }

    /// Get a reference to the MLS handle.
    pub fn mls_handle(&self) -> Option<&Arc<Mutex<MlsGroupHandle>>> {
        self.mls_handle.as_ref()
    }

    /// Set the MLS group handle (used when joining a group).
    pub fn set_mls_handle(&mut self, handle: MlsGroupHandle) {
        self.mls_handle = Some(Arc::new(Mutex::new(handle)));
    }

    /// Become the steward of this group.
    pub fn become_steward(&mut self) {
        if self.steward.is_none() {
            self.steward = Some(Steward::new());
        }
    }

    /// Resign as steward of this group.
    pub fn resign_steward(&mut self) {
        self.steward = None;
    }

    /// Get a reference to the steward (if any).
    pub fn steward(&self) -> Option<&Steward> {
        self.steward.as_ref()
    }

    /// Get a mutable reference to the steward (if any).
    pub fn steward_mut(&mut self) -> Option<&mut Steward> {
        self.steward.as_mut()
    }

    // ─────────────────────────── Steward Operations ───────────────────────────

    /// Store an add member proposal (steward only).
    ///
    /// # Arguments
    /// * `key_package` - The key package of the member to add
    ///
    /// # Returns
    /// The update request if steward, None otherwise.
    pub fn store_add_proposal(
        &mut self,
        key_package: mls_crypto::KeyPackageBytes,
    ) -> Option<UpdateRequest> {
        if let Some(steward) = &mut self.steward {
            let wallet_bytes = key_package.identity_bytes().to_vec();
            steward.add_proposal(GroupUpdateRequest::AddMember(key_package));
            Some(UpdateRequest {
                request_type: RequestType::AddMember as i32,
                wallet_address: wallet_bytes,
            })
        } else {
            None
        }
    }

    /// Store a remove member proposal (steward only).
    ///
    /// # Arguments
    /// * `identity` - The wallet address of the member to remove
    ///
    /// # Returns
    /// The update request if steward, None otherwise.
    pub fn store_remove_proposal(&mut self, identity: String) -> Option<UpdateRequest> {
        if let Some(steward) = &mut self.steward {
            let normalized =
                normalize_wallet_address_str(&identity).unwrap_or_else(|_| identity.clone());
            let wallet_bytes =
                alloy::hex::decode(normalized.strip_prefix("0x").unwrap_or(&normalized))
                    .unwrap_or_default();

            steward.add_proposal(GroupUpdateRequest::RemoveMember(normalized));
            Some(UpdateRequest {
                request_type: RequestType::RemoveMember as i32,
                wallet_address: wallet_bytes,
            })
        } else {
            None
        }
    }

    /// Get the count of approved proposals.
    pub fn approved_proposals_count(&self) -> usize {
        self.steward
            .as_ref()
            .map(|s| s.approved_proposals_count())
            .unwrap_or(0)
    }

    /// Get the approved proposals.
    pub fn approved_proposals(&self) -> Vec<GroupUpdateRequest> {
        self.steward
            .as_ref()
            .map(|s| s.approved_proposals())
            .unwrap_or_default()
    }

    /// Start a new voting epoch.
    ///
    /// # Returns
    /// The number of proposals moved to voting, or None if not steward.
    pub fn start_voting_epoch(&mut self) -> Option<usize> {
        self.steward.as_mut().map(|s| s.start_voting_epoch())
    }

    /// Get the count of voting proposals.
    pub fn voting_proposals_count(&self) -> usize {
        self.steward
            .as_ref()
            .map(|s| s.voting_proposals_count())
            .unwrap_or(0)
    }

    /// Get the voting proposals.
    pub fn voting_proposals(&self) -> Vec<GroupUpdateRequest> {
        self.steward
            .as_ref()
            .map(|s| s.voting_proposals())
            .unwrap_or_default()
    }

    /// Complete voting and clear voting proposals.
    pub fn complete_voting(&mut self) {
        if let Some(steward) = &mut self.steward {
            steward.complete_voting();
        }
    }
}
