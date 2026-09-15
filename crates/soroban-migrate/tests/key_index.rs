//! Key index behaviour, with an emphasis on the page boundaries where an
//! off-by-one produces a *silently* short index — and therefore a migration that
//! reports success without having migrated everything.

mod support;

use soroban_sdk::{Env, Val, Vec};

use soroban_migrate::index::{self, PAGE_SIZE};
use soroban_migrate::MigrationError;

use support::{assert_keys_eq, entry_key, host, keys_equal, seed_v1, DataKey};

fn with_env<T>(f: impl FnOnce(&Env) -> T) -> T {
    let env = Env::default();
    let id = host(&env);
    env.as_contract(&id, || f(&env))
}

/// Builds the key list a range read should return for payload range `start..start+count`.
fn expected(env: &Env, start: u32, count: u32) -> Vec<Val> {
    let mut v = Vec::new(env);
    let mut n = start;
    while n < start + count {
        v.push_back(entry_key(env, n));
        n += 1;
    }
    v
}

#[test]
fn an_index_that_was_never_written_does_not_exist() {
    with_env(|env| {
        assert_eq!(index::len::<DataKey>(env, 1), 0);
        assert!(!index::exists::<DataKey>(env, 1));
        assert_eq!(index::footprint_entries::<DataKey>(env, 1), 0);
    });
}

#[test]
fn registering_one_key_allocates_the_first_page() {
    with_env(|env| {
        index::register::<DataKey>(env, 1, &entry_key(env, 0));
        let info = index::info::<DataKey>(env, 1).expect("index should exist");
        assert_eq!(info.len, 1);
        // The first page is allocated the moment the first key lands in it, so
        // `page_count` is never zero for an index that exists.
        assert_eq!(info.page_count, 1);
        assert_eq!(index::footprint_entries::<DataKey>(env, 1), 2);
    });
}

#[test]
fn a_page_holds_exactly_page_size_keys() {
    with_env(|env| {
        seed_v1(env, PAGE_SIZE);
        let info = index::info::<DataKey>(env, 1).unwrap();
        assert_eq!(info.len, PAGE_SIZE);
        assert_eq!(info.page_count, 1);

        index::register::<DataKey>(env, 1, &entry_key(env, PAGE_SIZE));
        let info = index::info::<DataKey>(env, 1).unwrap();
        assert_eq!(info.len, PAGE_SIZE + 1);
        // The 33rd key must roll onto a second page. If it did not, it would
        // overwrite the 32nd and the index would be short by one key forever.
        assert_eq!(info.page_count, 2);
    });
}

#[test]
fn keys_survive_page_boundaries() {
    with_env(|env| {
        let total = PAGE_SIZE * 3 + 7;
        seed_v1(env, total);
        assert_eq!(index::len::<DataKey>(env, 1), total);
        assert_eq!(index::info::<DataKey>(env, 1).unwrap().page_count, 4);

        let mut n = 0u32;
        while n < total {
            let actual = index::get::<DataKey>(env, 1, n).unwrap();
            assert!(
                keys_equal(env, &actual, &entry_key(env, n)),
                "key {n} did not round-trip through the index"
            );
            n += 1;
        }
    });
}

#[test]
fn reading_past_the_end_is_reported_as_corruption() {
    with_env(|env| {
        seed_v1(env, 3);
        // Not a silent `None`: a key the summary says exists but that the page
        // does not hold means the index is inconsistent, and a migration walking
        // it must stop rather than skip.
        assert_eq!(
            index::get::<DataKey>(env, 1, 3).unwrap_err(),
            MigrationError::CorruptIndex
        );
    });
}

#[test]
fn range_reads_across_page_boundaries() {
    with_env(|env| {
        seed_v1(env, 100);
        let keys = index::range::<DataKey>(env, 1, 30, 5).unwrap();
        assert_keys_eq(env, &keys, &expected(env, 30, 5));
    });
}

#[test]
fn range_spans_four_pages_without_duplicating_or_dropping() {
    with_env(|env| {
        seed_v1(env, 100);
        // 31..97, so page 0 from offset 31, pages 1 and 2 in full, page 3 up to
        // offset 0. This is the shape of a real batch straddling boundaries.
        let keys = index::range::<DataKey>(env, 1, 31, 66).unwrap();
        assert_keys_eq(env, &keys, &expected(env, 31, 66));
    });
}

