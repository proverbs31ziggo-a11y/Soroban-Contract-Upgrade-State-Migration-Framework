//! CAP-85: atomic upgrades of a whole fleet of contracts.
//!
//! # The problem CAP-85 solves
//!
//! A factory pattern deploys many contract instances sharing one Wasm. Upgrading
//! them means updating each instance, and that is only atomic if the updates fit
//! in one transaction. Past the resource limits they do not, so a rollout leaves
//! some instances on the old code and some on the new — unacceptable when the
//! upgrade carries a security fix.
//!
//! CAP-85 ([protocol 28, "Adapter"][cap85]) adds a second kind of contract
//! executable: instead of embedding a Wasm hash, an instance can point at a
//! *tagged persistent entry owned by another contract*. That entry holds the
//! Wasm hash. One write to the entry changes the code of every instance
//! referencing it, in one transaction, no matter how large the fleet.
//!
//! The protocol enforces the interesting part: an executable reference entry can
//! never be deleted, and its value must be the hash of Wasm that has already been
//! uploaded. A reference cannot be pointed at code that does not exist, and
//! cannot be removed out from under the instances using it. That is why the
//! framework's fleet helpers do not need to check either.
//!
//! [cap85]: https://github.com/stellar/stellar-protocol/blob/master/core/cap-0085.md
//!
//! # How this pairs with a state migration
//!
//! The two halves of an upgrade — new code and new state — are independent, and
//! CAP-85 gives them an ordering that has no bad intermediate state:
//!
//! 1. `publish(tag = "vault-v1", hash)` once, before any migration. The running
//!    code is frozen under a permanent, never-deletable name. This is the
//!    rollback target, and it is written down before anything can go wrong.
//! 2. `publish(tag = "vault-v2", hash)` for the new build.
//! 3. Migrate state in batches. Instances resolve their code from `vault-live`,
//!    which still points at v1 — so the old code keeps serving reads while the
//!    entries are being rewritten, and tolerant decoding covers the overlap.
//! 4. `promote(live = "vault-live", candidate = "vault-v2")`. Every instance
//!    switches to the new code in one ledger.
//! 5. Rollback, if needed, is `promote(live = "vault-live", candidate =
//!    "vault-v1")` — one transaction, at any point, without redeploying.
//!
//! Step 1 is the one teams skip, and it is the one that makes step 5 cheap. It
//! costs one `upload` and one entry.
//!
//! # Which instances this applies to
//!
//! Only instances deployed with an executable reference benefit. An instance
//! deployed with a plain Wasm hash is on its own, and `soroban-migrate fleet
//! inspect` reports the split so an operator knows how much of the fleet a
//! `promote` will actually move.

use soroban_sdk::{Address, BytesN, ContractExecutableRef, Env, String};

use crate::error::MigrationError;

/// Points the tagged entry at `wasm_hash`, creating it if needed.
///
/// Call this for the *candidate* tag before promoting. The hash must already be
/// uploaded; the protocol panics otherwise, and the framework does not pre-check
/// because there is no host function that answers "is this hash uploaded?" short
/// of performing the write.
///
/// # Panics
///
/// Panics if `wasm_hash` is not the hash of Wasm that has already been uploaded.
pub fn publish(env: &Env, tag: &String, wasm_hash: &BytesN<32>) {
    env.executable_refs().set(tag, wasm_hash);
}

/// Reads the Wasm hash a tag currently points at.
pub fn resolve(env: &Env, tag: &String) -> Option<BytesN<32>> {
    env.executable_refs().get(tag)
}

/// Whether the tagged entry exists.
///
/// Worth checking before `promote`: pointing a live instance at a tag that was
/// never published fails at the protocol level, but it fails *after* the caller
/// has already committed to the rollout.
pub fn exists(env: &Env, tag: &String) -> bool {
    env.executable_refs().has(tag)
}

