//! The on-chain key index: how a batched migrator finds work without a storage
//! scan.
//!
//! Soroban has no primitive for enumerating a contract's storage, on-chain or
//! over RPC. `getLedgerEntries` requires exact keys, and there is no host
//! function that walks contract data. A contract therefore cannot ask "which
//! entries do I have?" — it can only be *told*.
//!
//! That single missing primitive is why migrating thousands of entries is hard,
//! and it is what this module exists to paper over. The index is an append-only,
//! paged list of the keys a contract has written, recorded as they are written.
//! It costs one extra read-modify-write per application write, in exchange for
//! making enumeration a *bounded* operation: the executor resumes a migration
//! from a cursor with a number of reads proportional to the batch size, not to
//! the size of the state.
//!
//! # Cost model
//!
//! Registering a key touches:
//!
//! * 1 read of the target page (32 keys per read, so amortized 1/32 of a read)
//! * 1 write of that page
//! * 1 read + 1 write of [`KeyIndexInfo`] to bump `len`
//!
//! Against Mainnet's 200-read / 200-write cap that is nowhere near the limit; it
//! is a fraction of a cent per write in rent and fees. Contracts that cannot pay
//! it run without an index and are migrated from a CLI-supplied key manifest
//! instead — see the CLI's `--manifest` flag.
//!
//! # Why pages and not one `Vec`
//!
//! A single `Vec<Val>` holding every key would pass the 64 KiB per-entry limit
//! at roughly a thousand keys, and rewriting it on every registration would make
//! each application write cost O(n). Pages keep every operation O(1) and the
//! size of any single entry bounded.

use soroban_sdk::{Env, Val, Vec};

use crate::error::MigrationError;
use crate::key::{storage_key, KeyIndexInfo, Keyspace, MigrationKey};

/// Keys per index page.
///
/// 32 fits comfortably inside the 64 KiB entry limit for realistic 32-byte keys
/// while keeping the amortized cost of a page read low: a 32-key page read
/// covers 32 keys for the price of one read, which is what keeps a 150-key batch
/// inside a 200-read budget.
pub const PAGE_SIZE: u32 = 32;

/// Reads the index summary for `version`, or `None` if nothing was ever
/// registered under it.
pub fn info<K: Keyspace>(env: &Env, version: u32) -> Option<KeyIndexInfo> {
    let key = storage_key::<K>(env, MigrationKey::IndexInfo(version));
    env.storage().instance().get::<Val, KeyIndexInfo>(&key)
}

/// Number of keys indexed under `version`. Zero when the index does not exist.
pub fn len<K: Keyspace>(env: &Env, version: u32) -> u32 {
    match info::<K>(env, version) {
        Some(i) => i.len,
        None => 0,
    }
}

/// True when `version` has a key index at all.
pub fn exists<K: Keyspace>(env: &Env, version: u32) -> bool {
    info::<K>(env, version).is_some()
}

/// Appends `key` to the index for `version`.
///
/// Registering the same key twice is allowed and is the normal case, not an
/// error: an entry migrated from v1 to v2 is registered under both, and an
/// application that rewrites a key under the current version registers it again
/// on every write. Deduplicating here would cost a full-index scan per write.
/// The executor therefore does not treat the index as a set of unique keys — it
/// treats [`crate::migration::Migration::up`] as the thing that must be
/// idempotent. See that method's documentation for why that is the right place
/// for the requirement.
///
/// # Footprint
///
/// Writes one page and the summary. Extends neither: both are persistent or
/// instance entries whose TTL is refreshed by the write itself, and the
/// instance entry is extended explicitly by the executor on every batch.
pub fn register<K: Keyspace>(env: &Env, version: u32, key: &Val) {
    let mut current = match info::<K>(env, version) {
        Some(i) => i,
        None => KeyIndexInfo {
            page_count: 0,
            len: 0,
        },
    };

    // The page holding slot `len` is always `len / PAGE_SIZE`, so a full page is
    // rolled onto the next one without needing to know whether it is full.
    let page_index = current.len / PAGE_SIZE;
    let mut page = read_page::<K>(env, version, page_index);
    page.push_back(*key);
    write_page::<K>(env, version, page_index, &page);

    current.len += 1;
    if page_index + 1 > current.page_count {
        current.page_count = page_index + 1;
    }

    let info_key = storage_key::<K>(env, MigrationKey::IndexInfo(version));
    env.storage()
        .instance()
        .set::<Val, KeyIndexInfo>(&info_key, &current);
}

/// Reads the key at `index` under `version`.
///
/// # Errors
///
/// [`MigrationError::CorruptIndex`] if `index` is inside the range the summary
/// advertises but the page does not hold a key there. The summary and the pages
/// are written in the same transaction, so this can only happen if something
/// outside the framework wrote to a framework key.
pub fn get<K: Keyspace>(env: &Env, version: u32, index: u32) -> Result<Val, MigrationError> {
    let page = read_page::<K>(env, version, index / PAGE_SIZE);
    match page.get(index % PAGE_SIZE) {
        Some(v) => Ok(v),
        None => Err(MigrationError::CorruptIndex),
    }
}