#[test]
fn range_clamps_at_the_end_and_handles_degenerate_windows() {
    with_env(|env| {
        seed_v1(env, 10);

        // Asking for more than exists returns what exists rather than failing:
        // running off the end is how a migration finishes.
        assert_keys_eq(
            env,
            &index::range::<DataKey>(env, 1, 8, 100).unwrap(),
            &expected(env, 8, 2),
        );
        assert_eq!(index::range::<DataKey>(env, 1, 10, 5).unwrap().len(), 0);
        assert_eq!(index::range::<DataKey>(env, 1, 500, 5).unwrap().len(), 0);
        assert_eq!(index::range::<DataKey>(env, 1, 0, 0).unwrap().len(), 0);
        assert_keys_eq(
            env,
            &index::range::<DataKey>(env, 1, 0, 10).unwrap(),
            &expected(env, 0, 10),
        );
    });
}

#[test]
fn ranges_tile_the_index_exactly() {
    with_env(|env| {
        // The property a batched migration depends on: concatenating the windows
        // a cursor produces must reproduce the index exactly, with no key visited
        // twice and none missed.
        let total = 137u32;
        seed_v1(env, total);

        let mut visited = Vec::new(env);
        let mut cursor = 0u32;
        while cursor < total {
            let window = index::range::<DataKey>(env, 1, cursor, 32).unwrap();
            let mut i = 0u32;
            while i < window.len() {
                visited.push_back(window.get(i).unwrap());
                i += 1;
            }
            cursor += window.len();
        }

        assert_eq!(visited.len(), total);
        assert_keys_eq(env, &visited, &expected(env, 0, total));
    });
}

#[test]
fn contains_finds_keys_on_any_page() {
    with_env(|env| {
        seed_v1(env, 70);
        assert!(index::contains::<DataKey>(env, 1, &entry_key(env, 0)).unwrap());
        assert!(index::contains::<DataKey>(env, 1, &entry_key(env, 31)).unwrap());
        assert!(index::contains::<DataKey>(env, 1, &entry_key(env, 32)).unwrap());
        assert!(index::contains::<DataKey>(env, 1, &entry_key(env, 69)).unwrap());
        assert!(!index::contains::<DataKey>(env, 1, &entry_key(env, 70)).unwrap());
    });
}

#[test]
fn duplicate_registration_is_recorded_and_is_not_an_error() {
    with_env(|env| {
        // The index is a log, not a set. Deduplicating on write would cost a full
        // scan per application write, so duplicates are expected — and the
        // idempotency requirement on `Migration::up` is what makes them safe.
        seed_v1(env, 1);
        seed_v1(env, 1);
        assert_eq!(index::len::<DataKey>(env, 1), 2);
        let first = index::get::<DataKey>(env, 1, 0).unwrap();
        let second = index::get::<DataKey>(env, 1, 1).unwrap();
        assert!(
            keys_equal(env, &first, &second),
            "the same key registered twice must resolve to the same entry"
        );
    });
}

#[test]
fn indexes_for_different_versions_are_independent() {
    with_env(|env| {
        seed_v1(env, 5);
        index::register::<DataKey>(env, 2, &entry_key(env, 99));
        assert_eq!(index::len::<DataKey>(env, 1), 5);
        assert_eq!(index::len::<DataKey>(env, 2), 1);
        assert!(!index::contains::<DataKey>(env, 1, &entry_key(env, 99)).unwrap());
        assert!(index::contains::<DataKey>(env, 2, &entry_key(env, 99)).unwrap());
    });
}

#[test]
fn reset_makes_an_index_logically_empty_and_reusable() {
    with_env(|env| {
        seed_v1(env, 40);
        assert_eq!(index::len::<DataKey>(env, 1), 40);

        index::reset::<DataKey>(env, 1);
        // The pages still exist in storage, but `len` is what bounds every read,
        // so they are unreachable.
        assert_eq!(index::len::<DataKey>(env, 1), 0);
        assert!(!index::exists::<DataKey>(env, 1));
        assert_eq!(index::range::<DataKey>(env, 1, 0, 100).unwrap().len(), 0);
        assert!(!index::contains::<DataKey>(env, 1, &entry_key(env, 0)).unwrap());

        // And the index refills from page zero, overwriting the orphaned pages.
        seed_v1(env, 3);
        assert_eq!(index::len::<DataKey>(env, 1), 3);
        assert_eq!(index::info::<DataKey>(env, 1).unwrap().page_count, 1);
        assert!(keys_equal(
            env,
            &index::get::<DataKey>(env, 1, 0).unwrap(),
            &entry_key(env, 0)
        ));
    });
}
