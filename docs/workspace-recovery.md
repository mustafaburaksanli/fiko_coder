# Workspace recovery audit (v1.3 preview)

FİKO CODER writes a pending marker before modifying targets in an approved multi-file workspace plan. It records relative target paths, SHA-256 hashes and local backup names, not the source contents. This is **not** a crash-atomic transaction log.

## Run read-only audit

From the **affected project's directory**, invoke an independently built FİKO CODER executable:

```powershell
& "C:\path\to\fiko_coder.exe" --recovery-audit
```

The audit does not call Ollama, PostgreSQL or Cargo. Avoid running `cargo run` inside an untrusted project for recovery; Cargo can execute build scripts.

## Interpret results

- Target `original` / `original_absent`: matches the original content, or an originally absent file is still absent.
- Target `updated`: matches proposed content; this alone does not prove code tests passed.
- Target `unexpected`: neither the original nor the updated bytes, potentially a partial write or external modification.
- Target `missing`, `unsafe_path`, `unsafe_or_oversized`, `unreadable`: cannot establish a safe file state.
- Backup `intact`: current backup bytes match the original SHA-256.
- Backup `not_required`: create-only target, no original to back up.
- Backup `missing`, `mismatch`, `missing_reference`, `unsafe_reference`, `invalid_original_hash`, `unreadable`, `unsafe_or_oversized`, `unexpected_reference`: do not assume recovery is possible.

**Pending marker write lock:** Both `apply_workspace` and single-file `apply_patch` reject new writes when an unresolved workspace marker exists. This does not freeze unrelated external processes or replace user-reviewed recovery.

## Structured restoration preview (read-only)

From the project directory, use an independently built FİKO CODER executable:

```powershell
& "C:\path\to\fiko_coder.exe" --recovery-plan
```

The command prints a machine-readable JSON review plan with target and backup states, a marker fingerprint, and a proposed action for each file: `restore_original_from_backup`, `remove_created_candidate`, `already_original`, `already_absent`, or `manual_review`.

`eligible_for_review: true` means all entries have expected hashes and usable backups **at the time they were read**. It is not permission to restore, an atomic snapshot, or a safety certification. The preview is deliberately read-only: it does not restore, clear the marker, or start Cargo/Ollama/PostgreSQL. An invalid duplicate target, forged hash, modified file, unsafe path, or corrupted backup produces `manual_review` and `eligible_for_review: false`.

## Explicitly approved restore (v1.3 experimental)

**Make an independent copy of the project and `.fiko_backups` first.** Review the JSON plan and all changed files. If `eligible_for_review` is `true` and you explicitly want to discard candidate edits and return to original code, run the following from the affected project directory using an independently built executable:

```powershell
& "C:\path\to\fiko_coder.exe" --recovery-restore
```

The command requires an **interactive local terminal** (piped input and unattended authorization are refused). It prints the current plan, then requires typing `RESTORE` followed by the **entire** 64-character marker fingerprint. The tool rechecks marker identity, all target and backup hashes, and each target again immediately before changing it. Successful recovery restores original source bytes from validated backups, deletes only candidate-created files that still have their expected contents, verifies final hashes, and only then clears the pending marker. No Cargo checks or model tools are run.

On an unexpected file, changed/invalid backup, wrong approval, broken marker, partial write, or external modification, recovery refuses or stops and **keeps the marker**. Some preceding files may have been restored before an error; inspect `--recovery-plan` again and take an independent snapshot before a retry. Resume only after new interactive approval. The operation is **not atomic** and does not provide an operating-system-wide lock or protection against malicious concurrent writers, abrupt power loss, or races between each read and write.

The host's restore routine is not available as an agent tool: it must never infer the user's approval from model output, an old conversation, or the plan's `eligible_for_review` flag.

## Incident response (no automatic restore)

1. Stop new writes and make an **independent copy** of the project and its `.fiko_backups` directory.
2. Run the audit, review the affected Git changes and compare files with their backups.
3. Do not automatically overwrite a target marked `unexpected`, or use an unverified backup.
4. Decide manually whether to keep the changed code or restore reviewed originals. The audit never grants this authority.
5. Do not remove the marker just to unblock edits; doing so loses recovery evidence.

## Developer interruption regression

The Windows and Linux CI workflows run an isolated process-termination regression in addition to ordinary Rust tests:

```shell
cargo test --locked --lib workspace_patch::tests::process_kill_during_workspace_apply_keeps_recovery_evidence -- --ignored --exact
```

The parent test kills a dedicated child process **after two workspace files have been written and before the third write**. It then verifies that the read-only audit can still describe the pending marker, intact backups and unchanged third target. This is a real process kill but **not** a partial-byte-write or sudden power-loss simulation.

### Restore interruption regression

The Windows and Linux CI also run a separate, ignored-by-default test:

```shell
cargo test --locked --lib workspace_recovery::tests::restore_parent_kills_child_and_resumes -- --ignored --exact
```

The parent process kills a dedicated child after the **first** of three files has been restored. It verifies that the marker is retained, the next two files remain unchanged, and the fresh preview reports a resumable `already_original` / `restore_original_from_backup` / `remove_created_candidate` plan. The test then uses the trusted library host API with a newly checked marker fingerprint. The production command still requires interactive local terminal authorization.

This is an actual process kill **between** file writes, not during a partial-byte write or a simulated power outage. The underlying restore procedure is still not an atomic filesystem transaction.

## Staged restore (v1.3 experimental)

Restoring an edited file now writes the original bytes to a **new sibling temporary file** in the same directory, verifies the original SHA-256, syncs that staged copy, preserves source permissions and then attempts to replace the candidate file. It does **not** truncate the original candidate in place. This narrows the damage window if the process stops mid-write.

If a process is killed while staging, a hidden `.fiko_restore_stage_*.tmp` file may remain next to the original target. Do not treat orphan stage files as authenticated sources or delete them blindly. Make an independent project copy and inspect the pending marker and target hashes first. A fresh approved restore may complete while leaving such orphan evidence for manual cleanup.

CI kills a real child process **after the staged file is synced but before replacement**. It verifies that the candidate target and pending marker remain unchanged and that a newly approved restore completes. This is distinct from an abrupt power outage, a crash during rename, or multi-file atomicity; those properties are **not guaranteed**.

### Partial staged-write interruption regression

A dedicated Windows/Linux CI regression terminates a real isolated process **after only half of the original recovery content was written to a sibling staging file**, and before any replacement. The parent verifies that the approved candidate, pending marker and verified backup remain unchanged, that the orphan staging file is partial and untrusted, and that a new separately approved recovery reads the verified backup rather than the orphan.

This is process termination after a controlled partial write, not sudden power loss or evidence of whole-workspace crash atomicity. Orphan staging files are retained for review rather than trusted or automatically deleted.

## Limitations

- A missing marker does not establish earlier crash-atomicity or successful verification.
- The audit is not a filesystem snapshot; external writers can race each read.
- The marker does not guarantee directory durability, absence of power-loss corruption or process isolation.
- SHA-256 matches demonstrate byte integrity relative to the marker, not authenticity or approval.
- Candidate verification via Cargo is **not sandboxed**, even in a temporary workspace.
- User-confirmed restoration now stages original bytes before each replacement, but remains **non-atomic across multiple files** and cannot guarantee power-loss durability or isolation from external writers.
