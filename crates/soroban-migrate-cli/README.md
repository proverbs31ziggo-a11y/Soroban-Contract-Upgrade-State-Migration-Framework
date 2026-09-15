# soroban-migrate-cli

The `soroban-migrate` binary. See the
[root README](https://github.com/soroban-migrate/soroban-migrate#readme) for the full
walkthrough.

## Commands

| command | needs a network | what it does |
| --- | --- | --- |
| `init` | no | Write `soroban-migrate.toml` and snapshot the shapes the source declares. |
| `schema export` | no | Snapshot every `#[storage_schema]` declaration. `--check` writes nothing and fails if a snapshot is out of date — this is the CI gate against a shape that changed without its version changing. |
| `schema list` / `schema show` | no | What the repository has committed, per shape, with any version jump flagged. |
| `check` | no | Diff every consecutive version pair, apply its plan, and refuse anything that would lose data or orphan entries. Exits `2` on refusal, `1` on a broken repository. |
| `plan` | no | Draft the declarations a diff demands, with a placeholder where a human has to decide. |
| `generate` | no | Write the migration implementation. Refuses while a placeholder remains, and refuses a denied diff. |
| `status` | yes | A deployed contract's schema version and migration progress, read-only. |
| `run` | yes | Drive batches. Simulates unless `--submit` is passed. |
| `dry-run` | a snapshot | Replay the whole migration against a forked ledger and report what each batch would cost. |

## Exit codes

`0` succeeded, `1` the repository or the invocation is wrong, `2` the tool ran and
refused. CI needs the last two apart: one is fixed by editing the repository, the other
by deciding not to ship the change.

## Network access

The commands that talk to a contract delegate transaction construction, signing and
submission to the [Stellar CLI](https://developers.stellar.org/docs/tools/cli/stellar-cli),
which is also where the operator's keys live. **This tool never handles a secret key.**

`dry-run` is different: it loads a ledger snapshot into a local host and replays the
migration in-process, because a simulation only answers for the *next* batch — later
batches depend on state the earlier ones wrote, and a simulation writes nothing.

## License

Apache-2.0.
