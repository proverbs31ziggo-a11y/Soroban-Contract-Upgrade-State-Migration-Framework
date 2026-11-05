# Security policy

soroban-migrate rewrites the persistent state of deployed contracts. A bug here can
make user funds unreadable, so security reports are treated as the highest priority.

## Supported versions

The project is pre-1.0. Only the latest `0.x` release and `main` receive fixes.

## Reporting a vulnerability

**Do not open a public issue.** Use GitHub's private reporting instead:
[Report a vulnerability](https://github.com/proverbs31ziggo-a11y/Soroban-Contract-Upgrade-State-Migration-Framework/security/advisories/new).

Please include:

* what an attacker or a mistaken operator can do,
* the crate and version (or commit),
* a test or steps that reproduce it, if you have them.

You can expect an acknowledgement within **72 hours** and a first assessment within
**7 days**. Once a fix is released we will publish an advisory and credit you, unless you
ask not to be credited.

## Threat model

What the framework is responsible for, and what it is not.

### In scope

| risk | how it is handled |
| --- | --- |
| **An unsafe schema change ships** — a required field is added, a field is dropped silently, a key-space variant is renamed | `soroban-migrate check` refuses it and exits `2`. Refusal is the default for any change not proven safe. |
| **A migration leaves entries behind** — entries written under the old shape are never visited | Every application write goes through the key index; `preflight` refuses to start a migration that would orphan live entries. |
| **A half-applied batch** — a batch fails partway through | A batch is one contract invocation, so the network discards every write it made, including the cursor. Retrying is always safe. |
| **A batch exceeds network limits** and can never be submitted | `MAX_KEYS_PER_BATCH` guards the size; `tests/resource_limits.rs` fails if a batch gets heavier; `dry-run` measures the real contract. |
| **An unauthorised caller drives a migration** | The framework never authorises anything itself. The host contract must gate its migration entry points with `require_auth` on an admin it controls — see `examples/vault`. |
| **Key collisions** between the framework and the application | The framework reaches storage only through a variant the application declares in its own key enum, so a collision cannot compile. |
| **The CLI leaks a secret** | The CLI never handles secret keys; signing and submission are delegated to `stellar`. |
| **A stalled RPC looks like progress** | `stellar` invocations are bounded by a timeout, reported as an ambiguous result that points at `status`. |

### Out of scope

* **The correctness of your conversion logic.** If a migration computes the wrong value,
  it computes it consistently. `check` proves the shapes are compatible, not that your
  arithmetic is right. Test your migrations against real entry shapes.
* **Admin key compromise.** Whoever holds the admin key can upgrade the contract to any
  code at all, with or without this framework.
* **Keys written outside the framework's store.** Entries the application writes directly,
  bypassing the index, cannot be found and cannot be migrated.
* **Bugs in Soroban, `soroban-sdk`, or `stellar-cli`.** Please report those upstream; we
  are happy to help reproduce them.
* **Temporary storage.** Not supported yet (see the issue tracker); do not rely on the
  framework to migrate it.

## Known limitations

`run`, `status` and `dry-run` are tested up to the network boundary but have not yet been
run against a live RPC endpoint. Treat your first real run as the first real run: try it
on Testnet before Mainnet.
