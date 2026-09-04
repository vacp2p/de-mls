//! The MLS group settings the contract pins on both construction paths.
//!
//! These are local OpenMLS settings, never negotiated — but a node that drops
//! a message its peers processed can diverge on the protocol above, so they
//! are pinned rather than left to each deployment. [`pin`] (create) and
//! [`pin_join`] (welcome) stamp them onto the application's builder last, so
//! they always win. Ciphersuite, capabilities and group-context extensions
//! stay the application's.

use openmls::group::{
    MlsGroupCreateConfig, MlsGroupCreateConfigBuilder, MlsGroupJoinConfig,
    MlsGroupJoinConfigBuilder,
};
use openmls::prelude::{
    PURE_CIPHERTEXT_WIRE_FORMAT_POLICY, SenderRatchetConfiguration, WireFormatPolicy,
};

/// Past epochs whose application-message keys are retained, so a message in
/// flight when a commit lands stays readable (OpenMLS default: 0 — lost).
/// Cost: a device compromise exposes this many past epochs of app messages.
pub const PAST_EPOCH_WINDOW: usize = 3;

/// Generations of out-of-order delivery tolerated per sender within an epoch
/// (OpenMLS default: 5 — too small for a reordering transport). Same
/// forward-secrecy trade as [`PAST_EPOCH_WINDOW`], scoped inside one epoch.
pub const OUT_OF_ORDER_TOLERANCE: u32 = 64;

/// Generations that may be skipped ahead from one sender (OpenMLS's default,
/// pinned explicitly).
pub const MAX_FORWARD_DISTANCE: u32 = 1000;

/// Bytes each outgoing message is padded to a multiple of. At 0 length leaks
/// the message class; raising it is a privacy/size trade, pinned here so the
/// choice is group-wide.
pub const PADDING_SIZE: usize = 0;

/// Resumption PSKs retained — unused by the protocol; pinned so both paths
/// agree.
pub const RESUMPTION_PSKS: usize = 0;

/// Application messages are pure ciphertext. Commits travel in the candidate
/// envelope, which carries them in the clear either way.
const WIRE_FORMAT_POLICY: WireFormatPolicy = PURE_CIPHERTEXT_WIRE_FORMAT_POLICY;

/// The pinned ratchet windows.
fn sender_ratchet_configuration() -> SenderRatchetConfiguration {
    SenderRatchetConfiguration::new(OUT_OF_ORDER_TOLERANCE, MAX_FORWARD_DISTANCE)
}

/// [`pin`]'s mirror on the welcome path: stamp the pinned settings onto the
/// application's join-config builder, last-write-wins.
pub fn pin_join(builder: MlsGroupJoinConfigBuilder) -> MlsGroupJoinConfig {
    builder
        .wire_format_policy(WIRE_FORMAT_POLICY)
        .padding_size(PADDING_SIZE)
        .max_past_epochs(PAST_EPOCH_WINDOW)
        .number_of_resumption_psks(RESUMPTION_PSKS)
        .use_ratchet_tree_extension(true)
        .sender_ratchet_configuration(sender_ratchet_configuration())
        .build()
}

/// Stamp the pinned settings onto the application's create-config builder
/// (ciphersuite, capabilities, extensions stay theirs) and build. Takes the
/// builder because a finished `MlsGroupCreateConfig` has no getters for
/// capabilities or leaf-node extensions and could not be rebuilt losslessly.
pub fn pin(builder: MlsGroupCreateConfigBuilder) -> MlsGroupCreateConfig {
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

    /// The point of taking the builder: whatever the application sets for
    /// these fields, the pinned values are the ones the group is created with.
    #[test]
    fn the_pins_override_an_application_that_sets_them() {
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

    /// The two paths compared whole: a field pinned on only one of them
    /// fails here.
    #[test]
    fn join_and_create_paths_agree() {
        assert_eq!(
            pin(MlsGroupCreateConfig::builder()).join_config(),
            &pin_join(MlsGroupJoinConfig::builder())
        );
    }

    /// Every pinned field survives an application that sets it.
    #[test]
    fn the_pins_survive_an_application_setting_every_one_of_them() {
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

    /// Same on the join builder.
    #[test]
    fn the_join_pins_survive_an_application_setting_every_one_of_them() {
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
