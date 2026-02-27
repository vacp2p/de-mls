use super::*;

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // ══════════════════════════════════════════════════════════
    // Groups
    // ══════════════════════════════════════════════════════════

    /// Create a new MLS group.
    ///
    /// The group name becomes the MLS group ID. The creator becomes
    /// the only member and is typically the steward.
    pub fn create_group(&self, group_id: &str) -> Result<()> {
        let guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let config = MlsGroupCreateConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let group = MlsGroup::new_with_group_id(
            &provider,
            &identity.signer,
            &config,
            GroupId::from_slice(group_id.as_bytes()),
            identity.credential.clone(),
        )?;

        self.groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .insert(group_id.to_string(), group);

        Ok(())
    }

    /// Join a group from a welcome message.
    ///
    /// Returns the group ID on success. The welcome must be for us
    /// (contain one of our key package references).
    pub fn join_group(&self, welcome_bytes: &[u8]) -> Result<String> {
        let provider = self.make_provider();

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Err(MlsError::Service(MlsServiceError::UnexpectedMessageType)),
        };

        // Check if this welcome is for us
        let is_for_us = welcome.secrets().iter().any(|s| {
            self.storage
                .is_our_key_package(s.new_member().as_slice())
                .unwrap_or(false)
        });
        if !is_for_us {
            return Err(MlsError::Service(MlsServiceError::WelcomeNotForUs));
        }

        // Remove used key package references
        for secret in welcome.secrets() {
            let _ = self
                .storage
                .remove_key_package_ref(secret.new_member().as_slice());
        }

        let config = MlsGroupJoinConfig::builder().build();
        let group = StagedWelcome::new_from_welcome(&provider, &config, welcome, None)?
            .into_group(&provider)?;

        let group_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();

        self.groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .insert(group_id.clone(), group);

        Ok(group_id)
    }

    /// Check if a welcome message is for us (without joining).
    ///
    /// Returns true if the welcome contains one of our key package references.
    pub fn is_welcome_for_us(&self, welcome_bytes: &[u8]) -> Result<bool> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(false),
        };

        Ok(welcome.secrets().iter().any(|s| {
            self.storage
                .is_our_key_package(s.new_member().as_slice())
                .unwrap_or(false)
        }))
    }

    /// Get all current group members as wallet addresses.
    pub fn members(&self, group_id: &str) -> Result<Vec<Vec<u8>>> {
        let groups = self
            .groups
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        Ok(group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }
}
