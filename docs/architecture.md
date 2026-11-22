# Architecture and trust boundaries

FİKO CODER is a **local-first, human-approved Rust coding agent**. It connects to a locally configured Ollama-compatible model, but the model is not the authority for project writes, successful tests, or user approval.

## Control flow

```text
User request
   |
Rust intent classification and write-authority gate
   |
Model-assisted reads/search or patch proposal
   |
Rust tool schema, path and project-root validation
   |
Verification of candidate in a temporary workspace
   |
Rust-generated diff and explicit terminal approval
   |
Freshness / project identity / candidate lock re-check
   |
Guarded apply, Cargo check/test, rollback on handled failures
   |
Rust-owned status report and optional PostgreSQL observation
```

## Component responsibilities

| Code | Responsibility |
| --- | --- |
| `src/intent.rs` | Intent recognition and scoped permissions |
| `src/orchestrator.rs` | Tool authorization, argument validation, task state and authoritative status |
| `src/agent.rs` | Model-facing session and guarded tool orchestration |
| `src/verification.rs` | Candidate verification and source freshness checks |
| `src/repair_controller.rs` | Verified-patch handoff and bounded repair state |
| `src/workspace_patch.rs` | Multi-file diff, protected paths, apply and recovery |
| `src/tools.rs` | Limited filesystem and Cargo execution operations |
| `src/memory*.rs` | Optional PostgreSQL persistence; never grants edit rights |
| `tests/` | Offline regressions, approval controls, integration and failure tests |

## Proof boundaries

- **Evidence from the Rust host:** inspected file bytes, exact patch candidate, recorded user response, process exit code, test output, observed project state
- **Untrusted data:** model-generated prose or tool calls, repository content, prior memory, and proposed patches
- **Never infer success from model prose.** A statement such as “I changed the file and all tests passed” is not evidence until the tool host recorded those actions.
- **Memory is not a permission system.** Prior success cannot override protected paths, a changed source hash, or an unapproved candidate.
- **Temporary workspace is not isolation.** Cargo may run build scripts, procedural macros and tests with the same filesystem/network privileges as the process.
- **No crash-atomic guarantee.** External writers and abrupt termination can defeat assumptions. Work on disposable or backed-up projects and inspect results.

## Independent verification

1. Follow [the live Qwen demo](live-qwen-acceptance.md) in a disposable crate. Review the candidate diff and verify the protected file's SHA-256 independently.
2. Run `cargo fmt --all -- --check`, `cargo check --locked`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`.
3. Follow [PostgreSQL local-test setup](postgresql-memory.md) only with a **separate, local test database**; CI uses disposable PostgreSQL 17 service containers.
4. Review [the Windows CI workflow](../.github/workflows/rust-ci.yml), [PostgreSQL CI workflow](../.github/workflows/postgres-acceptance.yml), and [changelog](../CHANGELOG.md) for results and limitations.

This overview is a software control-flow explanation, not a formal security certification.
