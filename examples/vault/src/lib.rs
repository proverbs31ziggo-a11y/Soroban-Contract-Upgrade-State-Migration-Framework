#![no_std]
//! A worked example of a Soroban contract upgrade driven by `soroban-migrate`.
//!
//! The contract is a deliberately small balance ledger. What it demonstrates is not
//! the ledger: it is the full shape of an upgrade that a real contract would go
//! through, with every step wired to the framework and every step testable.
//!
//! # The change
//!
//! Schema version 1 stores a balance as `{ owner, amount }`. Version 2 adds a freeze
//! flag: `{ owner, amount, frozen: Option<bool> }`. See [`schema`].
//!
//! # The sequence
//!
//! 1. **Adopt.** The deployed contract records which schema version its entries are
//!    in. Done by [`Vault::initialize`], or by `soroban-migrate adopt` for a contract
//!    that predates the framework.
//! 2. **Freeze the running build under a tag** ([`Vault::publish_build`]). One upload
//!    and one entry, and it is what makes step 6 a single transaction instead of a
//!    redeploy. Teams skip this step, and it is the one that makes rollback cheap.
//! 3. **Pre-flight** ([`Vault::preflight`]). Refuses to start unless the contract is at
//!    the expected version, no migration is in flight, and the framework has actually
//!    seen the contract's keys. That last check is the one that catches the failure
//!    nothing else can: a migration that silently misses entries because the
//!    application never registered them.
//! 4. **Migrate in batches** ([`Vault::migrate_batch`]). Each call is one transaction.
//!    The cursor lives on chain, so a failed or abandoned batch resumes where it left
//!    off rather than restarting. The version stays at 1 until the last batch, which
//!    advances it in the same transaction.
//! 5. **Confirm** ([`Vault::confirm_migration`]), discarding the bookkeeping once the
//!    contract is healthy at the new version.
//! 6. **Promote** ([`Vault::promote_fleet`]), repointing the live tag at the new build.
//!    Every instance deployed against that tag switches code in one ledger. Rollback is
//!    the same call with the older tag.
//!
//! # What the application has to get right
//!
//! Every write goes through [`store::put`], which registers the key in the framework's
//! index *and* writes the new shape. That is the invariant the framework cannot check
//! for the application: a write in the old shape after the cursor has passed that key
//! is never revisited, and a write whose key is never registered is invisible to the
//! next migration. Both failures are silent.

pub mod keys;
pub mod migration;
pub mod schema;

use soroban_sdk::{contract, contractimpl, Address, BytesN, Env, IntoVal, String, Val};

use soroban_migrate::migration::{BatchOutcome, MigrationState};
use soroban_migrate::preflight::PreflightReport;
use soroban_migrate::{executor, fleet, preflight, rollback, store, version, MigrationError};

use keys::DataKey;
use migration::AddFreezeFlag;
use schema::{BalanceV2, StatsV2};

/// The version a freshly deployed contract starts at.
pub const DEPLOY_VERSION: u32 = 1;

/// How long a fleet tag's TTL is extended for, in ledgers. Roughly 30 days at five
/// seconds per ledger.
pub const TAG_TTL_THRESHOLD: u32 = 100_000;

/// See [`TAG_TTL_THRESHOLD`].
pub const TAG_TTL_EXTEND_TO: u32 = 518_400;

#[contract]
pub struct Vault;

#[contractimpl]
impl Vault {
    // --- lifecycle ---------------------------------------------------------

