//! The reference router from the design, over the fake group, plus the
//! multi-node bed that drives several of them on virtual time.

use std::{collections::HashMap, time::Duration};

use de_mls::EngineConfig;
use de_mls::engine::{
    CommitHash, Decision, DecisionFailure, Engine, Event, InMemoryStore, MemberId, Outbound,
    Output, Phase, StagedFacts, Timestamp,
};

use crate::common::{
    fake_mls::{FakeMls, Kind, WelcomeDraft},
    net::{Frame, Net, Welcome},
};

/// One node: its group, its engine, and the bookkeeping the router contract
/// asks for.
pub struct FakeRouter {
    pub mls: FakeMls,
    pub engine: Engine<InMemoryStore>,
    welcomes: HashMap<CommitHash, WelcomeDraft>,
    outbox: Vec<Frame>,
    /// Every event the engine returned, in order.
    pub events: Vec<Event>,
    /// Every decision the engine returned, in order.
    pub decisions: Vec<Decision>,
    /// Chat the application received: `(sender, text)`.
    pub chats: Vec<(MemberId, Vec<u8>)>,
    next_wakeup: Option<Timestamp>,
    /// Set once `Decision::Leave` ran.
    pub left: bool,
    /// Announcements the router holds because `propose_add` refused with
    /// `ConversationBlocked` — retried once an `Event::PhaseChange(Phase::Working)`
    /// is drained. The engine keeps no memory of these; the router does.
    pending_announcements: Vec<(MemberId, Vec<u8>)>,
}

impl FakeRouter {
    /// Creator side.
    pub fn create(
        now: Timestamp,
        conversation_id: &str,
        own: MemberId,
        config: EngineConfig,
    ) -> Self {
        let mls = FakeMls::create(conversation_id, own.clone());
        let (engine, out) = Engine::create(
            conversation_id,
            own,
            mls.epoch(),
            &mls.members(),
            config,
            InMemoryStore::default(),
        )
        .expect("engine create");
        let mut router = Self::over(mls, engine);
        router.drive(now, out);
        router
    }

    /// Joiner side: adopt the welcome and start the engine unsynced. It
    /// adopts the first `ConversationSync` a steward broadcasts.
    pub fn join(now: Timestamp, own: MemberId, welcome: &Welcome, config: EngineConfig) -> Self {
        let mls = FakeMls::from_welcome(own.clone(), &welcome.snapshot);
        let (engine, out) = Engine::join(
            mls.conversation_id(),
            own,
            mls.epoch(),
            &mls.members(),
            config,
            InMemoryStore::default(),
        )
        .expect("engine join");
        let mut router = Self::over(mls, engine);
        router.drive(now, out);
        router
    }

    fn over(mls: FakeMls, engine: Engine<InMemoryStore>) -> Self {
        Self {
            mls,
            engine,
            welcomes: HashMap::new(),
            outbox: Vec::new(),
            events: Vec::new(),
            decisions: Vec::new(),
            chats: Vec::new(),
            next_wakeup: None,
            left: false,
            pending_announcements: Vec::new(),
        }
    }

    pub fn own_id(&self) -> &MemberId {
        self.mls.own_id()
    }

    /// Announce a key package for `member` (router contract rule 7b): hold
    /// it and propose it right away. A `ConversationBlocked` refusal (a
    /// commit round is open) keeps it parked until this router drains
    /// `Event::PhaseChange(Phase::Working)`.
    pub fn announce(&mut self, now: Timestamp, member: MemberId, key_package: Vec<u8>) {
        self.pending_announcements.push((member, key_package));
        let out = self.propose_announced(now);
        self.drive(now, out);
    }

    /// `propose_add` for every held announcement; one refused because a
    /// round is open stays held.
    fn propose_announced(&mut self, now: Timestamp) -> Output {
        let mut out = Output::default();
        for (member, key_package) in std::mem::take(&mut self.pending_announcements) {
            match self
                .engine
                .propose_add(now, member.clone(), key_package.clone())
            {
                Ok(o) => out.merge(o),
                Err(de_mls::ConversationError::ConversationBlocked(_)) => {
                    self.pending_announcements.push((member, key_package));
                }
                Err(e) => panic!("announce: propose_add failed: {e}"),
            }
        }
        out
    }

    /// Restart from this router's own store: the group is unaffected (the
    /// fake bed keeps no separate MLS-side storage to reload), and the
    /// engine is rebuilt fresh from what it last flushed.
    pub fn restart(&mut self, now: Timestamp) {
        let store = self.engine.store().clone();
        self.restart_with(now, store);
    }

