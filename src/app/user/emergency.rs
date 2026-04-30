//! Emergency Criteria Proposal (ECP) post-consensus policy.
//!
//! Given a finalised ECP outcome, derives the peer-score deltas that the
//! app should apply:
//! - accepted target-bearing emergency → target penalty + creator reward.
//! - accepted `SCORE_BELOW_THRESHOLD` or `DEADLOCK` → creator reward only.
//! - rejected emergency → creator penalty.
//!
//! Consensus (`crate::core::consensus`) handles the queue-level state
//! transition; this module owns the scoring policy that follows.

use prost::Message;

use crate::core::{ScoreEvent, ScoreOp};
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, group_update_request::Payload,
};

/// Score ops to apply when an emergency proposal resolves. Returns an
/// empty vector when the payload isn't an ECP or has no evidence.
pub fn emergency_score_ops(payload: &[u8], approved: bool) -> Vec<ScoreOp> {
    let Ok(req) = GroupUpdateRequest::decode(payload) else {
        return Vec::new();
    };
    let Some(Payload::EmergencyCriteria(ec)) = req.payload else {
        return Vec::new();
    };
    let Some(evidence) = ec.evidence else {
        return Vec::new();
    };

    if approved {
        let mut ops = vec![creator_reward(&evidence)];
        if let Some(target_op) = evidence.target_score_op() {
            ops.push(target_op);
        }
        ops
    } else {
        vec![creator_penalty(&evidence)]
    }
}

fn creator_reward(ev: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: ev.creator_member_id.clone(),
        event: ScoreEvent::EmergencyYesCreator,
    }
}

fn creator_penalty(ev: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: ev.creator_member_id.clone(),
        event: ScoreEvent::EmergencyNoCreator,
    }
}
