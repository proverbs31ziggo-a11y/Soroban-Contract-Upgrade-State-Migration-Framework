# Changelog

All notable changes to this project are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## A note on what counts as a breaking change

This is a library that a deployed contract compiles against, so the versioning rules are
stricter than semver's usual reading, and two categories matter enough to call out:

* **A stricter `check` verdict is a breaking change, even though no API changed.** If a
  change that previously passed the gate is now refused, a consumer's CI starts failing
  on an upgrade they did not make. Those releases are recorded under `Changed` with the
  reason, and the corresponding `ChangeKind` named.
* **Generated migration output is part of the public API.** If `generate` emits different
  code for an unchanged plan, an existing contract's committed migration will no longer
  match what the tool would produce. Those are recorded too.

Everything is `0.x` until the first stable release, so minor versions may contain breaking
changes; each one is listed rather than left to be discovered.

## [Unreleased]

### Added

* `LICENSE` containing the Apache License 2.0, which all four crate manifests already
  declared.
* This changelog.
* **The key space is now checked, and committed as `migrations/keyspace.json`.**
  `soroban-migrate check` previously reported `verdict: safe` about a change to the
  contract's `DataKey` enum, because the diff only ever compared storage shapes. It now
  diffs the enum and refuses the changes that make stored keys undecodable. See "Changed"
  below for what moved from allowed to refused, since that is the part that can break a
  build.
* `Network::stellar_timeout_seconds` bounds a single `stellar` invocation, defaulting to
  120 seconds; `0` restores the previous unbounded behaviour.

### Changed

* **A changed, removed, or renamed key-space variant is now refused, where it was
  previously reported as safe.** Also refused: a change to a variant's payload, and a move
  of the `#[migration]` marker.

  This is a breaking change in the sense that matters most for this tool — a change that
  used to pass the gate can now fail it — so it is called out rather than listed. The
  classification follows the encoding rather than intuition: a variant rename or payload
  change is fatal because the discriminant on the wire is the variant's *name*, whereas
  reordering variants is safe, because the generated case list and dispatch arms are both
  built from declaration order. Reordering, and adding a variant, are reported as
  information rather than refused.

  A project adopting this will need one `soroban-migrate schema export` to commit its key
  space. Until it does, `check` refuses rather than passing, since with no baseline
  committed there is nothing to compare against.
* `check`'s repository-findings section now prints a severity per finding instead of
  assuming every one of them is fatal.
* A `stellar` invocation that exceeds its deadline is killed and reported as an
  *ambiguous* result, naming `soroban-migrate status` as the way to find out what actually
  happened, rather than as a plain failure. A rejection is safe to retry; a stall is not.

### Fixed

* `repository` and `homepage` in every manifest pointed at a placeholder organization that
  does not exist. Both now point at the real repository, as do the cross-reference links in
  the four crate READMEs.
* A `stellar` subprocess could block forever on a stalled RPC endpoint, leaving `run`
  apparently waiting on a slow batch, with no way to tell that apart from a hang.

[Unreleased]: https://github.com/proverbs31ziggo-a11y/Soroban-Contract-Upgrade-State-Migration-Framework/commits/main
