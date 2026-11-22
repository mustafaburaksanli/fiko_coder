# Changelog

## v1.2.2 — experimental open-source source distribution (2026-11-22)

FİKO CODER is a local-first Rust coding agent for Ollama-compatible models. Version 1.2.2 publishes the independently maintained code under Apache-2.0. It is **experimental**, not a production-ready security boundary.

### Included

- Rust-controlled intent gates: Analyze, Explain, Generate, Modify, Test, and Unknown
- Read/search tools and guarded single-file or multi-file patch paths
- Verified candidate matching, source hash and project-fingerprint checks, explicit human approval, bounded retries and rollback handling
- Cargo verification, visible changes, and host-generated execution-status reporting rather than trusting model text
- Optional PostgreSQL task memory with separated migrations, project/file observations, and historical outcomes
- English README, security and contribution guidance, and a reproducible Windows + local Qwen acceptance walkthrough

### Reproducible evidence

- [Windows CI on the merged source](https://github.com/mustafaburaksanli/fiko_coder/actions/runs/45040120636): formatting, Cargo build checks, strict Clippy, offline tests, and 40 Windows stabilization repeats completed successfully
- [PostgreSQL CI on the merged source](https://github.com/mustafaburaksanli/fiko_coder/actions/runs/45040120648): PostgreSQL 17 acceptance against isolated temporary test databases completed successfully
- [PostgreSQL PR acceptance](https://github.com/mustafaburaksanli/fiko_coder/actions/runs/45038843308): 12/12 real database tests and 1/1 controlled-HTTP agent integration acceptance
- [Earlier Windows stress PR run](https://github.com/mustafaburaksanli/fiko_coder/actions/runs/45036862325) and [push run](https://github.com/mustafaburaksanli/fiko_coder/actions/runs/45036857375): together 80 successful stabilization test processes, 400 successful stabilization test cases
- One maintainer-run Windows + local `qwen-code-local:9b` acceptance exercise demonstrated an authorized patch and preserved `src/lib.rs` hash. [Reproduce it](docs/live-qwen-acceptance.md)

### Known limitations

- A temporary Cargo workspace is **not** an OS sandbox. Build scripts, procedural macros, and tests may execute with the user's privileges.
- Multi-file edits and rollback are not crash-atomic, and compilation does not prove intended behavior.
- Local model outputs are probabilistic and may fail; the distinct single-file live Qwen path has not yet been independently validated in the cited workspace exercise.
- No claims of independent third-party adoption, a verified broad security audit, or production readiness are made.
- Production PostgreSQL credentials must **never** be used for acceptance tests.

### Getting started

See the [README](README.md), [architecture and security model](docs/architecture.md), and [security policy](SECURITY.md). The Cargo package version is `1.2.2`; distribution of source code does not itself imply that prebuilt binaries are available.
