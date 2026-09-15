//! CAP-85 executable references: the atomic fleet upgrade primitive.
//!
//! These tests exercise the reference entries directly. The end-to-end shape — a
//! registry contract owning the tags, instances deployed against them, and one
//! `promote` moving the whole fleet — is covered against a real deployed contract
//! in `examples/vault`, where a contract frame exists to deploy from.

mod support;

use soroban_sdk::{Env, String};

use soroban_migrate::fleet;
use soroban_migrate::MigrationError;

use support::{host, HostContract};

fn tagged(env: &Env, tag: &str) -> String {
    String::from_str(env, tag)
}

/// Uploads a contract under two distinct Wasm hashes, standing in for two builds.
///
/// `Env::upload` registers a native contract as if it were Wasm, which is the
/// supported way to test deployments where several instances share one code entry
/// without building a Wasm artifact first.
fn two_builds(env: &Env) -> (soroban_sdk::BytesN<32>, soroban_sdk::BytesN<32>) {
    let v1 = env.upload(HostContract);
    let v2 = env.upload(HostContract);
    assert_ne!(v1, v2, "the two builds must be distinguishable");
    (v1, v2)
}

#[test]
fn a_tag_resolves_to_nothing_before_it_is_published() {
    let env = Env::default();
    let id = host(&env);
    let tag = tagged(&env, "vault-v1");
    env.as_contract(&id, || {
        assert!(!fleet::exists(&env, &tag));
        assert_eq!(fleet::resolve(&env, &tag), None);
    });
}

#[test]
fn publishing_records_the_wasm_hash_under_the_tag() {
    let env = Env::default();
    let id = host(&env);
    let (v1, _) = two_builds(&env);
    let tag = tagged(&env, "vault-v1");

    env.as_contract(&id, || {
        assert!(!fleet::exists(&env, &tag));
        fleet::publish(&env, &tag, &v1);
        assert!(fleet::exists(&env, &tag));
        assert_eq!(fleet::resolve(&env, &tag), Some(v1.clone()));
    });
}

#[test]
fn publishing_the_same_tag_again_repoints_it() {
    let env = Env::default();
    let id = host(&env);
    let (v1, v2) = two_builds(&env);
    let tag = tagged(&env, "vault-live");

    env.as_contract(&id, || {
        fleet::publish(&env, &tag, &v1);
        fleet::publish(&env, &tag, &v2);
        // This single write is the whole atomic fleet upgrade: every instance
        // resolving its code from this entry runs `v2` from its next invocation,
        // no matter how many there are.
        assert_eq!(fleet::resolve(&env, &tag), Some(v2.clone()));
    });
}

#[test]
fn promote_moves_the_live_tag_to_the_candidate() {
    let env = Env::default();
    let id = host(&env);
    let (v1, v2) = two_builds(&env);
    let live = tagged(&env, "vault-live");
    let candidate = tagged(&env, "vault-v2");

    env.as_contract(&id, || {
        fleet::publish(&env, &live, &v1);
        fleet::publish(&env, &candidate, &v2);

        let promoted = fleet::promote(&env, &live, &candidate).unwrap();
        assert_eq!(promoted, v2);
        assert_eq!(fleet::resolve(&env, &live), Some(v2.clone()));
        // The candidate tag is untouched, so it remains a valid target for a
        // later promote — and `vault-v1` remains the rollback target.
        assert_eq!(fleet::resolve(&env, &candidate), Some(v2));
    });
}

#[test]
fn promote_is_the_rollback_primitive_too() {
    let env = Env::default();
    let id = host(&env);
    let (v1, v2) = two_builds(&env);
    let live = tagged(&env, "vault-live");
    let v1_tag = tagged(&env, "vault-v1");
    let v2_tag = tagged(&env, "vault-v2");

    env.as_contract(&id, || {
        // Step 1 of the documented rollout: freeze the running build under a
        // permanent name *before* anything can go wrong. This is the step teams
        // skip, and it is the one that makes the rollback below a single write.
        fleet::publish(&env, &v1_tag, &v1);
        fleet::publish(&env, &live, &v1);
        fleet::publish(&env, &v2_tag, &v2);

        fleet::promote(&env, &live, &v2_tag).unwrap();
        assert_eq!(fleet::resolve(&env, &live), Some(v2));

        // Roll back. No redeploy, no new upload, one transaction.
        let rolled_back = fleet::promote(&env, &live, &v1_tag).unwrap();
        assert_eq!(rolled_back, v1);
        assert_eq!(fleet::resolve(&env, &live), Some(v1));
    });
}

#[test]
fn promote_refuses_an_unknown_tag_without_moving_the_live_one() {
    let env = Env::default();
    let id = host(&env);
    let (v1, _) = two_builds(&env);
    let live = tagged(&env, "vault-live");
    let missing = tagged(&env, "vault-v3");

    env.as_contract(&id, || {
        fleet::publish(&env, &live, &v1);

        // Both tags are checked before anything is written, so a typo in a tag
        // name does not leave the fleet half-moved.
        assert_eq!(
            fleet::promote(&env, &live, &missing),
            Err(MigrationError::UnknownExecutableTag)
        );
        assert_eq!(fleet::resolve(&env, &live), Some(v1));

        let never_published = tagged(&env, "vault-never");
        assert_eq!(
            fleet::promote(&env, &never_published, &live),
            Err(MigrationError::UnknownExecutableTag)
        );
    });
}

#[test]
fn promote_to_hash_is_the_escape_hatch_for_an_off_chain_rollback_target() {
    let env = Env::default();
    let id = host(&env);
    let (v1, v2) = two_builds(&env);
    let live = tagged(&env, "vault-live");

    env.as_contract(&id, || {
        fleet::publish(&env, &live, &v2);
        // Less safe than a tag: a hash recorded in a deploy log cannot be
        // validated in advance, whereas a published tag's entry cannot have been
        // deleted and is guaranteed to hold uploaded Wasm.
        fleet::promote_to_hash(&env, &live, &v1);
        assert_eq!(fleet::resolve(&env, &live), Some(v1));
    });
}

#[test]
fn instance_ref_builds_a_reference_to_a_fleet_tag() {
    let env = Env::default();
    let owner = host(&env);
    let tag = tagged(&env, "vault-live");

    match fleet::instance_ref(&owner, &tag) {
        soroban_sdk::ContractExecutable::ExternalRef(r) => {
            assert_eq!(r.owner, owner);
            assert_eq!(r.tag, tag);
        }
        soroban_sdk::ContractExecutable::Wasm(_) => {
            panic!("a fleet instance must be deployed against the tag, not a frozen hash")
        }
    }
}

#[test]
fn ttl_can_be_extended_for_a_live_tag() {
    let env = Env::default();
    let id = host(&env);
    let (v1, _) = two_builds(&env);
    let live = tagged(&env, "vault-live");

    env.as_contract(&id, || {
        fleet::publish(&env, &live, &v1);
        // An archived reference entry makes every instance that resolves through
        // it unloadable, and the application cannot restore it because it does not
        // own the entry's contents. Fleets must keep this alive on a schedule.
        fleet::extend_ttl(&env, &live, 100, 200_000);
        env.executable_refs().get_ttl(&live);
    });
}
