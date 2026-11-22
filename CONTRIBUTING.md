# Contributing

Thanks for helping improve FİKO CODER.

## Before submitting

- Open an issue for large architecture changes, especially security-sensitive behavior.
- Keep each patch focused; never mix an unrelated reformat/line-ending migration into a security fix.
- Do not commit model files, secrets, personal data, database dumps, or generated backups.
- All code changes must preserve Rust-side permission checks, Verified Patch Lock, fingerprint revalidation and rollback semantics.

## Development commands

```powershell
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

PostgreSQL tests are opt-in. Follow `docs/postgresql-memory.md` and use an isolated local test database.

## Pull requests

Describe the problem, design, files changed, proof from relevant tests, and known limitations. For changes to approval or patch application, include tests for user rejection, stale source, protected paths, malicious model prose, success, and failure/rollback.

**Do not state tests passed unless you ran them.** Attach command results and the target OS and Rust version.
