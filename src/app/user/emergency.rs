//! Emergency Criteria Proposal (ECP) post-consensus policy.
//!
//! Given a finalised ECP outcome, derives the peer-score deltas that the
//! app should apply:
//! - accepted target-bearing emergency (e.g. broken commit) → target
//!   penalty + creator reward.
//! - accepted `SCORE_BELOW_THRESHOLD` → creator reward only (target is
//!   already being removed via the urgent commit).
//! - accepted `DEADLOCK` (Layer 3) → creator reward only (no target;
//!   the recovery itself is the action).
//! - rejected emergency → creator penalty (anti-abuse).
//!
//! Consensus (`crate::core::consensus`) handles the queue-level state
//! transition; this module owns the scoring policy that follows.

use prost::Message;

use crate::core::{ScoreEvent, ScoreOp};
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, ViolationType, group_update_request::Payload,
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
        if is_targetless_or_self_executing(&evidence) {
            // ScoreBelowThreshold: target is being removed via urgent commit.
            // Deadlock: no target; recovery is its own action.
            vec![creator_reward(&evidence)]
        } else {
            vec![evidence.target_score_op(), creator_reward(&evidence)]
        }
    } else {
        vec![creator_penalty(&evidence)]
    }
}

fn is_targetless_or_self_executing(ev: &ViolationEvidence) -> bool {
    matches!(
        ViolationType::try_from(ev.violation_type),
        Ok(ViolationType::ScoreBelowThreshold) | Ok(ViolationType::Deadlock)
    )
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
