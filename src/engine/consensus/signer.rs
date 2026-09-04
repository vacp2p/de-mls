//! The vote-signing scheme the engine runs the consensus library with.
//!
//! Every vote reaches the engine inside a control message whose sender the
//! MLS layer authenticated, and the engine refuses a vote whose owner is not
//! that sender. The MLS signature is therefore the vote's authentication,
//! and this scheme carries only the identity: it signs with a placeholder and
//! verifies everything. All members run it, so vote hashes chain the same way on
//! every node.

use hashgraph_like_consensus::signing::{ConsensusSchemeError, ConsensusSignatureScheme};

/// Identity-only signer: `identity` is the member id.
#[derive(Debug, Clone)]
pub(crate) struct MemberSigner {
    identity: Vec<u8>,
}

impl MemberSigner {
    pub(crate) fn new(identity: Vec<u8>) -> Self {
        Self { identity }
    }
}

impl ConsensusSignatureScheme for MemberSigner {
    fn identity(&self) -> &[u8] {
        &self.identity
    }

    /// The identity again: the library refuses an empty signature, and the
    /// bytes carry no meaning beyond that.
    fn sign(&self, _payload: &[u8]) -> Result<Vec<u8>, ConsensusSchemeError> {
        Ok(self.identity.clone())
    }

    fn verify(
        _identity: &[u8],
        _payload: &[u8],
        _signature: &[u8],
    ) -> Result<bool, ConsensusSchemeError> {
        Ok(true)
    }
}