    /// Restart with `store` in place of this router's own — a deliberately
    /// stale or empty snapshot, for exercising `Engine::restore`'s recovery
    /// path. Replaces the engine and drives what `Engine::restore` returns.
    pub fn restart_with(&mut self, now: Timestamp, store: InMemoryStore) {
        let (engine, out) = Engine::restore(
            self.mls.conversation_id(),
            self.own_id().clone(),
            self.mls.epoch(),
            &self.mls.members(),
            store,
        )
        .expect("engine restore");
        self.engine = engine;
        self.pending_announcements.clear();
        self.drive(now, out);
    }

    /// Frames produced since the last call, in order.
    pub fn take_outbox(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.outbox)
    }

    pub fn next_wakeup(&self) -> Option<Timestamp> {
        self.next_wakeup
    }

    /// The application sends chat: sealed by the group, never seen by the
    /// engine.
    pub fn send_chat(&mut self, text: &[u8]) {
        let sealed = self.mls.seal(Kind::Chat, text);
        self.outbox.push(Frame::Sealed(sealed));
    }

    /// A local action on the engine, driven to completion.
    pub fn act(
        &mut self,
        now: Timestamp,
        action: impl FnOnce(&mut Engine<InMemoryStore>) -> Result<Output, de_mls::ConversationError>,
    ) -> Result<(), de_mls::ConversationError> {
        let out = action(&mut self.engine)?;
        self.drive(now, out);
        Ok(())
    }

    /// Router contract, inbound.
    pub fn on_frame(&mut self, now: Timestamp, frame: Frame) {
        if self.left {
            return;
        }
        let out = match frame {
            Frame::Sealed(sealed) => {
                let Some(opened) = self.mls.open(&sealed) else {
                    return;
                };
                match opened.kind {
                    Kind::Chat => {
                        self.chats.push((opened.sender, opened.plaintext));
                        return;
                    }
                    Kind::Control => self.engine.handle_control(
                        now,
                        opened.sender,
                        opened.epoch,
                        &opened.plaintext,
                    ),
                }
            }
            Frame::Commit(bytes) => match self.mls.stage(&bytes) {
                Ok(facts) => self
                    .engine
                    .handle_candidate(now, CommitHash::of(&bytes), facts),
                Err(_) => return,
            },
            Frame::Welcome(_) => return,
        };
        self.finish(now, out);
    }

    /// Router contract, time: tick when the armed wakeup is due.
    pub fn on_wakeup(&mut self, now: Timestamp) {
        if self.left {
            return;
        }
        if self.next_wakeup.is_some_and(|due| due <= now) {
            self.next_wakeup = None;
            let out = self.engine.tick(now);
            self.finish(now, out);
        }
    }

    fn finish(&mut self, now: Timestamp, out: Result<Output, de_mls::ConversationError>) {
        match out {
            Ok(out) => self.drive(now, out),
            Err(e) => self.events.push(Event::Error {
                operation: "driving call".into(),
                message: e.to_string(),
            }),
        }
    }

    /// Router contract, `Output`: outbound, then decisions, then events,
    /// looping on the report calls' outputs until one is empty.
    pub fn drive(&mut self, now: Timestamp, mut out: Output) {
        loop {
            for o in out.outbound.drain(..) {
                self.send(o);
            }
            let mut next = Output::default();
            for decision in out.decisions.drain(..) {
                self.decisions.push(decision.clone());
                next.merge(self.execute(now, decision));
            }
            let round_closed = out
                .events
                .iter()
                .any(|e| matches!(e, Event::PhaseChange(Phase::Working)));
            self.events.append(&mut out.events);
            if round_closed {
                next.merge(self.propose_announced(now));
            }
            if let Some(d) = out.wakeup {
                self.next_wakeup = Some(now + d);
            }
            if next.is_empty() {
                if let Some(d) = next.wakeup {
                    self.next_wakeup = Some(now + d);
                }
                break;
            }
            out = next;
        }
    }

    fn send(&mut self, outbound: Outbound) {
        let Outbound::Control(bytes) = outbound;
        let sealed = self.mls.seal(Kind::Control, &bytes);
        self.outbox.push(Frame::Sealed(sealed));
    }

    fn execute(&mut self, now: Timestamp, decision: Decision) -> Output {
        let result = match decision.clone() {
            Decision::ValidateKeyPackage {
                proposal_id,
                member,
                key_package,
            } => {
                let valid = FakeMls::key_package_identity(&key_package).as_ref() == Some(&member);
                self.engine.key_package_checked(now, proposal_id, valid)
            }
            Decision::BuildCommit { actions } => {
                let built = self.mls.build_commit(&actions);
                if let Some(draft) = built.welcome {
                    self.welcomes.insert(built.hash, draft);
                }
                self.outbox.push(Frame::Commit(built.commit));
                self.engine.handle_candidate(
                    now,
                    built.hash,
                    StagedFacts {
                        sender: self.mls.own_id().clone(),
                        epoch: self.mls.epoch(),
                        actions: built.actions,
                        proposal_count: built.proposal_count,
                        self_removed: false,
                    },
                )
            }
            Decision::Merge { hash } => match self.merge(hash) {
                Ok(applied) => {
                    let out =
                        self.engine
                            .commit_applied(now, hash, applied.epoch, &applied.members);
                    if out.is_ok() {
                        self.deliver_welcome(hash);
                    }
                    out
                }
                Err(reason) => Err(reason),
            },
            Decision::Discard { hashes } => {
                for hash in hashes {
                    self.mls.discard(hash);
                }
                Ok(Output::default())
            }
            Decision::Leave => {
                self.mls.delete();
                self.left = true;
                Ok(Output::default())
            }
        };
        match result {
            Ok(out) => out,
            Err(reason) => self
                .engine
                .decision_failed(
                    now,
                    DecisionFailure {
                        decision,
                        reason: reason.to_string(),
                    },
                )
                .unwrap_or_default(),
        }
    }

    fn merge(
        &mut self,
        hash: CommitHash,
    ) -> Result<crate::common::fake_mls::Applied, de_mls::ConversationError> {
        if self.mls.pending_hash() != Some(hash) {
            self.mls.clear_pending();
        }
        self.mls
            .merge(hash)
            .map_err(de_mls::ConversationError::InvalidConfig)
    }

    /// Queue the welcome minted for `hash` for its joiners. It carries only
    /// the group snapshot: the joiners' sync arrives as an ordinary control
    /// message once the epoch steward broadcasts it.
    pub fn deliver_welcome(&mut self, hash: CommitHash) {
        let Some(draft) = self.welcomes.remove(&hash) else {
            return;
        };
        self.outbox.push(Frame::Welcome(Welcome {
            joiners: draft.joiners,
            for_commit: hash,
            snapshot: draft.snapshot,
        }));
    }
}

