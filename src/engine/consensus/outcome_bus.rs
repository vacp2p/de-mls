//! de-mls's per-conversation consensus-outcome channel.
//!
//! The consensus service publishes every resolved outcome into an
//! [`OutcomeBus`]; the conversation drains the paired [`OutcomeReceiver`] once
//! per poll. One queue fans in the outcomes of all the conversation's
//! concurrent sessions, so a single drain loop handles them regardless of which
//! call (vote, timeout, bundled YES) resolved which proposal.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use hashgraph_like_consensus::{events::ConsensusEventBus, scope::ScopeID, types::ConsensusEvent};

/// Publish end held by the consensus service. Cloning shares the same queue.
#[derive(Clone, Default)]
pub(crate) struct OutcomeBus {
    queue: Arc<Mutex<VecDeque<(ScopeID, ConsensusEvent)>>>,
}

/// Drain end held by the conversation. Popped in `tick_deadlines`.
pub(crate) struct OutcomeReceiver {
    queue: Arc<Mutex<VecDeque<(ScopeID, ConsensusEvent)>>>,
}

impl ConsensusEventBus<ScopeID> for OutcomeBus {
    type Receiver = OutcomeReceiver;

    fn subscribe(&self) -> Self::Receiver {
        OutcomeReceiver {
            queue: Arc::clone(&self.queue),
        }
    }

    fn publish(&self, scope: ScopeID, event: ConsensusEvent) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back((scope, event));
        }
    }
}

impl OutcomeReceiver {
    /// Pop the next resolved outcome, or `None` when the queue is empty.
    pub(crate) fn try_recv(&mut self) -> Option<(ScopeID, ConsensusEvent)> {
        self.queue.lock().ok()?.pop_front()
    }
}
