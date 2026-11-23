# Historical regression parity audit (PR #37)

This is a traceability record, **not** a claim that every historical acceptance scenario has independently been replayed.

## Scope and guardrails

- Target branch: `ci/macos-full-regression-parity-v1.3`, PR #37.
- GitHub PR search yielded PR #1, #2 and #4–#37 (36 entries); PR #3 did not appear in the result. Do not infer a test from the missing number.
- Current four-runner Rust matrix: Windows, Linux, native macOS Intel x86_64 and native macOS Apple Silicon arm64.
- `cargo test --locked` runs ordinary unit/integration tests. `scripts/ci_regressions.py ignored` runs the explicitly listed destructive process-interruption tests; `stress --repeats 20` runs 20 parallel and 20 serial `stabilization_flow` passes on each Rust runner.
- PostgreSQL 17 opt-in memory/migration tests and controlled-HTTP agent acceptance run on Linux and both Mac architectures; **not Windows**. This is an explicit coverage exception, not a Windows pass.
- `scripts/live_qwen_acceptance.ps1` is tested with an offline fixture in CI. Actual model responses, latency, hardware memory use and real-Qwen coding success **are not** tested in this matrix.
- Safety: a green CI run does not prove sandboxing of Cargo build scripts, procedural macros or tests.

## Verified historical dedicated ignored process-interruption regressions

The table maps dedicated `cargo test --locked --lib ... -- --ignored` invocations introduced by historical PR diffs to entries in `ci/ignored-regressions.txt`. Every row below was present in the manifest inspected for PR #37.

| Introduced in | Exact test identifier |
|---|---|
| #7 | `workspace_patch::tests::process_kill_during_workspace_apply_keeps_recovery_evidence` |
| #10 | `workspace_recovery::tests::restore_parent_kills_child_and_resumes` |
| #11 | `workspace_recovery::tests::process_kill_during_staging_preserves_candidate_and_allows_reapproved_restore` |
| #12 | `workspace_recovery::tests::process_kill_mid_stage_write_keeps_target_and_marker_for_reapproval` |
| #15 | `workspace_patch::tests::process_kill_during_partial_staged_apply_preserves_original_source` |
| #16 | `workspace_patch::tests::process_kill_mid_staged_create_preserves_target_absence_and_pending_marker` |
| #16 | `workspace_patch::tests::process_kill_after_staged_create_publication_resumes_with_explicit_approval` |
| #17 | `workspace_patch::tests::process_kill_mid_rollback_stage_preserves_updated_target_and_marker` |
| #17 | `workspace_patch::tests::process_kill_after_rollback_rename_keeps_original_and_recoverable_marker` |
| #20 | `workspace_patch::tests::process_kill_after_created_file_rollback_delete_requires_new_approval` |
| #23 | `workspace_recovery::tests::approved_created_file_recovery_kill_after_delete_requires_fresh_approval` |

The shared runner checks that each manifest entry exactly matches a library test before running it, preventing silently successful zero-match filters. This audit proves correspondence for the **11 explicitly enumerated** process-kill tests, not for every normal or ignored test that ever existed.

## Historical coverage categories to trace

| Historical PRs | Feature/test families | CI route | Audit status |
|---|---|---|---|
| #1–#2 | Baseline Rust tests, stabilization stress, PostgreSQL acceptance | Four-runner normal/stress + three-runner PostgreSQL | Workflow coverage identified; historical test-by-test inventory pending |
| #4–#6, #8–#9 | Journal, recovery, approvals, integrity, CLI tests | `cargo test --locked` | Normal-test route identified; exact names/provenance pending |
| #7, #10–#12, #15–#17, #20, #23 | Dedicated process-interruption cases | Shared ignored manifest | **11 dedicated tests mapped** |
| #13–#14, #18–#19, #21–#22, #24–#27 | Integrity, locking, adversarial symlink/directory swaps, final-delete checks | `cargo test --locked` (unless explicitly ignored) | Exact names/ignored markers and applicability pending |
| #28–#34 | Real Cargo workspace integration and controlled-model agent repair/rollback/approval | `cargo test --locked` | CI route identified; exact names/provenance pending |
| #35 | Local real-Qwen acceptance measurement | Cross-platform PowerShell offline smoke; separate manual real-model execution | Script smoke present; live Qwen still unmeasured |
| #36–#37 | Native macOS and unified cross-platform regressions | Four-runner Rust matrix + macOS PostgreSQL | CI configured; final SHA checks and ongoing regression policy must be verified |

## Remaining blockers before claiming full historical parity

1. Enumerate **every** added/modified test from PR #1–#36, including conditional, ignored, CLI, real-Cargo and agent tests. Compare exact test identifiers with current source and runner discovery. Check for deleted/renamed tests and document functional replacements, not just matching titles.
2. Detect any current ignored library/integration tests omitted from the explicit crash manifest or the database acceptance workflows; classify intentionally manual tests separately.
3. Record each platform's **actual run result on one immutable head SHA**. If audit changes trigger new CI runs, prior green runs are not evidence for the new SHA.
4. Decide and document platform exclusions with a rationale, especially Windows PostgreSQL and live hardware-bound Qwen tests; do not label excluded work as passed.
5. Keep PR #37 open/draft until the above evidence is complete and all required CI checks pass. No merge is authorized by this document alone.

## Verified #28–#34 real-Cargo and controlled-agent test introductions

The following identifiers were extracted from added test functions in the corresponding PR patches (not inferred from their PR titles). They are expected to be discovered by the normal `cargo test --locked` command, subject to final exact-name inventory and platform run validation.

| PR | Test identifier in its source file |
|---|---|
| #28 | `real_cargo_project_compiles_before_after_multifile_apply_and_after_rollback` (`tests/real_cargo_workspace_acceptance.rs`) |
| #29 | `real_cargo_failed_multifile_change_can_be_rolled_back_to_passing_state` (same integration suite) |
| #30 | `real_cargo_compiler_failure_rollback_restores_a_compilable_project` (same integration suite) |
| #31 | `workspace_agent_failed_real_cargo_test_rolls_back_all_files` (`src/agent.rs`) |
| #32 | `workspace_agent_denied_apply_consent_preserves_all_project_files` (`src/agent.rs`) |
| #33 | `workspace_agent_failed_candidate_then_corrected_candidate_is_verified` (`src/agent.rs`) |
| #34 | `workspace_agent_failing_unit_test_then_corrected_candidate_passes` (`src/agent.rs`) |

This establishes seven historical test introductions with source provenance, **not** proof that the named tests survived unchanged or passed on every platform. The `ci/historical-acceptance-tests.txt` manifest now pins these seven exact libtest identifiers, and `scripts/test_inventory.py` fails if one is missing from the discovered test list. The inventory also lists ignored library tests for manual classification and runs in the four-runner Rust CI matrix. This establishes automated *presence* of these seven cases, not their independent historical execution at every old commit. Full parity still requires reconciling all other PR diffs and manually classified opt-in cases.