/// A node slot in the bed: live once it has a router.
pub struct Node {
    pub name: String,
    pub own: MemberId,
    pub router: Option<FakeRouter>,
}

/// Several routers on one virtual clock and one network.
pub struct Bed {
    pub now: Timestamp,
    pub net: Net,
    pub nodes: Vec<Node>,
    conversation_id: String,
    config: EngineConfig,
}

impl Bed {
    /// A bed whose node 0 created the conversation.
    pub fn new(conversation_id: &str, creator: &str, config: EngineConfig) -> Self {
        let now = Timestamp::from_duration_since_epoch(Duration::from_secs(1_000));
        let own = FakeMls::member_id(creator);
        let router = FakeRouter::create(now, conversation_id, own.clone(), config.clone());
        Self {
            now,
            net: Net::new(1),
            nodes: vec![Node {
                name: creator.to_string(),
                own,
                router: Some(router),
            }],
            conversation_id: conversation_id.to_string(),
            config,
        }
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// A node that exists on the network but is not in the group yet.
    pub fn add_pending(&mut self, name: &str) -> usize {
        self.net.add_node();
        self.nodes.push(Node {
            name: name.to_string(),
            own: FakeMls::member_id(name),
            router: None,
        });
        self.nodes.len() - 1
    }

    /// The key package `node` would announce.
    pub fn key_package_of(&self, node: usize) -> Vec<u8> {
        FakeMls::key_package(&self.nodes[node].name)
    }

    pub fn router(&self, node: usize) -> &FakeRouter {
        self.nodes[node].router.as_ref().expect("node is live")
    }

    pub fn router_mut(&mut self, node: usize) -> &mut FakeRouter {
        self.nodes[node].router.as_mut().expect("node is live")
    }

    pub fn is_live(&self, node: usize) -> bool {
        self.nodes[node].router.is_some()
    }

    pub fn live_nodes(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| self.is_live(i)).collect()
    }

    /// A clone of `node`'s current store, e.g. to hold onto for a later
    /// deliberately-stale restart.
    pub fn store_of(&self, node: usize) -> InMemoryStore {
        self.router(node).engine.store().clone()
    }