/// Reads up to `limit` contiguous keys starting at `start`, in index order.
///
/// The executor uses this rather than calling [`get`] in a loop, because one page
/// read covers [`PAGE_SIZE`] keys: fetching them individually would spend 32 of
/// the transaction's 200 reads on the same page. Returns fewer than `limit` keys
/// when `start + limit` runs past the end, and an empty vector when `start` is
/// already past the end — running off the end is a normal way to finish, not an
/// error.
pub fn range<K: Keyspace>(
    env: &Env,
    version: u32,
    start: u32,
    limit: u32,
) -> Result<Vec<Val>, MigrationError> {
    let mut out = Vec::new(env);
    if limit == 0 {
        return Ok(out);
    }
    let total = len::<K>(env, version);
    if start >= total {
        return Ok(out);
    }
    let end = core::cmp::min(start.saturating_add(limit), total);

    let first_page = start / PAGE_SIZE;
    let last_page = (end - 1) / PAGE_SIZE;
    let mut page_index = first_page;
    while page_index <= last_page {
        let page = read_page::<K>(env, version, page_index);
        let from = if page_index == first_page {
            start % PAGE_SIZE
        } else {
            0
        };
        let to = if page_index == last_page {
            ((end - 1) % PAGE_SIZE) + 1
        } else {
            PAGE_SIZE
        };
        let mut offset = from;
        while offset < to {
            match page.get(offset) {
                Some(v) => out.push_back(v),
                None => return Err(MigrationError::CorruptIndex),
            }
            offset += 1;
        }
        page_index += 1;
    }
    Ok(out)
}

/// Whether `key` appears anywhere in the index for `version`.
///
/// A linear scan of the whole index, so this is only for bounded checks — the
/// pre-flight path, where the CLI hands over a manifest of at most a few hundred
/// keys. It is deliberately not used by the executor.
pub fn contains<K: Keyspace>(env: &Env, version: u32, key: &Val) -> Result<bool, MigrationError> {
    let total = len::<K>(env, version);
    let mut page_index = 0;
    while page_index * PAGE_SIZE < total {
        let page = read_page::<K>(env, version, page_index);
        if page.contains(key) {
            return Ok(true);
        }
        page_index += 1;
    }
    Ok(false)
}

/// Discards the index for `version`, making it logically empty.
///
/// Only [`crate::rollback::begin`] calls this, and the reasoning is worth stating
/// because "delete the index" sounds like the exact opposite of what a migration
/// framework should do.
///
/// A forward migration carries *every* visited key into the new version's index,
/// including keys it did not manage. So the new index is always a superset of the
/// old one. Rebuilding the old index from scratch — reset, then re-register each
/// key as the rollback visits it — therefore reproduces it exactly, whereas
/// appending would double it. Doubling is not a correctness bug, because
/// [`register`] is a log and [`crate::migration::Migration::up`] is idempotent,
/// but it doubles the cost of every subsequent migration forever, and it would do
/// so in a way nobody would notice until the rent bill arrived.
///
/// The pages are left in place rather than deleted. `len` is what bounds every
/// read, so orphaned pages are unreachable, and the first write to each page
/// overwrites it as the index refills.
///
pub fn reset<K: Keyspace>(env: &Env, version: u32) {
    let info_key = storage_key::<K>(env, MigrationKey::IndexInfo(version));
    env.storage().instance().remove::<Val>(&info_key);
}

/// Number of ledger entries the index occupies: its pages plus the summary.
///
/// The CLI reports this before a migration so operators can see what the
/// framework's own bookkeeping costs on top of the entries being migrated.
pub fn footprint_entries<K: Keyspace>(env: &Env, version: u32) -> u32 {
    match info::<K>(env, version) {
        Some(i) => i.page_count + 1,
        None => 0,
    }
}

fn read_page<K: Keyspace>(env: &Env, version: u32, page_index: u32) -> Vec<Val> {
    let key = storage_key::<K>(env, MigrationKey::IndexPage(version, page_index));
    // An unwritten page reads as empty. Callers that need to distinguish "page
    // absent" from "page present but short" get that from the summary's `len`,
    // and a short page inside the advertised range surfaces as `CorruptIndex`.
    match env.storage().persistent().get::<Val, Vec<Val>>(&key) {
        Some(page) => page,
        None => Vec::new(env),
    }
}

fn write_page<K: Keyspace>(env: &Env, version: u32, page_index: u32, page: &Vec<Val>) {
    let key = storage_key::<K>(env, MigrationKey::IndexPage(version, page_index));
    env.storage().persistent().set::<Val, Vec<Val>>(&key, page);
}
