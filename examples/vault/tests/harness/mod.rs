//! Shared setup for the example's tests.
//!
//! # Why seeding writes storage directly
//!
//! The interesting state to test a migration against is state written by the *old*
//! build, and the old build no longer exists by the time the test runs. There are two
//! ways to get it: keep a second copy of the contract compiled from the old source, or
//! write the old shape into storage directly.
//!
//! The second is what this harness does, and it is the better test. A second contract
//! can only produce old-shaped state through its own entry points, which means the test
//! exercises two contract builds and inherits every difference between them. Writing
//! `BalanceV1` into the contract's storage and registering the key under version 1 does
//! exactly what the old build did, and nothing else.

#![allow(dead_code)]

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env, IntoVal, Val};

use soroban_migrate::index;
use vault_example::keys::DataKey;
use vault_example::schema::{BalanceV1, BalanceV2};
use vault_example::{Vault, VaultClient};

/// A registered, initialized contract with resource limits relaxed.
pub struct Fixture {
    pub env: Env,
    pub id: Address,
    pub admin: Address,
}

impl Fixture {
    /// The client for this contract.
    pub fn client(&self) -> VaultClient<'_> {
        VaultClient::new(&self.env, &self.id)
    }

    /// Writes `count` accounts' balances in the **version-1** shape and registers their
    /// keys under version 1, exactly as the version-1 build would have.
    ///
    /// Returns the addresses, so a test can assert on a specific account afterwards.
    pub fn seed_v1(&self, count: u32) -> soroban_sdk::Vec<Address> {
        let mut owners = soroban_sdk::Vec::new(&self.env);
        let mut n = 0u32;
        while n < count {
            let owner = Address::generate(&self.env);
            owners.push_back(owner.clone());
            n += 1;
        }

        self.env.as_contract(&self.id, || {
            let mut i = 0u32;
            while i < count {
                let owner = owners.get(i).expect("indexed above");
                let key: Val = DataKey::Balance(owner.clone()).into_val(&self.env);
                self.env.storage().persistent().set::<Val, BalanceV1>(
                    &key,
                    &BalanceV1 {
                        owner: owner.clone(),
                        amount: 1_000 + i as i128,
                    },
                );
                // Registration is what makes the entry findable. A version-1 build
                // that never registered keys is the failure mode `preflight` exists to
                // catch, and it is tested separately.
                index::register::<DataKey>(&self.env, 1, &key);
                i += 1;
            }
        });
        owners
    }

    /// Reads an account's balance using the version-2 shape, as a live contract does.
    pub fn balance(&self, owner: &Address) -> Option<BalanceV2> {
        self.client().balance(owner)
    }

    /// Whether `entry` has already been through the migration.
    ///
    /// The migration's own definition of "done", which is what the framework's
    /// idempotency depends on: `frozen` present means the entry carries a value the
    /// migration put there or the application did.
    pub fn migrated(&self, owner: &Address) -> bool {
        self.balance(owner).is_some_and(|b| b.frozen.is_some())
    }

    /// How many of `owners` have been migrated.
    pub fn migrated_count(&self, owners: &soroban_sdk::Vec<Address>) -> u32 {
        let mut done = 0u32;
        let mut i = 0u32;
        while i < owners.len() {
            if self.migrated(&owners.get(i).unwrap()) {
                done += 1;
            }
            i += 1;
        }
        done
    }

    /// How many keys the framework's index holds for `version`.
    ///
    /// Framework storage is addressed relative to the *current contract*, so this has
    /// to be asked from inside the contract's frame. That is not a test artefact: it is
    /// why nothing outside the contract can enumerate its storage, which is the
    /// constraint the whole design is built around.
    pub fn index_len(&self, version: u32) -> u32 {
        self.env
            .as_contract(&self.id, || index::len::<DataKey>(&self.env, version))
    }
}

/// Extracts the contract error from a `try_` client call.
///
/// The generated `try_` variants have four outcomes, and collapsing them into
/// `Result<Result<..>>` at the call site is how a test ends up passing for the wrong
/// reason. The SDK's shape is:
///
/// ```text
/// Result<
///     Result<SuccessType, ConversionError>,   // did the *return value* convert?
///     Result<ContractError, InvokeError>,     // did the *contract* return an error, or
///                                             // did the invocation fail outright?
/// >
/// ```
///
/// Only one of the four means "the contract refused". This helper returns that one and
/// panics on the other three, so a test that meant to assert a refusal but actually got
/// a failed invocation says so.
pub fn contract_error<T, C, E, I>(result: Result<Result<T, C>, Result<E, I>>) -> E
where
    T: core::fmt::Debug,
    C: core::fmt::Debug,
    I: core::fmt::Debug,
{
    match result {
        Err(Ok(e)) => e,
        Err(Err(invoke)) => panic!("the invocation failed rather than returning: {invoke:?}"),
        Ok(Ok(value)) => panic!("expected a contract error, got success: {value:?}"),
        Ok(Err(conversion)) => panic!(
            "the call succeeded but its return value did not convert, which is a contract bug \
             rather than a refusal: {conversion:?}"
        ),
    }
}

/// Registers and initializes the contract, with resource limits relaxed so that tests
/// can build keyspaces too large to fit in one transaction.
///
/// Limits are re-enabled explicitly by the tests that measure against them; see
/// `tests/atomicity.rs`.
pub fn fixture() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().disable_resource_limits();

    let id = env.register(Vault, ());
    let admin = Address::generate(&env);
    VaultClient::new(&env, &id).initialize(&admin);

    Fixture { env, id, admin }
}
