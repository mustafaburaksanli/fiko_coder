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

## Incident response (no automatic restore)

1. Stop new writes and make an **independent copy** of the project and its `.fiko_backups` directory.
2. Run the audit, review the affected Git changes and compare files with their backups.
3. Do not automatically overwrite a target marked `unexpected`, or use an unverified backup.
4. Decide manually whether to keep the changed code or restore reviewed originals. The audit never grants this authority.
5. Do not remove the marker just to unblock edits; doing so loses recovery evidence.

## Limitations

- A missing marker does not establish earlier crash-atomicity or successful verification.
- The audit is not a filesystem snapshot; external writers can race each read.
- The marker does not guarantee directory durability, absence of power-loss corruption or process isolation.
- SHA-256 matches demonstrate byte integrity relative to the marker, not authenticity or approval.
- Candidate verification via Cargo is **not sandboxed**, even in a temporary workspace.
- Full crash recovery with user-approved file restoration is not yet implemented.
