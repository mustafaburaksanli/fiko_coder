# FİKO CODER

**Local-first, human-approved code changes.** FİKO CODER is an experimental Rust coding agent for Ollama-compatible models. It treats generated text and tool requests as untrusted: Rust controls tool authorization, exact patch matching, source freshness, temporary verification, explicit user approval, and rollback handling.

The point is to separate *model suggestions* from *host-verified code changes*. A model saying "done" is never evidence that a file was written or that Cargo checks succeeded.

> **Status: v1.2.2 development / pre-release.** This repository is a work in progress, not a security sandbox. Do not use it on untrusted repositories or valuable working trees without independent backups.

## What it does

- Supports local models via Ollama (default: `qwen-code-local:9b`).
- Classifies Analyze, Explain, Generate, Modify, Test and Unknown intents before tool execution.
- Provides bounded source reading, search, and Cargo check/test tools.
- Prepares single-file or multi-file code patches, verifies candidates in a temporary project copy, requests human approval, and runs post-apply checks.
- Supports strict patch matching and explicitly opted-in LF/CRLF recovery with source hash checks.
- Records limited task and file metadata in optional PostgreSQL memory; memory cannot authorize code changes.
- Tracks real tool execution outcomes rather than trusting the language model's claims.

## Reproducible evidence

- **Automated checks:** Windows GitHub Actions runs formatting, `cargo check --locked`, strict Clippy, and `cargo test --locked` on the review branch.
- **Local automated run (2026-11-22):** 338 tests passed, 0 failed, 13 skipped. Skipped tests are opt-in PostgreSQL integration/agent tests, **not** successful database acceptance results.
- **Updated Windows GitHub Actions (commit `56cea37`, 2026-11-22):** 339 passing tests, 0 failed, 13 ignored; both push and pull-request workflows succeeded. This includes an additional 16-thread fixture-uniqueness regression. Earlier Windows CI showed intermittent `0xc0000409` aborts; the test fixture collision and cleanup risks were addressed, but repeated stress validation is still recommended.
- **Live local model acceptance (2026-11-22):** A single Windows + `qwen-code-local:9b` exercise showed `VERIFY_WORKSPACE_OK`, explicit approval, `PATCH_VERIFIED`, 3 → 4 passing Rust math tests, and an unchanged protected `src/lib.rs` SHA-256 hash.
- **Repeat it yourself:** [Step-by-step local Qwen acceptance demo](docs/live-qwen-acceptance.md). This exercises the workspace path; separate single-file handoff has unit/integration regression tests but has not been live-validated in that exercise.

The local exercise is one maintainer-run observation, **not** a reliability benchmark or evidence of third-party adoption. See [SECURITY.md](SECURITY.md) for limitations.

## Requirements

- Windows with Rust stable (edition 2024), Cargo, and a running Ollama service.
- A local Ollama model suitable for tool calling (the default tag is user-configurable).
- PostgreSQL is **optional**; ordinary use and default unit tests do not require it.

## Build and run

```powershell
git clone https://github.com/mustafaburaksanli/fiko_coder.git
cd fiko_coder
git switch fix/verified-handoff-and-oss-readiness-v1.2.2
cargo build --locked
cargo run -- qwen-code-local:9b
```

**Note:** The default GitHub branch currently points to an older development version. Switch to the newer branch explicitly until a reviewed release is merged.

The agent operates on **the current working directory**, not automatically the directory containing its executable. Open the target Rust/Cargo project first, and invoke the compiled agent by absolute path:

```powershell
# In the FIKO CODER checkout:
cargo build --release --locked
$FikoExe = (Resolve-Path .\target\release\fiko_coder.exe).Path

# In a separate disposable Rust project:
Set-Location C:\path\to\disposable-rust-project
& $FikoExe qwen-code-local:9b
```

In the interactive CLI, type `geri al` / `rollback` to request a rollback of the last patch during the current session; type `cikis` to exit.

## Verification and tests

```powershell
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

PostgreSQL integration tests are opt-in and must use a **separate local test database** with the environment variable `FIKO_TEST_DATABASE_URL`. See [PostgreSQL setup](docs/postgresql-memory.md). Never run a migration or integration test against a production database.

## Security limitations

- A temporary source copy is **not an OS process sandbox**. `cargo check` and `cargo test` can execute build scripts, procedural macros, or tests with the user's OS privileges.
- Patch transactions are not guaranteed to be crash-atomic; partial filesystem failures require manual inspection.
- Compiling a patch does not prove it meets the requested functionality. Review diffs and run relevant tests.
- A local model can fail to call tools or produce incorrect code. No model prose counts as proof of applied changes.
- Changes require explicit user approval; never treat a model message as consent.
- PostgreSQL storage is optional and observational, not an execution authority.

Read [SECURITY.md](SECURITY.md) before using on real projects.

## Project structure

| Path | Responsibility |
| --- | --- |
| `src/agent.rs` | Ollama session, candidate verification and guarded apply |
| `src/orchestrator.rs` | Intent gating, tool validation and task-state reporting |
| `src/repair_controller.rs` | Verified candidate lock, retry limits and task state |
| `src/tools.rs` | File tools, patch matching and Cargo process results |
| `src/verification.rs` | Temporary workspace compilation and checks |
| `src/workspace_patch.rs` | Multi-file patch preparation and rollback |
| `src/memory*.rs` | Optional bounded PostgreSQL task memory |
| `migrations/` | Database schema changes |
| `tests/` | Integration tests |

## Contributing and licensing

See [CONTRIBUTING.md](CONTRIBUTING.md). This review branch **proposes** the **Apache License 2.0** for code contained in this repository (see [LICENSE](LICENSE)); the maintainer must approve this license decision before the branch is merged or a release is published.
