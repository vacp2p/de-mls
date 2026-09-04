//! The conformance suite for the group-operations contract.
//!
//! Every case is written against [`GroupOps`] and reaches an implementation
//! only through the [`Suite`] trait below, which says how to mint a member,
//! stand a group up, and compare two nodes. To run the suite against another
//! implementation, write a second `Suite` impl and point the `#[test]`
//! functions at it.

use std::collections::BTreeSet;

use mls_reference::{Action, GroupOps, Reference};
use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls::group::{MlsGroupCreateConfig, MlsGroupJoinConfig};
use openmls::key_packages::KeyPackage;
use openmls::prelude::Ciphersuite;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
const CONVERSATION: &str = "conformance-conversation";

// ══════════════════════════════════════════════════════════════
// What the suite needs beyond the trait
// ══════════════════════════════════════════════════════════════

/// Construction and comparison, which [`GroupOps`] deliberately leaves to the
/// implementation.
trait Suite {
    /// The implementation under test.
    type Group: GroupOps;
    /// An identity that does not belong to a group yet: keys, credential, and
    /// whatever storage the implementation needs.
    type Member;

    fn member(name: &str) -> Self::Member;
    /// Key-package bytes for a member that is about to be added.
    fn key_package(member: &Self::Member) -> Vec<u8>;
    fn create(member: Self::Member, conversation_id: &str) -> Self::Group;
    /// `None` when the welcome does not address this member.
    fn join(member: Self::Member, welcome: &[u8]) -> Option<Self::Group>;
    /// The MLS group id, for the create-time identity check.
    fn group_id(group: &Self::Group) -> Vec<u8>;
    /// A value two nodes at the same epoch of the same group agree on and
    /// nobody else can produce — the convergence check.
    fn authenticator(group: &Self::Group) -> Vec<u8>;
}

/// The reference implementation, over the in-memory OpenMLS provider and
/// basic credentials.
struct ReferenceSuite;

struct ReferenceMember {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    credential: CredentialWithKey,
}

impl Suite for ReferenceSuite {
    type Group = Reference<OpenMlsRustCrypto, SignatureKeyPair>;
    type Member = ReferenceMember;

    fn member(name: &str) -> Self::Member {
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm()).expect("signer");
        let credential = CredentialWithKey {
            credential: BasicCredential::new(name.as_bytes().to_vec()).into(),
            signature_key: signer.to_public_vec().into(),
        };
        ReferenceMember {
            provider: OpenMlsRustCrypto::default(),
            signer,
            credential,
        }
    }

    fn key_package(member: &Self::Member) -> Vec<u8> {
        KeyPackage::builder()
            .build(
                CIPHERSUITE,
                &member.provider,
                &member.signer,
                member.credential.clone(),
            )
            .expect("key package")
            .key_package()
            .tls_serialize_detached()
            .expect("serialize key package")
    }

    fn create(member: Self::Member, conversation_id: &str) -> Self::Group {
        Reference::create(
            conversation_id.to_string(),
            member.provider,
            member.signer,
            member.credential,
            MlsGroupCreateConfig::builder(),
        )
        .expect("create group")
    }

    fn join(member: Self::Member, welcome: &[u8]) -> Option<Self::Group> {
        Reference::open_welcome(
            member.provider,
            member.signer,
            welcome,
            MlsGroupJoinConfig::builder(),
        )
        .expect("open welcome")
    }

    fn group_id(group: &Self::Group) -> Vec<u8> {
        group.group().group_id().as_slice().to_vec()
    }

    fn authenticator(group: &Self::Group) -> Vec<u8> {
        group.epoch_authenticator()
    }
}

// ══════════════════════════════════════════════════════════════
// Helpers, all generic over the suite
// ══════════════════════════════════════════════════════════════

/// The creator plus one joiner per name, every node merged in and at the same
/// epoch. Index 0 is the creator.
fn group_of<S: Suite>(joiner_names: &[&str]) -> Vec<S::Group> {
    let mut creator = S::create(S::member("creator"), CONVERSATION);
    let joiners: Vec<S::Member> = joiner_names.iter().map(|n| S::member(n)).collect();

    let actions: Vec<Action> = joiners.iter().map(add_action::<S>).collect();
    let built = creator.build_commit(&actions).expect("build add commit");
    creator.merge(built.hash).expect("merge add commit");
    let welcome = built.welcome.expect("adds produce a welcome");

    let mut nodes = vec![creator];
    for joiner in joiners {
        nodes.push(S::join(joiner, &welcome).expect("welcome addresses the joiner"));
    }
    nodes
}

