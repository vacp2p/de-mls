//! The MLS group settings de-mls pins, and the builder integrators seed from.
//!
//! OpenMLS keeps these in `MlsGroupJoinConfig`: they are local, never
//! negotiated, and absent from group state, so every node chooses its own.
//! de-mls pins them instead of leaving them to the integrator, because a node
//! that drops a message its peers processed reaches a different consensus
//! outcome — a badly-set window forks that node out of the group rather than
//! merely degrading it.
//!
//! The values apply on both construction paths so every member runs the same
//! ones. Each path takes the integrator's config builder and stamps the pinned
//! fields on last — [`pin`] on create, [`pin_join`] on the welcome path — so
//! whatever the integrator sets, these values are the ones a group runs.

use openmls::group::{
    MlsGroupCreateConfig, MlsGroupCreateConfigBuilder, MlsGroupJoinConfig,
    MlsGroupJoinConfigBuilder,
};
use openmls::prelude::{
    PURE_CIPHERTEXT_WIRE_FORMAT_POLICY, SenderRatchetConfiguration, WireFormatPolicy,
};

/// Past epochs whose application-message keys are retained, so a message still
/// in flight when a commit lands stays readable afterwards.
///
/// At `0` — OpenMLS's default — every message sent before a commit and
/// delivered after it is lost, which is most of what makes one missed commit
/// unrecoverable. The cost is forward secrecy: a device compromise exposes
/// application messages from this many past epochs instead of only the current
/// one. Commits are unaffected; the window covers application messages only.
pub const PAST_EPOCH_WINDOW: usize = 3;

/// Generations of out-of-order delivery tolerated per sender within an epoch.
///
/// OpenMLS defaults to `5`, which a reordering transport exceeds routinely:
/// twelve messages delivered newest-first decrypt five of twelve at the
/// default. Same forward-secrecy trade as [`PAST_EPOCH_WINDOW`], scoped inside
/// one epoch.
pub const OUT_OF_ORDER_TOLERANCE: u32 = 64;

/// Generations that may be skipped ahead from one sender before a message is
/// refused. OpenMLS's default, restated so the pin is explicit rather than
/// inherited.
pub const MAX_FORWARD_DISTANCE: u32 = 1000;

/// Bytes each outgoing message is padded out to a multiple of.
///
/// At `0` a message's length reveals its class — a commit carrying a welcome is
/// unmistakable beside a chat line on a public topic. Raising it coarsens that
/// channel at the cost of wire size, so it is a privacy decision to make
/// deliberately rather than a reliability one; it is pinned here so the choice
/// applies to every member rather than only the group's creator.
pub const PADDING_SIZE: usize = 0;

/// Resumption PSKs retained. de-mls opens no resumption sessions, so it keeps
/// OpenMLS's default; pinned so both construction paths agree.
pub const RESUMPTION_PSKS: usize = 0;

/// Handshake messages stay encrypted. de-mls reads a message's routing fields
/// from the `PrivateMessage` framing, which a `PublicMessage` does not carry in
/// the same shape.
const WIRE_FORMAT_POLICY: WireFormatPolicy = PURE_CIPHERTEXT_WIRE_FORMAT_POLICY;

/// The pinned ratchet windows.
fn sender_ratchet_configuration() -> SenderRatchetConfiguration {
    SenderRatchetConfiguration::new(OUT_OF_ORDER_TOLERANCE, MAX_FORWARD_DISTANCE)
}

/// Apply the pinned settings to an integrator's join-config builder and build
/// it — [`pin`]'s mirror on the welcome path. Stamping happens last, so
/// whatever the builder carries, the pinned fields are de-mls's.
/// `join_and_create_paths_agree` holds the two paths equal.
pub(crate) fn pin_join(builder: MlsGroupJoinConfigBuilder) -> MlsGroupJoinConfig {
    builder
        .wire_format_policy(WIRE_FORMAT_POLICY)
        .padding_size(PADDING_SIZE)
        .max_past_epochs(PAST_EPOCH_WINDOW)
        .number_of_resumption_psks(RESUMPTION_PSKS)
        .use_ratchet_tree_extension(true)
        .sender_ratchet_configuration(sender_ratchet_configuration())
        .build()
}