    /// Records the administrator and the initial schema version.
    ///
    /// Idempotent: calling it twice with the same administrator is a no-op, which
    /// matters because a deploy script that re-runs must not fail, and because
    /// [`soroban_migrate::version::initialize`] is the one place a version is written
    /// without a migration justifying it.
    pub fn initialize(env: Env, admin: Address) -> Result<u32, MigrationError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            env.storage().instance().set(&DataKey::Admin, &admin);
        }
        version::initialize::<DataKey>(&env, DEPLOY_VERSION)
    }

    /// The administrator.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    // --- application ------------------------------------------------------

    /// Adds `amount` to `owner`'s balance, creating it if absent.
    ///
    /// Note what it does *not* do: it does not branch on the schema version. The write
    /// path is identical before, during, and after the migration, because
    /// [`store::put`] resolves the version to register under and because
    /// [`BalanceV2`] can read an entry written at version 1. A write path that had to
    /// ask "is the migration running?" would be a write path with a bug in it on the
    /// day the answer changed mid-batch.
    pub fn deposit(env: Env, owner: Address, amount: i128) -> BalanceV2 {
        owner.require_auth();
        let current = Self::balance(env.clone(), owner.clone()).unwrap_or(BalanceV2 {
            owner: owner.clone(),
            amount: 0,
            frozen: None,
        });
        let updated = BalanceV2 {
            owner: owner.clone(),
            amount: current.amount + amount,
            // Always materialise the flag on write. An entry left with `frozen: None`
            // is valid but ambiguous — it means both "not yet migrated" and "migrated
            // and legitimately unset" — and writing through the ambiguity is how a
            // migration ends up unable to tell what it still has to do.
            frozen: Some(current.frozen.unwrap_or(false)),
        };
        Self::write_balance(&env, &owner, &updated);
        Self::bump_stats(&env, amount);
        updated
    }

    /// Reads a balance.
    ///
    /// Decodes with the *new* type, which is what makes this work throughout a
    /// migration: an entry written at version 1 arrives with `frozen: None`.
    pub fn balance(env: Env, owner: Address) -> Option<BalanceV2> {
        let key = DataKey::Balance(owner).into_val(&env);
        env.storage().persistent().get::<Val, BalanceV2>(&key)
    }

    /// Sets an account's freeze flag, initialising the entry if it does not exist.
    pub fn set_frozen(env: Env, owner: Address, frozen: bool) -> BalanceV2 {
        owner.require_auth();
        let current = Self::balance(env.clone(), owner.clone()).unwrap_or(BalanceV2 {
            owner: owner.clone(),
            amount: 0,
            frozen: None,
        });
        let updated = BalanceV2 {
            owner: owner.clone(),
            amount: current.amount,
            frozen: Some(frozen),
        };
        Self::write_balance(&env, &owner, &updated);
        updated
    }

    /// The contract-wide counter.
    pub fn stats(env: Env) -> Option<StatsV2> {
        env.storage().persistent().get(&DataKey::Stats)
    }

    // --- introspection ----------------------------------------------------

    /// The schema version the contract's entries are recorded as being at.
    ///
    /// A *deployed* contract can be asked this, which is what makes it possible to tell
    /// a half-migrated contract from a healthy one without reading a deploy log.
    pub fn schema_version(env: Env) -> Option<u32> {
        version::read::<DataKey>(&env)
    }

    /// Where an in-flight or finished migration is.
    pub fn migration_status(env: Env) -> Option<MigrationState> {
        executor::status::<AddFreezeFlag>(&env)
    }

    /// Whether the contract is in a state where a migration can start, and how many
    /// keys the framework knows about.
    ///
    /// Call this before [`Vault::begin_migration`]: the check it performs that nothing
    /// else can is the last one, and the failure it catches is the one that looks like
    /// success.
    pub fn preflight(env: Env) -> Result<PreflightReport, MigrationError> {
        preflight::assert_upgradeable::<AddFreezeFlag>(&env)
    }

    // --- migration driver -------------------------------------------------

    /// Starts the v1 to v2 migration, or resumes it.
    pub fn begin_migration(env: Env) -> Result<MigrationState, MigrationError> {
        let admin = Self::require_admin(&env);
        Self::extend_instance(&env);
        admin.require_auth();
        preflight::assert_upgradeable::<AddFreezeFlag>(&env)?;
        executor::begin::<AddFreezeFlag>(&env)
    }

    /// Applies up to `limit` keys.
    ///
    /// One call is one transaction. `limit` must not exceed
    /// [`soroban_migrate::MAX_KEYS_PER_BATCH`]; the framework refuses anything larger
    /// rather than letting it fail on chain halfway through.
    pub fn migrate_batch(env: Env, limit: u32) -> Result<BatchOutcome, MigrationError> {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        executor::batch::<AddFreezeFlag>(&env, limit)
    }

    /// Discards the migration bookkeeping once the contract is healthy at v2.
    ///
    /// The version itself is kept: it is what reads are gated on and it must outlive
    /// the record of how it got there.
    pub fn confirm_migration(env: Env) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        executor::clear::<DataKey>(&env)
    }

    /// Abandons an in-flight migration without undoing it.
    ///
    /// Leaves entries in whichever shape they reached, relying on tolerant decoding
    /// until a migration is run to completion. Use the rollback path instead when the
    /// goal is a consistent old-shaped state.
    pub fn abort_migration(env: Env) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        executor::abort::<DataKey>(&env)
    }

    /// Starts undoing a completed migration.
    ///
    /// Refused before anything is written if the migration does not declare a
    /// compensating transformation. Pair it with a fleet rollback: reverting state and
    /// promoting the old build are two steps, and the framework will not do one without
    /// saying so.
    pub fn begin_rollback(env: Env) -> Result<MigrationState, MigrationError> {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        rollback::begin::<AddFreezeFlag>(&env)
    }

    /// Reverts up to `limit` keys, newest first.
    pub fn rollback_batch(env: Env, limit: u32) -> Result<BatchOutcome, MigrationError> {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        rollback::batch::<AddFreezeFlag>(&env, limit)
    }

    // --- CAP-85 fleet ----------------------------------------------------

    /// Publishes a build's Wasm hash under a tag this contract owns.
    ///
    /// An executable reference entry can never be deleted and its value must be the
    /// hash of already-uploaded Wasm, so a published tag is a permanent, validated name
    /// for a build. Publishing the *currently running* build before an upgrade is what
    /// makes the rollback a single write.
    pub fn publish_build(env: Env, tag: String, wasm_hash: BytesN<32>) {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        fleet::publish(&env, &tag, &wasm_hash);
    }

    /// Repoints a live tag at another tag's build, moving every instance that resolves
    /// through it.
    ///
    /// This is both the upgrade and the rollback: promote the new build's tag to ship,
    /// and the previous build's tag to undo. Neither needs a redeploy, and neither needs
    /// a migration — which is why a bad state migration is not a catastrophe as long as
    /// the previous build was frozen under a tag first.
    pub fn promote_fleet(
        env: Env,
        live_tag: String,
        candidate_tag: String,
    ) -> Result<BytesN<32>, MigrationError> {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        fleet::promote(&env, &live_tag, &candidate_tag)
    }

    /// Keeps a tag's reference entry from archiving.
    ///
    /// If the entry archives, every instance that resolves through it stops being
    /// loadable — and the application cannot restore it, because it does not own the
    /// entry's contents. Fleets have to do this on a schedule.
    pub fn extend_tag_ttl(env: Env, tag: String) {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        fleet::extend_ttl(&env, &tag, TAG_TTL_THRESHOLD, TAG_TTL_EXTEND_TO);
    }

    /// Points this contract's own executable at a tag's build.
    ///
    /// The change applies only if the invocation succeeds, so an upgrade that fails
    /// halfway leaves the old code running. That guarantee comes from the protocol, not
    /// from this function, which is why there is nothing here to get wrong.
    pub fn adopt_build(env: Env, owner: Address, tag: String) {
        let admin = Self::require_admin(&env);
        admin.require_auth();
        fleet::adopt_executable_ref(&env, &owner, &tag);
    }

    // --- internals -------------------------------------------------------

    /// Writes a balance and registers its key in the framework's index.
    ///
    /// [`store::put`] is the whole write path for a reason: it registers the key under
    /// whichever version is current — the migration's target while one is in flight, the
    /// recorded version otherwise — so the index can never describe a shape the data is
    /// not in.
    fn write_balance(env: &Env, owner: &Address, balance: &BalanceV2) {
        let key = DataKey::Balance(owner.clone()).into_val(env);
        store::put::<DataKey, BalanceV2>(env, &key, balance);
    }

    /// Recomputes and stores the running total.
    ///
    /// The counter is indexed like anything else, and deliberately so: it is derived
    /// state, and a future migration that needed to know whether it had seen this entry
    /// would have no way to ask if it had never been registered.
    fn bump_stats(env: &Env, amount: i128) {
        let mut stats = Self::read_stats(env);
        stats.total += amount;
        store::put::<DataKey, StatsV2>(env, &DataKey::Stats.into_val(env), &stats);
    }

    fn read_stats(env: &Env) -> StatsV2 {
        env.storage()
            .persistent()
            .get(&DataKey::Stats)
            .unwrap_or(StatsV2 { total: 0 })
    }

    fn require_admin(env: &Env) -> Address {
        // Panics when unauthorized, which is what `require_auth` is for. Panicking
        // rather than returning an error keeps the framework's error codes intact
        // through every entry point instead of mapping them onto a contract-specific
        // enum and losing the detail an operator needs.
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("contract is not initialized")
    }

    /// Keeps the instance entry — which holds the version and the migration cursor —
    /// alive across a migration that may span hours of operator attention.
    fn extend_instance(env: &Env) {
        env.storage().instance().extend_ttl(
            executor::INSTANCE_TTL_THRESHOLD,
            executor::INSTANCE_TTL_EXTEND_TO,
        );
    }
}