/// The add action seating `member`, with the id read off its key package.
fn add_action<S: Suite>(member: &S::Member) -> Action {
    let key_package = S::key_package(member);
    Action::Add {
        member: <S::Group as GroupOps>::key_package_identity(&key_package)
            .expect("key package identity"),
        key_package,
    }
}

/// One empty commit round won by `builder`: everyone else stages it, everyone
/// merges it, and the nodes are checked to have converged.
fn advance<S: Suite>(nodes: &mut [S::Group], builder: usize) {
    let built = nodes[builder].build_commit(&[]).expect("build commit");
    for (i, node) in nodes.iter_mut().enumerate() {
        if i != builder {
            node.stage(&built.commit).expect("stage the winner");
        }
    }
    for node in nodes.iter_mut() {
        node.merge(built.hash).expect("merge the winner");
    }
    assert_converged::<S>(nodes);
}

/// Same epoch, same epoch authenticator, same member set.
fn assert_converged<S: Suite>(nodes: &[S::Group]) {
    let first = &nodes[0];
    for node in &nodes[1..] {
        assert_eq!(node.epoch(), first.epoch(), "epochs diverged");
        assert_eq!(
            S::authenticator(node),
            S::authenticator(first),
            "epoch authenticators diverged"
        );
        assert_eq!(member_set(node), member_set(first), "member sets diverged");
    }
}

fn member_set<G: GroupOps>(group: &G) -> BTreeSet<Vec<u8>> {
    group.members().into_iter().collect()
}

// ══════════════════════════════════════════════════════════════
// (a) create
// ══════════════════════════════════════════════════════════════

fn case_create<S: Suite>() {
    let group = S::create(S::member("creator"), CONVERSATION);

    assert_eq!(group.conversation_id(), CONVERSATION);
    assert_eq!(
        S::group_id(&group),
        CONVERSATION.as_bytes(),
        "the group id is the conversation id bytes"
    );
    assert_eq!(group.epoch(), 0);
    assert_eq!(group.members(), vec![group.own_id()]);
    assert_eq!(group.pending_hash(), None);
}

