use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::{error::UserError, group::Group, state_machine::GroupState, user::User};
use mls_crypto::{identity::normalize_wallet_address, MlsGroupService};

impl User {
    /// Fetch the shared reference to a group by name.
    pub(crate) async fn group_ref(&self, name: &str) -> Result<Arc<RwLock<Group>>, UserError> {
        self.groups
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or(UserError::GroupNotFoundError)
    }

    /// Create a new group for this user.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to create
    /// - `is_creation`: Whether this is a group creation (true) or joining (false)
    ///
    /// ## Effects:
    /// - If `is_creation` is true: Creates MLS group with steward capabilities
    /// - If `is_creation` is false: Creates empty group for later joining
    /// - Adds group to user's groups map
    ///
    /// ## Errors:
    /// - `UserError::GroupAlreadyExistsError` if group already exists
    /// - Various MLS group creation errors
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(group_name) {
            return Err(UserError::GroupAlreadyExistsError);
        }
        let group = if is_creation {
            Group::new(
                group_name,
                true,
                Some(&self.identity_service as &dyn mls_crypto::MlsGroupService),
            )?
        } else {
            Group::new(group_name, false, None)?
        };

        groups.insert(group_name.to_string(), Arc::new(RwLock::new(group)));
        Ok(())
    }

    /// Join a group after receiving an invitation message.
    ///
    /// ## Parameters:
    /// - `invite_bytes`: Serialized MLS invitation message bytes
    ///
    /// ## Effects:
    /// - Creates new MLS group from welcome message
    /// - Sets the MLS group in the user's group instance
    /// - Updates group state to reflect successful joining
    ///
    /// ## Preconditions:
    /// - Group must already exist in user's groups map
    /// - Welcome message must be valid and contain proper group data
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various MLS group creation errors
    pub(crate) async fn join_group(&mut self, invite_bytes: Vec<u8>) -> Result<(), UserError> {
        let (mls_group, group_id) = self
            .identity_service
            .join_group_from_invite(invite_bytes.as_slice())?;
        let group_name = String::from_utf8(group_id)?;

        let group = self.group_ref(&group_name).await?;
        group.write().await.set_mls_group(mls_group)?;

        info!("[join_group]: User joined group {group_name}");
        Ok(())
    }

    /// Get the state of a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the state of
    ///
    /// ## Returns:
    /// - `GroupState` of the group
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        let group = self.group_ref(group_name).await?;
        let state = group.read().await.get_state().await;

        Ok(state)
    }

    /// Get the number of members in a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the number of members of
    ///
    /// ## Returns:
    /// - The number of members in the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn get_group_number_of_members(&self, group_name: &str) -> Result<usize, UserError> {
        let group = self.group_ref(group_name).await?;
        let members = group
            .read()
            .await
            .members_identity(&self.identity_service)
            .await?;
        Ok(members.len())
    }

    /// Retrieve the list of member identities for a group.
    ///
    /// ## Parameters:
    /// - `group_name`: Target group
    ///
    /// ## Returns:
    /// - Vector of normalized wallet addresses (e.g., `0xabc...`)
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group is missing
    pub async fn get_group_members(&self, group_name: &str) -> Result<Vec<String>, UserError> {
        let group = self.group_ref(group_name).await?;
        if !group.read().await.is_mls_group_initialized() {
            return Ok(Vec::new());
        }
        let members = group
            .read()
            .await
            .members_identity(&self.identity_service)
            .await?;
        Ok(members
            .into_iter()
            .map(|raw| normalize_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    /// Get the MLS epoch of a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the MLS epoch of
    ///
    /// ## Returns:
    /// - The MLS epoch of the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn get_group_mls_epoch(&self, group_name: &str) -> Result<u64, UserError> {
        let group = self.group_ref(group_name).await?;
        let epoch = group.read().await.epoch(&self.identity_service).await?;
        Ok(epoch)
    }

    /// Leave a group and clean up associated resources.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to leave
    ///
    /// ## Effects:
    /// - Removes group from user's groups map
    /// - Cleans up all group-related resources
    ///
    /// ## Preconditions:
    /// - Group must exist in user's groups map
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[leave_group]: Leaving group {group_name}");
        let group = self.group_ref(group_name).await?;
        self.groups.write().await.remove(group_name);
        drop(group);
        Ok(())
    }
}
