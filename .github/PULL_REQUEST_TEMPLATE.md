Closes #

## What this changes

<!-- One or two sentences. What was wrong or missing, and what is different now. -->

## How it was tested

<!-- Which tests you added, and what they would catch. -->

## Checklist

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items`
- [ ] `CHANGELOG.md` updated under `[Unreleased]` (if a user would notice the change)
- [ ] If `check` now refuses something it used to allow, or `generate` output changed, that is called out as breaking in the changelog