#[test]
fn create_seeds_the_conversation() {
    case_create::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (b) build_commit stays pending; the welcome seats the joiners
// ══════════════════════════════════════════════════════════════

fn case_adds_stay_pending_and_welcome_joiners<S: Suite>() {
    let mut creator = S::create(S::member("creator"), CONVERSATION);
    let joiners = [S::member("bob"), S::member("carol")];
    let actions: Vec<Action> = joiners.iter().map(add_action::<S>).collect();

    let built = creator.build_commit(&actions).expect("build add commit");

    assert_eq!(creator.epoch(), 0, "a built commit does not move the epoch");
    assert_eq!(creator.members().len(), 1, "and does not seat anybody yet");
    assert_eq!(creator.pending_hash(), Some(built.hash));
    assert_eq!(
        built.hash,
        <S::Group as GroupOps>::commit_hash(&built.commit)
    );
    assert_eq!(built.proposal_count, 2);
    let welcome = built.welcome.clone().expect("two adds produce a welcome");

    creator.merge(built.hash).expect("merge add commit");
    assert_eq!(creator.epoch(), 1);
    assert_eq!(creator.pending_hash(), None);

    let mut nodes = vec![creator];
    for joiner in joiners {
        nodes.push(S::join(joiner, &welcome).expect("welcome addresses the joiner"));
    }

    assert_eq!(nodes.len(), 3);
    for node in &nodes {
        assert_eq!(node.conversation_id(), CONVERSATION);
        assert_eq!(node.members().len(), 3);
    }
    assert_converged::<S>(&nodes);
}

#[test]
fn adds_stay_pending_and_the_welcome_seats_the_joiners() {
    case_adds_stay_pending_and_welcome_joiners::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (c) three candidates for one epoch, one winner, both losers converge
// ══════════════════════════════════════════════════════════════

fn case_competing_commits<S: Suite>() {
    let mut nodes = group_of::<S>(&["bob", "carol"]);
    let ids: Vec<Vec<u8>> = nodes.iter().map(|n| n.own_id()).collect();
    let epoch = nodes[0].epoch();

    let a = nodes[0].build_commit(&[]).expect("a builds");
    let b = nodes[1].build_commit(&[]).expect("b builds");
    let c = nodes[2].build_commit(&[]).expect("c builds");

    // a holds its own candidate and stages both rivals for the same epoch.
    let from_b = nodes[0].stage(&b.commit).expect("a stages b");
    let from_c = nodes[0].stage(&c.commit).expect("a stages c");

    assert_eq!(nodes[0].epoch(), epoch, "staging does not apply");
    assert_eq!(
        nodes[0].pending_hash(),
        Some(a.hash),
        "staging leaves our own candidate pending"
    );
    assert_eq!(from_b.sender, ids[1], "the sender is MLS-authenticated");
    assert_eq!(from_c.sender, ids[2]);
    assert_eq!(from_b.epoch, epoch);
    assert!(
        from_b.actions.is_empty(),
        "an empty commit adds and removes nobody"
    );
    assert!(from_c.actions.is_empty());
    assert!(!from_b.self_removed);

    // b wins. a is a loser holding a staged winner; c is a loser that has not
    // staged it yet.
    nodes[0].clear_pending().expect("a drops its candidate");
    let applied = nodes[0].merge(b.hash).expect("a merges the winner");
    assert_eq!(applied.epoch, epoch + 1);
    assert_eq!(member_set(&nodes[0]), applied.members.into_iter().collect());

    nodes[1]
        .merge(b.hash)
        .expect("b merges its own pending commit");

    nodes[2].stage(&b.commit).expect("c stages the winner");
    nodes[2].clear_pending().expect("c drops its candidate");
    nodes[2].merge(b.hash).expect("c merges the winner");

    assert_converged::<S>(&nodes);
    for node in &nodes {
        assert_eq!(node.pending_hash(), None);
    }

    // The round after the race runs normally on every node.
    advance::<S>(&mut nodes, 2);
}

#[test]
fn competing_commits_converge_on_one_winner() {
    case_competing_commits::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (d) stage rejects the wrong group and a past epoch
// ══════════════════════════════════════════════════════════════

fn case_stage_rejects<S: Suite>() {
    let mut nodes = group_of::<S>(&["bob"]);

    let mut outsider = S::create(S::member("outsider"), "another-conversation");
    let foreign = outsider.build_commit(&[]).expect("outsider builds");
    assert!(
        nodes[0].stage(&foreign.commit).is_err(),
        "a commit for another group is rejected"
    );

    let stale = nodes[1].build_commit(&[]).expect("bob builds");
    advance::<S>(&mut nodes, 0);
    assert!(
        nodes[0].stage(&stale.commit).is_err(),
        "a commit from a past epoch is rejected"
    );
}

#[test]
fn stage_rejects_foreign_and_stale_commits() {
    case_stage_rejects::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (e) the open window
// ══════════════════════════════════════════════════════════════

fn case_open_window<S: Suite>() {
    // Newer than ours: a peer sealed at an epoch we have not merged into.
    {
        let mut nodes = group_of::<S>(&["bob"]);
        let ahead = nodes[1].build_commit(&[]).expect("bob builds");
        nodes[1].merge(ahead.hash).expect("bob merges alone");
        let sealed = nodes[1].seal(b"from the future").expect("seal");
        assert_eq!(
            nodes[0].open(&sealed).expect("open"),
            None,
            "a message from an epoch we have not reached is dropped"
        );
    }

    // Older than `current - past_window`, with the boundary still readable.
    {
        let mut nodes = group_of::<S>(&["bob"]);
        let at_boundary = nodes[1].seal(b"old").expect("seal");
        let past_boundary = nodes[1].seal(b"older").expect("seal");
        for _ in 0..3 {
            advance::<S>(&mut nodes, 0);
        }
        assert!(
            nodes[0].open(&at_boundary).expect("open").is_some(),
            "three epochs back is still inside the retained window"
        );
        advance::<S>(&mut nodes, 0);
        assert_eq!(
            nodes[0].open(&past_boundary).expect("open"),
            None,
            "four epochs back is outside it"
        );
    }

    // Older than our own join epoch.
    {
        let mut nodes = group_of::<S>(&["bob"]);
        let before_carol = nodes[1].seal(b"before carol joined").expect("seal");

        let carol = S::member("carol");
        let built = nodes[0]
            .build_commit(&[add_action::<S>(&carol)])
            .expect("build add commit");
        nodes[1].stage(&built.commit).expect("bob stages");
        for node in nodes.iter_mut() {
            node.merge(built.hash).expect("merge add commit");
        }
        let mut carol = S::join(carol, &built.welcome.expect("welcome")).expect("carol joins");

        assert_eq!(
            carol.open(&before_carol).expect("open"),
            None,
            "a message sealed before we joined is dropped"
        );
    }

    // A commit is not application traffic.
    {
        let mut nodes = group_of::<S>(&["bob"]);
        let built = nodes[1].build_commit(&[]).expect("bob builds");
        assert_eq!(
            nodes[0].open(&built.commit).expect("open"),
            None,
            "a commit fed to open is dropped, not processed"
        );
    }

    // And the ordinary case: sealed at the epoch we just merged into.
    {
        let mut nodes = group_of::<S>(&["bob"]);
        advance::<S>(&mut nodes, 0);
        let sealed = nodes[1].seal(b"after the merge").expect("seal");
        let opened = nodes[0].open(&sealed).expect("open").expect("not dropped");
        assert_eq!(opened.plaintext, b"after the merge");
        assert_eq!(opened.epoch, nodes[0].epoch());
    }
}

#[test]
fn open_drops_everything_outside_the_window() {
    case_open_window::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (f) the sender comes from the signature, never the payload
// ══════════════════════════════════════════════════════════════

fn case_sender_is_authenticated<S: Suite>() {
    let mut nodes = group_of::<S>(&["bob"]);
    let alice_id = nodes[0].own_id();
    let bob_id = nodes[1].own_id();

    // The payload is bob claiming to be alice.
    let sealed = nodes[1].seal(&alice_id).expect("seal");
    let opened = nodes[0].open(&sealed).expect("open").expect("not dropped");

    assert_eq!(opened.sender, bob_id, "the sender is the signer");
    assert_eq!(opened.plaintext, alice_id, "the payload is untouched");
    assert_ne!(opened.sender, alice_id);
}

#[test]
fn the_sender_is_the_signer() {
    case_sender_is_authenticated::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (g) a key package already names the member it will become
// ══════════════════════════════════════════════════════════════

fn case_key_package_identity<S: Suite>() {
    let mut creator = S::create(S::member("creator"), CONVERSATION);
    let bob = S::member("bob");
    let key_package = S::key_package(&bob);

    let announced =
        <S::Group as GroupOps>::key_package_identity(&key_package).expect("key package identity");
    creator
        .validate_key_package(&key_package)
        .expect("key package validates");
    assert!(
        creator
            .validate_key_package(&key_package[..key_package.len() / 2])
            .is_err(),
        "a truncated key package does not validate"
    );

    let built = creator
        .build_commit(&[Action::Add {
            member: announced.clone(),
            key_package,
        }])
        .expect("build add commit");
    creator.merge(built.hash).expect("merge add commit");
    let bob = S::join(bob, &built.welcome.expect("welcome")).expect("bob joins");

    assert!(
        creator.members().contains(&announced),
        "the announced id is the seated id"
    );
    assert_eq!(bob.own_id(), announced);
}

#[test]
fn a_key_package_names_the_seated_member() {
    case_key_package_identity::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (h) discard drops a staged commit
// ══════════════════════════════════════════════════════════════

fn case_discard<S: Suite>() {
    let mut nodes = group_of::<S>(&["bob"]);
    let epoch = nodes[0].epoch();

    let built = nodes[1].build_commit(&[]).expect("bob builds");
    nodes[0].stage(&built.commit).expect("alice stages");
    nodes[0].discard(built.hash);

    assert!(
        nodes[0].merge(built.hash).is_err(),
        "a discarded commit cannot be merged"
    );
    assert_eq!(nodes[0].epoch(), epoch, "and the epoch did not move");

    nodes[0].discard(built.hash);
}

#[test]
fn discard_drops_a_staged_commit() {
    case_discard::<ReferenceSuite>();
}

// ══════════════════════════════════════════════════════════════
// (i) teardown is idempotent
// ══════════════════════════════════════════════════════════════

fn case_delete_is_idempotent<S: Suite>() {
    let mut group = S::create(S::member("creator"), CONVERSATION);
    group.delete().expect("first teardown");
    group.delete().expect("second teardown");
}

#[test]
fn delete_is_idempotent() {
    case_delete_is_idempotent::<ReferenceSuite>();
}