/// Apply the pinned settings to an integrator's create-config builder and
/// build it.
///
/// The integrator sets ciphersuite, capabilities, and extensions; de-mls
/// applies its own fields afterwards, so a group is never created with
/// settings that would strand its creator. Taking the builder rather than a
/// finished config is what makes that possible: `MlsGroupCreateConfig` exposes
/// no getter for capabilities or leaf-node extensions, so a supplied config
/// could not be rebuilt without silently dropping them.
pub(crate) fn pin(builder: MlsGroupCreateConfigBuilder) -> MlsGroupCreateConfig {
    builder
        .wire_format_policy(WIRE_FORMAT_POLICY)
        .padding_size(PADDING_SIZE)
        .max_past_epochs(PAST_EPOCH_WINDOW)
        .number_of_resumption_psks(RESUMPTION_PSKS)
        .use_ratchet_tree_extension(true)
        .sender_ratchet_configuration(sender_ratchet_configuration())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pins_are_applied_to_a_bare_builder() {
        let config = pin(MlsGroupCreateConfig::builder());
        assert_eq!(config.max_past_epochs(), PAST_EPOCH_WINDOW);
        assert_eq!(
            config.sender_ratchet_configuration(),
            &sender_ratchet_configuration()
        );
        assert!(config.use_ratchet_tree_extension());
    }

    /// The point of taking the builder: whatever the integrator sets for these
    /// fields, de-mls's values are the ones the group is created with.
    #[test]
    fn the_pins_override_an_integrator_that_sets_them() {
        let config = pin(MlsGroupCreateConfig::builder()
            .use_ratchet_tree_extension(false)
            .max_past_epochs(0)
            .sender_ratchet_configuration(SenderRatchetConfiguration::new(1, 2)));
        assert_eq!(config.max_past_epochs(), PAST_EPOCH_WINDOW);
        assert_eq!(
            config.sender_ratchet_configuration(),
            &sender_ratchet_configuration()
        );
        assert!(config.use_ratchet_tree_extension());
    }

    /// A group's creator and its joiners must run identical local settings, so
    /// the two paths are compared whole rather than field by field — a field
    /// added to `MlsGroupJoinConfig` and pinned on only one path fails here.
    #[test]
    fn join_and_create_paths_agree() {
        assert_eq!(
            pin(MlsGroupCreateConfig::builder()).join_config(),
            &pin_join(MlsGroupJoinConfig::builder())
        );
    }

    /// The create path takes the integrator's builder, so every pinned field
    /// has to survive an integrator that sets it. Compared whole, as above.
    #[test]
    fn the_pins_survive_an_integrator_setting_every_one_of_them() {
        let meddled = MlsGroupCreateConfig::builder()
            .wire_format_policy(openmls::prelude::MIXED_PLAINTEXT_WIRE_FORMAT_POLICY)
            .padding_size(PADDING_SIZE + 128)
            .max_past_epochs(0)
            .number_of_resumption_psks(RESUMPTION_PSKS + 5)
            .use_ratchet_tree_extension(false)
            .sender_ratchet_configuration(SenderRatchetConfiguration::new(1, 2));
        assert_eq!(
            pin(meddled).join_config(),
            &pin_join(MlsGroupJoinConfig::builder())
        );
    }

    /// The join path mirrors the create path: the pinned fields survive an
    /// integrator that sets every one of them on the join builder too.
    #[test]
    fn the_join_pins_survive_an_integrator_setting_every_one_of_them() {
        let meddled = MlsGroupJoinConfig::builder()
            .wire_format_policy(openmls::prelude::MIXED_PLAINTEXT_WIRE_FORMAT_POLICY)
            .padding_size(PADDING_SIZE + 128)
            .max_past_epochs(0)
            .number_of_resumption_psks(RESUMPTION_PSKS + 5)
            .use_ratchet_tree_extension(false)
            .sender_ratchet_configuration(SenderRatchetConfiguration::new(1, 2));
        assert_eq!(pin_join(meddled), pin_join(MlsGroupJoinConfig::builder()));
    }
}