    /// Restart `node`'s engine from its own store.
    pub fn restart(&mut self, node: usize) {
        let now = self.now;
        self.router_mut(node).restart(now);
    }

    /// Restart `node`'s engine using `store` instead of its own.
    pub fn restart_with(&mut self, node: usize, store: InMemoryStore) {
        let now = self.now;
        self.router_mut(node).restart_with(now, store);
    }

    /// Node 0 seats `nodes` with one commit, bypassing the engine's
    /// proposal path: the group builds and merges, the engine is told, the
    /// welcome goes out. For beds that start with an established group.
    pub fn seed(&mut self, nodes: &[usize]) {
        let actions: Vec<de_mls::engine::Action> = nodes
            .iter()
            .map(|&i| de_mls::engine::Action::Add {
                member: self.nodes[i].own.clone(),
                key_package: self.key_package_of(i),
            })
            .collect();
        let now = self.now;
        let creator = self.router_mut(0);
        let built = creator.mls.build_commit(&actions);
        let hash = built.hash;
        if let Some(draft) = built.welcome {
            creator.welcomes.insert(hash, draft);
        }
        let applied = creator.mls.merge(hash).expect("seed merge");
        let out = creator
            .engine
            .commit_applied(now, hash, applied.epoch, &applied.members)
            .expect("seed commit_applied");
        creator.deliver_welcome(hash);
        creator.drive(now, out);
        self.flush();
    }

    /// Move outboxes onto the network.
    fn flush(&mut self) {
        let now = self.now;
        for i in 0..self.nodes.len() {
            let frames = match self.nodes[i].router.as_mut() {
                Some(router) => router.take_outbox(),
                None => continue,
            };
            for frame in frames {
                self.net.broadcast(now, i, frame);
            }
        }
    }

    /// Advance the clock by `step`, deliver what is due, fire due wakeups.
    pub fn process(&mut self, step: Duration) {
        self.flush();
        self.now = self.now + step;
        let now = self.now;
        for (from, frame) in self.net.take_due(now) {
            for i in 0..self.nodes.len() {
                if i == from || self.net.is_muted(i) {
                    continue;
                }
                match (&frame, self.nodes[i].router.as_mut()) {
                    (Frame::Welcome(welcome), None) => {
                        if welcome.joiners.contains(&self.nodes[i].own) {
                            let router = FakeRouter::join(
                                now,
                                self.nodes[i].own.clone(),
                                welcome,
                                self.config.clone(),
                            );
                            self.nodes[i].router = Some(router);
                        }
                    }
                    (Frame::Welcome(_), Some(_)) => {}
                    (frame, Some(router)) => router.on_frame(now, frame.clone()),
                    (_, None) => {}
                }
            }
        }
        for node in &mut self.nodes {
            if let Some(router) = node.router.as_mut() {
                router.on_wakeup(now);
            }
        }
        self.flush();
    }

    /// Step in 50 ms until `predicate`, or panic after 30 virtual seconds.
    pub fn process_until(&mut self, label: &str, predicate: impl Fn(&Self) -> bool) {
        const TIMEOUT: Duration = Duration::from_secs(30);
        const STEP: Duration = Duration::from_millis(50);
        let mut elapsed = Duration::ZERO;
        while !predicate(self) {
            if elapsed >= TIMEOUT {
                panic!("process_until timed out: {label}");
            }
            self.process(STEP);
            elapsed += STEP;
        }
    }

    pub fn mute(&mut self, node: usize) {
        self.net.mute(node);
    }

    pub fn unmute(&mut self, node: usize) {
        self.net.unmute(node);
    }

    /// Every live node is at the same epoch.
    pub fn epochs_agree(&self) -> bool {
        let epochs: Vec<u64> = self
            .live_nodes()
            .iter()
            .map(|&i| self.router(i).mls.epoch())
            .collect();
        epochs.windows(2).all(|w| w[0] == w[1])
    }

    /// Every live node merged the same commits: the fork detector.
    pub fn converged(&self) -> bool {
        let auths: Vec<[u8; 32]> = self
            .live_nodes()
            .iter()
            .map(|&i| self.router(i).mls.authenticator())
            .collect();
        self.epochs_agree() && auths.windows(2).all(|w| w[0] == w[1])
    }

    /// Every live node holds the same member set.
    pub fn membership_agrees(&self) -> bool {
        let sets: Vec<Vec<MemberId>> = self
            .live_nodes()
            .iter()
            .map(|&i| self.router(i).mls.members())
            .collect();
        sets.windows(2).all(|w| w[0] == w[1])
    }
}
