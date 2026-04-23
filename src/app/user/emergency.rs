//! Emergency Criteria Proposal (ECP) post-consensus policy.
//!
//! Given a finalised ECP outcome, derives the peer-score deltas that the
//! app should apply:
//! - accepted non-`SCORE_BELOW_THRESHOLD` emergency → target penalty + creator reward
//! - accepted `SCORE_BELOW_THRESHOLD` → creator reward only (target is being removed)
//! - rejected emergency → creator penalty (false accusation)
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
        if is_score_below_threshold(&evidence) {
            vec![creator_reward(&evidence)]
        } else {
            vec![target_penalty(&evidence), creator_reward(&evidence)]
        }
    } else {
        vec![creator_penalty(&evidence)]
    }
}

fn is_score_below_threshold(ev: &ViolationEvidence) -> bool {
    ViolationType::try_from(ev.violation_type) == Ok(ViolationType::ScoreBelowThreshold)
}

/// `ScoreBelowThreshold` is filtered upstream (target is being removed),
/// so only the other three penalty kinds reach this function. Unknown
/// wire values fall back to the harshest penalty.
fn target_penalty(ev: &ViolationEvidence) -> ScoreOp {
    let event = match ViolationType::try_from(ev.violation_type) {
        Ok(ViolationType::BrokenCommit) => ScoreEvent::BrokenCommit,
        Ok(ViolationType::BrokenMlsProposal) => ScoreEvent::BrokenMlsProposal,
        Ok(ViolationType::CensorshipInactivity) => ScoreEvent::CensorshipInactivity,
        _ => ScoreEvent::BrokenCommit,
    };
    ScoreOp {
        member_id: ev.target_member_id.clone(),
        event,
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