/// Extends the tagged entry's TTL.
///
/// Executable reference entries are persistent and can archive like any other
/// entry. If one archives, every instance referencing it stops being loadable —
/// and unlike ordinary data, the reference entry cannot be "restored from a
/// backup" by the application, because the application does not own its
/// contents. Fleets should extend their live tag's TTL on a schedule, and
/// [`crate::executor`] does it for the tag in use when a migration runs.
///
/// # Panics
///
/// Panics if the entry does not exist.
pub fn extend_ttl(env: &Env, tag: &String, threshold: u32, extend_to: u32) {
    env.executable_refs().extend_ttl(tag, threshold, extend_to);
}

/// Repoints `live_tag` at whatever `candidate_tag` currently resolves to.
///
/// One write; every instance using `live_tag` runs the candidate's code from its
/// next invocation. Returns the hash that was promoted so the caller can record
/// it, and so a rollback has something concrete to compare against.
///
/// # Errors
///
/// [`MigrationError::UnknownExecutableTag`] if either tag has no entry. Both are
/// checked before any write, so a failed promote leaves the live tag untouched
/// rather than half-moved.
pub fn promote(
    env: &Env,
    live_tag: &String,
    candidate_tag: &String,
) -> Result<BytesN<32>, MigrationError> {
    let refs = env.executable_refs();
    if !refs.has(live_tag) {
        return Err(MigrationError::UnknownExecutableTag);
    }
    let Some(candidate) = refs.get(candidate_tag) else {
        return Err(MigrationError::UnknownExecutableTag);
    };
    refs.set(live_tag, &candidate);
    Ok(candidate)
}

/// Repoints `live_tag` straight at a Wasm hash.
///
/// The rollback primitive, for when the previous hash was recorded off-chain
/// rather than under a tag. Prefer [`promote`] with a versioned tag: a hash
/// recorded in a deploy log cannot be validated in advance, whereas a published
/// tag's entry cannot have been deleted and must always hold uploaded Wasm.
///
/// # Panics
///
/// Panics if `wasm_hash` has not been uploaded.
pub fn promote_to_hash(env: &Env, live_tag: &String, wasm_hash: &BytesN<32>) {
    env.executable_refs().set(live_tag, wasm_hash);
}

/// Makes the *current* contract resolve its code from `owner`'s `tag`.
///
/// Applies at the end of the invocation, and only if it returns successfully —
/// so an upgrade entry point that fails halfway leaves the old code running. This
/// is the protocol's own safety property and the reason the framework does not
/// wrap it: there is nothing to add.
///
/// # Panics
///
/// Panics if `owner` has no entry for `tag`.
pub fn adopt_executable_ref(env: &Env, owner: &Address, tag: &String) {
    env.deployer()
        .update_current_contract(soroban_sdk::ContractExecutable::ExternalRef(
            ContractExecutableRef {
                owner: owner.clone(),
                tag: tag.clone(),
            },
        ));
}

/// Builds the executable reference to deploy an instance with.
///
/// Pass to `DeployerWithAddress::deploy_contract` so a new instance is born
/// pointed at the fleet tag rather than at a frozen hash. Instances created this
/// way are covered by every future `promote`.
pub fn instance_ref(owner: &Address, tag: &String) -> soroban_sdk::ContractExecutable {
    soroban_sdk::ContractExecutable::ExternalRef(ContractExecutableRef {
        owner: owner.clone(),
        tag: tag.clone(),
    })
}

// # Auditing which instances are reference-backed
//
// There is deliberately no `address_executable_ref` here. The host's
// `get_address_executable` resolves an executable reference to the Wasm hash it
// points at, so that a third party can vet an implementation without being able
// to read another contract's storage — and the consequence is that the reference
// *itself* is not observable for an arbitrary address. An instance's Wasm hash is;
// the tag it came from is not.
//
// Auditing a fleet therefore works from an off-chain manifest of instance
// addresses plus the deployment records that say how each was deployed, not from
// the ledger. `soroban-migrate fleet inspect` consumes that manifest and reports
// how many instances a `promote` will actually move — because an operator who
// assumes their whole fleet is reference-backed will otherwise discover the
// pinned instances the hard way, one at a time, in production.
