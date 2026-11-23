//! Disk-backed, fail-closed marker for interrupted multi-file workspace writes.
//!
//! The marker is diagnostic evidence, not a crash-atomic transaction log.
//! It never restores files automatically and never grants write authorization.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    discovery,
    workspace_patch::{MAX_FILE_BYTES, MAX_WORKSPACE_FILES, WorkspaceRecord},
};

const MARKER: &str = "workspace.pending.json";
const MAX_MARKER_BYTES: u64 = 64 * 1024;

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Reject a replaced directory, a symlink/junction marker or an unsafe entry.
/// A missing backup directory / marker is not itself an error.
fn marker_path(root: &Path) -> Result<Option<PathBuf>, String> {
    let directory = root.join(".fiko_backups");
    let meta = match fs::symlink_metadata(&directory) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if !meta.is_dir()
        || is_link(&meta)
        || directory.canonicalize().map_err(|e| e.to_string())? != root.join(".fiko_backups")
    {
        return Err("Kurtarma klasoru guvenli degil.".into());
    }
    let path = directory.join(MARKER);
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_file() || is_link(&meta) => {
            Err("Kurtarma kaydi normal bir dosya degil.".into())
        }
        Ok(_) => Ok(Some(path)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn manifest(record: &WorkspaceRecord) -> Value {
    let files: Vec<_> = record
        .files
        .iter()
        .map(|entry| {
            json!({
                "path": entry.file.relative,
                "original_sha256": entry.file.original.as_ref().map(|text| sha(text.as_bytes())),
                "updated_sha256": sha(entry.file.updated.as_bytes()),
                "backup_name": entry.backup_path.as_ref()
                    .and_then(|path| path.file_name()).map(|s| s.to_string_lossy().into_owned()),
            })
        })
        .collect();
    json!({"version": 1, "status": "applying_or_unverified", "files": files})
}

fn read_marker(root: &Path) -> Result<Option<Value>, String> {
    let Some(path) = marker_path(root)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    File::open(&path)
        .map_err(|e| e.to_string())?
        .take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err("Kurtarma kaydi beklenenden buyuk.".into());
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "Kurtarma kaydi bozuk; elle incelenmeli.".to_string())?;
    if value["version"] != 1
        || value["status"] != "applying_or_unverified"
        || value["files"]
            .as_array()
            .is_none_or(|files| files.is_empty() || files.len() > MAX_WORKSPACE_FILES)
    {
        return Err("Bilinmeyen kurtarma kaydi; elle incelenmeli.".into());
    }
    Ok(Some(value))
}

fn matching_marker(root: &Path, record: &WorkspaceRecord) -> Result<bool, String> {
    let Some(found) = read_marker(root)? else {
        return Ok(false);
    };
    if found != manifest(record) {
        return Err("Baska veya degistirilmis bir kurtarma kaydi var; islem reddedildi.".into());
    }
    Ok(true)
}

/// OS-backed advisory lock, shared by FIKO's single-file edits, workspace
/// apply and explicitly approved recovery. The file stays on disk on purpose:
/// deleting a lock file can let two processes lock different inodes.
pub struct WorkspaceOperationGuard {
    _locked_file: File,
}

// Explicitly release a completed operation's advisory lock before the handle
// closes. A hard process kill still relies on the OS closing its handles.
impl Drop for WorkspaceOperationGuard {
    fn drop(&mut self) {
        let _ = self._locked_file.unlock();
    }
}

/// Non-blocking exclusive lock. A killed process releases its OS handle; a
/// pending marker remains separately responsible for crash recovery.
///
/// This coordinates cooperating FIKO processes only; it does not stop external
/// editors or offer sandboxing or multi-file atomicity.
pub fn try_lock_operation(root: &Path) -> Result<WorkspaceOperationGuard, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err("Proje koku bir dizin olmali.".into());
    }
    let directory = root.join(".fiko_backups");
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("Kilit klasoru olusturulamadi: {error}")),
    }
    // Reject substituted directory symlinks / Windows junctions.
    let _ = marker_path(&root)?;
    let path = directory.join("workspace.operation.lock");
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_file() || is_link(&meta) => {
            return Err("Islem kilidi normal bir dosya degil.".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("Islem kilidi acilamadi: {e}"))?;
    let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() || is_link(&meta) {
        return Err("Islem kilidi son kontrolu basarisiz; yazma reddedildi.".into());
    }
    file.try_lock()
        .map_err(|error| format!("Baska bir FIKO yazma/kurtarma islemi devam ediyor: {error}"))?;
    // Never unlink the lock file; closing the handle releases the OS lock.
    Ok(WorkspaceOperationGuard { _locked_file: file })
}

/// Reject new writes before creating backup artifacts when a previous transaction is unresolved.
pub fn ensure_clear(root: &Path) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if marker_path(&root)?.is_some() {
        return Err(
            "Onceki workspace islemi belirsiz: --recovery-audit calistirin; yeni yazma reddedildi."
                .into(),
        );
    }
    Ok(())
}

/// Called after all original-file backups are written, before the first target write.
/// create_new prevents a second workspace transaction from overwriting an unresolved marker.
pub fn start(root: &Path, record: &WorkspaceRecord) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if marker_path(&root)?.is_some() {
        return Err(
            "Onceki workspace islemi belirsiz: --recovery-audit calistirin; yeni yazma reddedildi."
                .into(),
        );
    }
    let directory = root.join(".fiko_backups");
    if !directory.exists() {
        fs::create_dir(&directory).map_err(|e| format!("Kurtarma klasoru olusturulamadi: {e}"))?;
    }
    // Recheck the directory after creating it; never follow a substituted link.
    let _ = marker_path(&root)?;
    let bytes = serde_json::to_vec_pretty(&manifest(record)).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err("Kurtarma kaydi boyut sinirini asiyor.".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(MARKER))
        .map_err(|e| format!("Kurtarma kaydi olusturulamadi: {e}"))?;
    file.write_all(&bytes)
        .map_err(|e| format!("Kurtarma kaydi yazilamadi: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("Kurtarma kaydi diske kaydedilemedi: {e}"))?;
    Ok(())
}

fn file_state(
    root: &Path,
    relative: &str,
    expected_original: Option<&str>,
    expected_updated: &str,
) -> &'static str {
    if discovery::check_relative_path(relative).is_err() {
        return "unsafe_path";
    }
    let target = match discovery::resolve_target(root, relative) {
        Ok(path) => path,
        Err(_) => return "unsafe_path",
    };
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_file() && !is_link(&meta) && meta.len() <= MAX_FILE_BYTES as u64 => {}
        Ok(_) => return "unsafe_or_oversized",
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return if expected_original.is_none() {
                "original_absent"
            } else {
                "missing"
            };
        }
        Err(_) => return "unreadable",
    }
    let mut bytes = Vec::new();
    if File::open(&target)
        .and_then(|file| file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes))
        .is_err()
    {
        return "unreadable";
    }
    if bytes.len() > MAX_FILE_BYTES {
        return "unsafe_or_oversized";
    }
    let actual = sha(&bytes);
    if actual == expected_updated {
        "updated"
    } else if expected_original == Some(actual.as_str()) {
        "original"
    } else {
        "unexpected"
    }
}

/// Do not trust paths from the on-disk manifest. Backup checks are strictly read-only.
fn backup_state(root: &Path, entry: &Value) -> &'static str {
    let Some(original) = entry["original_sha256"].as_str() else {
        return if entry["backup_name"].is_null() {
            "not_required"
        } else {
            "unexpected_reference"
        };
    };
    if original.len() != 64 || !original.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return "invalid_original_hash";
    }
    let Some(name) = entry["backup_name"].as_str() else {
        return "missing_reference";
    };
    if name.is_empty()
        || name.len() > 240
        || !name.ends_with(".bak")
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control)
        || name == "."
        || name == ".."
    {
        return "unsafe_reference";
    }
    let path = root.join(".fiko_backups").join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !is_link(&meta) && meta.len() <= MAX_FILE_BYTES as u64 => {}
        Ok(_) => return "unsafe_or_oversized",
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return "missing",
        Err(_) => return "unreadable",
    }
    let mut bytes = Vec::new();
    if File::open(&path)
        .and_then(|file| file.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes))
        .is_err()
    {
        return "unreadable";
    }
    if bytes.len() > MAX_FILE_BYTES {
        return "unsafe_or_oversized";
    }
    if sha(&bytes) == original {
        "intact"
    } else {
        "mismatch"
    }
}

fn state_matches(root: &Path, record: &WorkspaceRecord, verified: bool) -> bool {
    record.files.iter().all(|entry| {
        let original = entry
            .file
            .original
            .as_ref()
            .map(|text| sha(text.as_bytes()));
        let updated = sha(entry.file.updated.as_bytes());
        let status = file_state(root, &entry.file.relative, original.as_deref(), &updated);
        status
            == if verified {
                "updated"
            } else if original.is_some() {
                "original"
            } else {
                "original_absent"
            }
    })
}

fn finish(root: &Path, record: &WorkspaceRecord, verified: bool) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !matching_marker(&root, record)? {
        return if verified {
            Err("Dogrulanmis islem icin kurtarma kaydi kayip; basari raporlanamaz.".into())
        } else {
            Ok(())
        };
    }
    if !state_matches(&root, record, verified) {
        return Err("Dosyalar beklenen kurtarma durumunda degil; kayit korundu.".into());
    }
    if verified {
        let expected = manifest(record);
        if !expected["files"].as_array().is_some_and(|files| {
            files
                .iter()
                .all(|entry| matches!(backup_state(&root, entry), "intact" | "not_required"))
        }) {
            return Err("Yedeklerin butunlugu dogrulanamadi; kurtarma kaydi korundu.".into());
        }
    }
    let path = marker_path(&root)?.ok_or("Kurtarma kaydi kayboldu.")?;
    fs::remove_file(&path).map_err(|e| format!("Kurtarma kaydi kapatilamadi: {e}"))
}

/// Successful post-apply Cargo verification must be checked by the trusted caller
/// before invoking this function. This only checks file contents and clears a marker.
pub fn finish_verified(root: &Path, record: &WorkspaceRecord) -> Result<(), String> {
    finish(root, record, true)
}

/// External changes must not be overwritten just to close a marker.
pub fn finish_rollback(root: &Path, record: &WorkspaceRecord) -> Result<(), String> {
    finish(root, record, false)
}

/// Refuse to roll back a record while a different transaction is unresolved.
pub fn assert_rollback_record(root: &Path, record: &WorkspaceRecord) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    matching_marker(&root, record).map(|_| ())
}

/// Produce a machine-readable *preview* for a future, explicitly approved
/// restoration procedure. This function does not write, restore or clear markers.
///
/// All data comes from an untrusted on-disk marker. Each target/backup is
/// independently classified and ambiguous cases fail closed.
pub fn restoration_preview(root: &Path) -> Result<String, String> {
    use std::collections::HashSet;

    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let Some(value) = read_marker(&root)? else {
        return serde_json::to_string_pretty(&json!({
            "status": "no_pending",
            "eligible_for_review": false,
            "automatic_restore": false,
            "files": [],
        }))
        .map_err(|e| e.to_string());
    };
    // A content fingerprint is intended for comparison at approval time, NOT
    // proof that the filesystem was atomically snapshotted or that the marker
    // came from a trusted actor.
    let fingerprint = sha(&serde_json::to_vec(&value).map_err(|e| e.to_string())?);
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut eligible_for_review = true;

    for entry in value["files"]
        .as_array()
        .ok_or("Kurtarma listesi gecersiz.")?
    {
        let path = entry["path"]
            .as_str()
            .ok_or("Kurtarma hedef yolu gecersiz.")?;
        let updated = entry["updated_sha256"]
            .as_str()
            .ok_or("Kurtarma hedef SHA256 eksik.")?;
        let original = match &entry["original_sha256"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(text) => Some(text.as_str()),
            _ => return Err("Kurtarma orijinal SHA256 gecersiz.".into()),
        };
        let valid_hash =
            |hash: &str| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit());
        let valid_entry = entry.get("path").is_some()
            && entry.get("original_sha256").is_some()
            && entry.get("updated_sha256").is_some()
            && entry.get("backup_name").is_some()
            && valid_hash(updated)
            && original.is_none_or(valid_hash);
        let target = if valid_entry && discovery::check_relative_path(path).is_ok() {
            discovery::resolve_target(&root, path).ok()
        } else {
            None
        };
        let unique = target.as_ref().is_some_and(|target| {
            let key = if cfg!(windows) {
                target.to_string_lossy().to_lowercase()
            } else {
                target.to_string_lossy().into_owned()
            };
            seen.insert(key)
        });
        let file_status = if valid_entry && unique {
            file_state(&root, path, original, updated)
        } else {
            "invalid_or_duplicate_target"
        };
        let backup_status = backup_state(&root, entry);
        // An original and a candidate with identical hashes should not be
        // actionable. A genuine workspace edit never contains this shape.
        let ambiguous = original == Some(updated);
        let action = if ambiguous {
            "manual_review"
        } else {
            match (original, file_status, backup_status) {
                (Some(_), "updated", "intact") => "restore_original_from_backup",
                (Some(_), "original", "intact") => "already_original",
                (None, "updated", "not_required") => "remove_created_candidate",
                (None, "original_absent", "not_required") => "already_absent",
                _ => "manual_review",
            }
        };
        if action == "manual_review" {
            eligible_for_review = false;
        }
        entries.push(json!({
            "path": path,
            "target_state": file_status,
            "backup_state": backup_status,
            "proposed_action": action,
        }));
    }
    serde_json::to_string_pretty(&json!({
        "status": "pending",
        "marker_fingerprint_sha256": fingerprint,
        "eligible_for_review": eligible_for_review,
        "automatic_restore": false,
        "requires_explicit_user_approval": true,
        "warning": "READ_ONLY preview. A separate interactive recovery command requires explicit approval and rechecks hashes before writing; this preview never restores files.",
        "files": entries,
    }))
    .map_err(|e| e.to_string())
}

/// A restore action prepared from a trusted, read-only preview.
struct RestoreStep {
    relative: String,
    target: PathBuf,
    original_sha256: Option<String>,
    updated_sha256: String,
    action: String,
    backup_bytes: Option<Vec<u8>>,
}

fn verified_backup_bytes(root: &Path, entry: &Value) -> Result<Vec<u8>, String> {
    if backup_state(root, entry) != "intact" {
        return Err("Kurtarma yedegi orijinal SHA256 ile dogrulanmadi.".into());
    }
    let name = entry["backup_name"]
        .as_str()
        .ok_or("Kurtarma yedegi adi gecersiz.")?;
    let path = root.join(".fiko_backups").join(name);
    let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || is_link(&metadata) || metadata.len() > MAX_FILE_BYTES as u64 {
        return Err("Kurtarma yedegi guvensiz veya cok buyuk.".into());
    }
    let mut bytes = Vec::new();
    File::open(&path)
        .map_err(|e| e.to_string())?
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES
        || entry["original_sha256"].as_str() != Some(sha(&bytes).as_str())
    {
        return Err("Kurtarma yedegi yeniden okundugunda SHA256 degisti.".into());
    }
    Ok(bytes)
}

/// Verify the exact staged path immediately before it can replace a source.
fn verify_staged_copy(path: &Path, original: &[u8]) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || is_link(&meta) || meta.len() != original.len() as u64 {
        return Err("Gecici dosyanin turu veya boyutu degisti; hedef korunuyor.".into());
    }
    let mut current = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut current)
        .map_err(|e| e.to_string())?;
    if current.len() != original.len() || sha(&current) != sha(original) {
        return Err("Gecici dosyanin SHA256 degeri degisti; hedef korunuyor.".into());
    }
    Ok(())
}

/// A file is staged as a sibling on the same filesystem and synced BEFORE
/// replacing the candidate. On process termination during staging, the target
/// remains untouched and the recovery marker stays pending.
///
/// This is not a whole-workspace transaction, a power-loss guarantee, or a
/// defense against a malicious concurrent filesystem writer.
fn stage_original_and_replace(
    root: &Path,
    step: &RestoreStep,
    opened: File,
    opened_metadata: &fs::Metadata,
    expected_fingerprint: &str,
) -> Result<(), String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let bytes = step
        .backup_bytes
        .as_ref()
        .ok_or("Geri yukleme yedegi kayip.")?;
    if step.original_sha256.as_deref() != Some(sha(bytes).as_str()) {
        return Err("Geri yukleme icerigi orijinal SHA256 ile uyusmuyor.".into());
    }
    let parent = step.target.parent().ok_or("Hedef ebeveyn klasoru yok.")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let mut created = None;
    for attempt in 0..16 {
        let name = format!(
            ".fiko_restore_stage_{}_{}_{}.tmp",
            std::process::id(),
            stamp,
            attempt
        );
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                created = Some((file, path));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Geri yukleme gecici dosyasi olusturulamadi: {e}")),
        }
    }
    let (stage_handle, stage_path) = created.ok_or("Gecici dosya adlari kullaniliyor.")?;
    // If the process dies, Drop is not invoked and an orphan staging file may
    // remain for manual review. On ordinary failures, clean up our own file.
    struct StageCleanup(Option<PathBuf>);
    impl Drop for StageCleanup {
        fn drop(&mut self) {
            if let Some(path) = &self.0 {
                let _ = fs::remove_file(path);
            }
        }
    }
    let mut cleanup = StageCleanup(Some(stage_path.clone()));
    // Declare the live file handle AFTER the cleanup guard, so it closes
    // before cleanup runs on early return (required on Windows).
    let mut staged = stage_handle;
    // Test-only interruption *during* staged write, before sync/replacement.
    // Production binaries cannot enable this branch.
    #[cfg(test)]
    if step.relative == "src/a.rs"
        && std::env::var_os("FIKO_TEST_PARTIAL_STAGE_KILL_PROJECT").is_some()
    {
        let partial_len = (bytes.len() / 2).max(1);
        staged
            .write_all(&bytes[..partial_len])
            .map_err(|e| e.to_string())?;
        staged.sync_all().map_err(|e| e.to_string())?;
        let signal = root.join(".fiko_test_partial_stage_ready");
        let mut ready = File::create(signal).map_err(|e| e.to_string())?;
        ready.write_all(b"ready").map_err(|e| e.to_string())?;
        ready.sync_all().map_err(|e| e.to_string())?;
        drop(ready);
        std::thread::sleep(std::time::Duration::from_secs(30));
        return Err("Partial-stage child should have been terminated.".into());
    }
    staged.write_all(bytes).map_err(|e| e.to_string())?;
    staged.sync_all().map_err(|e| e.to_string())?;
    // Preserve source permissions, including Unix executable bits. Timestamp
    // metadata may change and is not guaranteed to be preserved.
    fs::set_permissions(&stage_path, opened_metadata.permissions()).map_err(|e| e.to_string())?;
    // Permission changes modify metadata too; flush them before replacement.
    staged.sync_all().map_err(|e| e.to_string())?;
    drop(staged);
    verify_staged_copy(&stage_path, bytes)?;

    #[cfg(test)]
    if step.relative == "src/a.rs"
        && std::env::var_os("FIKO_TEST_RESTORE_STAGE_KILL_PROJECT").is_some()
    {
        let signal = root.join(".fiko_test_restore_stage_ready");
        let mut file = File::create(signal).map_err(|e| e.to_string())?;
        file.write_all(b"ready").map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::thread::sleep(std::time::Duration::from_secs(30));
        return Err("Stage test child should have been terminated.".into());
    }

    // Recheck before replacement. This narrows, but does not fully close,
    // the race between inspection and renaming the staged file.
    let marker = read_marker(root)?.ok_or("Kurtarma kaydi kayboldu.")?;
    if sha(&serde_json::to_vec(&marker).map_err(|e| e.to_string())?) != expected_fingerprint {
        return Err("Kurtarma kaydi gecici dosya hazirlanirken degisti.".into());
    }
    if discovery::resolve_existing(root, &step.relative)? != step.target
        || file_state(
            root,
            &step.relative,
            step.original_sha256.as_deref(),
            &step.updated_sha256,
        ) != "updated"
    {
        return Err("Hedef gecici dosya hazirlanirken degisti; degisiklik durduruldu.".into());
    }
    // A staged file could have changed after its initial readback (e.g. a
    // concurrent writer while the marker or target was being revalidated).
    // Recheck immediately before rename; this still is NOT an OS lock.
    #[cfg(test)]
    if step.relative == "src/a.rs" && std::env::var_os("FIKO_TEST_TAMPER_STAGED_COPY").is_some() {
        fs::write(&stage_path, vec![b'x'; bytes.len()]).map_err(|e| e.to_string())?;
    }
    verify_staged_copy(&stage_path, bytes)?;
    // Release the source handle before rename, especially on Windows.
    drop(opened);
    fs::rename(&stage_path, &step.target)
        .map_err(|e| format!("Gecici dosya hedefin yerine konulamadi: {e}"))?;
    cleanup.0 = None;
    Ok(())
}

/// Trusted HOST API for recovery after a separately collected, explicit local
/// terminal approval. Not a model tool. This method NEVER asks for approval:
/// callers must perform the human prompt and pass the exact marker fingerprint.
///
/// Re-checks all targets before any mutation, then re-checks each before use.
/// On uncertainty or partial failures the pending marker is deliberately kept.
pub fn restore_after_explicit_approval(
    root: &Path,
    approved_fingerprint: &str,
) -> Result<String, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let _operation_lock = try_lock_operation(&root)?;
    let preview: Value =
        serde_json::from_str(&restoration_preview(&root)?).map_err(|e| e.to_string())?;
    if preview["status"] != "pending" || preview["eligible_for_review"] != true {
        return Err(
            "Kurtarma plani manuel inceleme istiyor veya kayit yok; yazma reddedildi.".into(),
        );
    }
    let fingerprint = preview["marker_fingerprint_sha256"]
        .as_str()
        .ok_or("Kurtarma kaydi parmak izi gecersiz.")?;
    if approved_fingerprint != fingerprint {
        return Err("Kullanici onayi bu kurtarma kaydinin parmak iziyle eslesmiyor.".into());
    }
    let marker = read_marker(&root)?.ok_or("Kurtarma kaydi kayboldu.")?;
    if sha(&serde_json::to_vec(&marker).map_err(|e| e.to_string())?) != fingerprint {
        return Err("Kurtarma kaydi onaydan sonra degisti.".into());
    }
    let source_files = marker["files"]
        .as_array()
        .ok_or("Kurtarma listesi gecersiz.")?;
    let planned_files = preview["files"]
        .as_array()
        .ok_or("Kurtarma onizlemesi gecersiz.")?;
    if source_files.len() != planned_files.len() {
        return Err("Kurtarma listesi onaydan sonra degisti.".into());
    }

    // Read all original backups and validate ALL targets before the first edit.
    // This is not a filesystem snapshot and cannot defeat an external writer.
    let mut steps = Vec::with_capacity(source_files.len());
    for (entry, proposal) in source_files.iter().zip(planned_files) {
        let relative = entry["path"].as_str().ok_or("Gecersiz kurtarma yolu.")?;
        if proposal["path"] != relative {
            return Err("Kurtarma hedef listesi uyusmuyor.".into());
        }
        let target = discovery::resolve_target(&root, relative)?;
        let original = entry["original_sha256"].as_str();
        let updated = entry["updated_sha256"]
            .as_str()
            .ok_or("Aday SHA256 eksik.")?;
        let action = proposal["proposed_action"]
            .as_str()
            .ok_or("Kurtarma eylemi eksik.")?;
        let required_state = match action {
            "restore_original_from_backup" | "remove_created_candidate" => "updated",
            "already_original" => "original",
            "already_absent" => "original_absent",
            _ => return Err("Manuel inceleme gerektiren kurtarma eylemi reddedildi.".into()),
        };
        if file_state(&root, relative, original, updated) != required_state {
            return Err(format!(
                "{relative}: Dosya onaydan once degisti; geri yukleme reddedildi."
            ));
        }
        let backup_bytes = if action == "restore_original_from_backup" {
            Some(verified_backup_bytes(&root, entry)?)
        } else {
            if !matches!(backup_state(&root, entry), "intact" | "not_required") {
                return Err(format!("{relative}: Yedek durumu degisti."));
            }
            None
        };
        steps.push(RestoreStep {
            relative: relative.to_owned(),
            target,
            original_sha256: original.map(str::to_owned),
            updated_sha256: updated.to_owned(),
            action: action.to_owned(),
            backup_bytes,
        });
    }

    for step in &steps {
        // Reject a replaced pending marker even if an earlier target was
        // restored. A marker is not a cross-process lock or atomic snapshot.
        let current_marker = read_marker(&root)?.ok_or("Kurtarma kaydi kayboldu.")?;
        if sha(&serde_json::to_vec(&current_marker).map_err(|e| e.to_string())?) != fingerprint {
            return Err(
                "Kurtarma kaydi islem sirasinda degisti; sonraki dosyaya dokunulmadi.".into(),
            );
        }
        let original = step.original_sha256.as_deref();
        let observed = file_state(&root, &step.relative, original, &step.updated_sha256);
        let expected = match step.action.as_str() {
            "restore_original_from_backup" | "remove_created_candidate" => "updated",
            "already_original" => "original",
            "already_absent" => "original_absent",
            _ => return Err("Bilinmeyen kurtarma eylemi.".into()),
        };
        if observed != expected {
            return Err(format!(
                "{}: Dis degisiklik tespit edildi; kurtarma durduruldu, kayit korundu.",
                step.relative
            ));
        }

        match step.action.as_str() {
            "restore_original_from_backup" => {
                let meta = fs::symlink_metadata(&step.target).map_err(|e| e.to_string())?;
                if !meta.is_file() || is_link(&meta) || meta.len() > MAX_FILE_BYTES as u64 {
                    return Err(format!(
                        "{}: Geri yuklenecek hedef guvenli degil.",
                        step.relative
                    ));
                }
                let mut handle = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&step.target)
                    .map_err(|e| format!("{}: {e}", step.relative))?;
                // Recheck the bytes through the exact writable file handle.
                // This reduces the race between the earlier path check and open,
                // though it is NOT an OS-level no-follow/exclusive-open guarantee.
                let opened = handle.metadata().map_err(|e| e.to_string())?;
                if !opened.is_file() || opened.len() > MAX_FILE_BYTES as u64 {
                    return Err(format!(
                        "{}: Acilan hedef normal bir dosya degil.",
                        step.relative
                    ));
                }
                if discovery::resolve_existing(&root, &step.relative)? != step.target {
                    return Err(format!("{}: Hedef dosya yolu degisti.", step.relative));
                }
                let mut candidate = Vec::new();
                (&mut handle)
                    .take(MAX_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut candidate)
                    .map_err(|e| e.to_string())?;
                if candidate.len() > MAX_FILE_BYTES || sha(&candidate) != step.updated_sha256 {
                    return Err(format!(
                        "{}: Acilan dosya onayli adayla eslesmiyor.",
                        step.relative
                    ));
                }
                stage_original_and_replace(&root, step, handle, &opened, fingerprint)?;
            }
            "remove_created_candidate" => {
                // A final type and content recheck narrows, but does not
                // eliminate, the race before deleting a created file.
                #[cfg(all(test, unix))]
                if step.relative == "src/new.rs"
                    && std::env::var_os("FIKO_TEST_RECOVERY_CREATE_SYMLINK_RACE").is_some()
                {
                    use std::os::unix::fs::symlink;
                    let external = root.join("external_created_recovery.rs");
                    fs::write(&external, b"new\n").map_err(|e| e.to_string())?;
                    fs::remove_file(&step.target).map_err(|e| e.to_string())?;
                    symlink(&external, &step.target).map_err(|e| e.to_string())?;
                }
                let meta = fs::symlink_metadata(&step.target)
                    .map_err(|e| format!("{}: {e}", step.relative))?;
                if !meta.is_file() || is_link(&meta) || meta.len() > MAX_FILE_BYTES as u64 {
                    return Err(format!(
                        "{}: Silinecek aday normal dosya degil; kayit korundu.",
                        step.relative
                    ));
                }
                if discovery::resolve_existing(&root, &step.relative)? != step.target
                    || file_state(&root, &step.relative, original, &step.updated_sha256)
                        != "updated"
                {
                    return Err(format!(
                        "{}: Silinecek aday degisti; kayit korundu.",
                        step.relative
                    ));
                }
                fs::remove_file(&step.target).map_err(|e| format!("{}: {e}", step.relative))?;
            }
            "already_original" | "already_absent" => {}
            _ => return Err("Bilinmeyen kurtarma eylemi.".into()),
        }
        let final_state = file_state(&root, &step.relative, original, &step.updated_sha256);
        let desired = if original.is_some() {
            "original"
        } else {
            "original_absent"
        };
        if final_state != desired {
            return Err(format!(
                "{}: Geri yukleme sonrasi hash farkli; kayit korundu.",
                step.relative
            ));
        }
        // Test-only pause after the first restored file. Parent test kills the
        // actual process at this point; production binaries cannot enable it.
        #[cfg(test)]
        if step.relative == "src/a.rs"
            && std::env::var_os("FIKO_TEST_RESTORE_CRASH_PROJECT").is_some()
        {
            let signal = root.join(".fiko_test_restore_interrupt_ready");
            let mut file = File::create(signal).map_err(|e| e.to_string())?;
            file.write_all(b"ready").map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            drop(file);
            std::thread::sleep(std::time::Duration::from_secs(30));
            return Err("Crash test child should have been terminated.".into());
        }
    }

    // Do not clear evidence until both marker and ALL file states are verified.
    let current = read_marker(&root)?.ok_or("Kurtarma kaydi kayboldu.")?;
    if sha(&serde_json::to_vec(&current).map_err(|e| e.to_string())?) != fingerprint {
        return Err("Kurtarma kaydi degisti; kayit korunuyor.".into());
    }
    for (step, entry) in steps.iter().zip(source_files) {
        let desired = if step.original_sha256.is_some() {
            "original"
        } else {
            "original_absent"
        };
        if file_state(
            &root,
            &step.relative,
            step.original_sha256.as_deref(),
            &step.updated_sha256,
        ) != desired
            || !matches!(backup_state(&root, entry), "intact" | "not_required")
        {
            return Err("Geri yuklenen dosyalar tekrar dogrulanamadi; kayit korundu.".into());
        }
    }
    let path = marker_path(&root)?.ok_or("Kurtarma kaydi kayboldu.")?;
    fs::remove_file(&path).map_err(|e| format!("Kayit silinemedi; tekrar inceleyin: {e}"))?;
    Ok(format!(
        "RECOVERY_RESTORED: {} dosya orijinal durumuyla eslesti. Cargo dogrulamasi calistirilmadi.",
        steps.len()
    ))
}

/// Read-only, bounded incident report. It does not erase, restore or trust a model.
pub fn audit(root: &Path) -> Result<String, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let Some(value) = read_marker(&root)? else {
        return Ok("Kurtarma kaydi yok. Bu, eski islemlerin atomik oldugunu kanitlamaz.".into());
    };
    let mut lines = vec![
        "WORKSPACE RECOVERY: tamamlanmamis veya dogrulanmamis islem. Otomatik geri alma yapilmadi."
            .to_string(),
    ];
    for entry in value["files"].as_array().unwrap() {
        let Some(path) = entry["path"].as_str() else {
            return Err("Kurtarma kaydinda gecersiz hedef.".into());
        };
        let Some(updated) = entry["updated_sha256"].as_str() else {
            return Err("Kurtarma kaydinda gecersiz SHA.".into());
        };
        let original = entry["original_sha256"].as_str();
        let state = file_state(&root, path, original, updated);
        let backup = backup_state(&root, entry);
        lines.push(format!(
            "{}: {state}; backup={backup}",
            serde_json::to_string(path).unwrap_or_default()
        ));
    }
    lines.push(
        "Yedekleri ve degisiklikleri elle inceleyin. Kaydi silmek tek basina kurtarma degildir."
            .into(),
    );
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_patch::{self, FileChange, WorkspacePlan};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(label: &str) -> Self {
            let id = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "fiko_journal_{label}_{}_{}",
                std::process::id(),
                id
            ));
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("src/a.rs"), "pub fn a() -> u8 { 1 }\n").unwrap();
            Self(root)
        }
        fn plan(&self) -> WorkspacePlan {
            workspace_patch::prepare(
                &self.0,
                &[FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 2 }".into(),
                }],
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn operation_lock_can_be_reacquired_immediately_after_drop() {
        let project = Fixture::new("lock_fast_reopen");
        for _ in 0..64 {
            let lock = try_lock_operation(&project.0).unwrap();
            assert!(try_lock_operation(&project.0).is_err());
            drop(lock);
        }
        try_lock_operation(&project.0).unwrap();
    }

    #[test]
    fn operation_lock_rejects_other_fiko_write_paths_and_manual_restore() {
        let project = Fixture::new("locked_writes");
        let root = project.0.canonicalize().unwrap();
        let plan = project.plan();
        let lock = try_lock_operation(&root).unwrap();
        let second = try_lock_operation(&root)
            .err()
            .expect("a second handle must not acquire the held lock");
        assert!(second.contains("Baska bir FIKO"), "{second}");

        let blocked = workspace_patch::apply(&root, &plan).unwrap_err();
        assert!(blocked.message.contains("Baska bir FIKO"), "{blocked}");

        let single = crate::tools::prepare_patch(
            &root,
            "src/a.rs",
            "pub fn a() -> u8 { 1 }",
            "pub fn a() -> u8 { 3 }",
        )
        .unwrap();
        let error = crate::tools::apply_approved_patch(&root, single)
            .err()
            .expect("single-file writer must respect the same lock");
        assert!(error.contains("Baska bir FIKO"), "{error}");
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        drop(lock);

        let record = workspace_patch::apply(&root, &plan).unwrap();
        let fingerprint = approved_fingerprint(&root);
        let lock = try_lock_operation(&root).unwrap();
        let error = restore_after_explicit_approval(&root, &fingerprint).unwrap_err();
        assert!(error.contains("Baska bir FIKO"), "{error}");
        let rollback_error = workspace_patch::rollback(&root, &record).unwrap_err();
        assert!(
            rollback_error.contains("Baska bir FIKO"),
            "{rollback_error}"
        );
        assert!(root.join(".fiko_backups").join(MARKER).exists());
        drop(lock);

        restore_after_explicit_approval(&root, &fingerprint).unwrap();
        assert!(!root.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn legacy_single_file_rollback_respects_cooperative_lock() {
        let project = Fixture::new("single_rollback_lock");
        let root = project.0.canonicalize().unwrap();
        let plan = crate::tools::prepare_patch(
            &root,
            "src/a.rs",
            "pub fn a() -> u8 { 1 }",
            "pub fn a() -> u8 { 3 }",
        )
        .unwrap();
        let record = crate::tools::apply_approved_patch(&root, plan).unwrap();
        let lock = try_lock_operation(&root).unwrap();
        let err = crate::tools::rollback_patch(&record).unwrap_err();
        assert!(err.contains("Baska bir FIKO"), "{err}");
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 3 }\n"
        );
        drop(lock);
        crate::tools::rollback_patch(&record).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }

    #[test]
    #[ignore = "isolated child only; parent test owns the process lifetime"]
    fn operation_lock_child_holds_until_killed() {
        let Some(project) = std::env::var_os("FIKO_TEST_WORKSPACE_LOCK_CHILD") else {
            return;
        };
        let root = PathBuf::from(project).canonicalize().unwrap();
        let _lock = try_lock_operation(&root).unwrap();
        let mut signal = File::create(root.join(".fiko_test_workspace_lock_ready")).unwrap();
        signal.write_all(b"ready").unwrap();
        signal.sync_all().unwrap();
        drop(signal);
        std::thread::sleep(std::time::Duration::from_secs(30));
        panic!("lock child should have been terminated by parent");
    }

    #[test]
    fn process_kill_releases_os_lock_without_deleting_lock_file() {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        let project = Fixture::new("lock_process_kill");
        let root = project.0.canonicalize().unwrap();
        let signal = root.join(".fiko_test_workspace_lock_ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_recovery::tests::operation_lock_child_holds_until_killed",
                "--ignored",
                "--nocapture",
            ])
            .env("FIKO_TEST_WORKSPACE_LOCK_CHILD", &root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("could not start workspace lock child");

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut signaled = false;
        while Instant::now() < deadline {
            if fs::read(&signal).ok().as_deref() == Some(&b"ready"[..]) {
                signaled = true;
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "lock child exited too soon ({status}): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !signaled {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "lock child never signaled: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let error = try_lock_operation(&root)
            .err()
            .expect("parent must not obtain a live child lock");
        assert!(error.contains("Baska bir FIKO"), "{error}");

        child.kill().expect("could not kill lock-owning child");
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(
            root.join(".fiko_backups/workspace.operation.lock")
                .is_file()
        );

        let new_owner = try_lock_operation(&root).unwrap();
        let blocked = try_lock_operation(&root).err().unwrap();
        assert!(blocked.contains("Baska bir FIKO"));
        drop(new_owner);
        try_lock_operation(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_operation_lock_is_never_accepted() {
        use std::os::unix::fs::symlink;

        let project = Fixture::new("symlink_lock");
        let root = project.0.canonicalize().unwrap();
        let lock = try_lock_operation(&root).unwrap();
        drop(lock);
        let path = root.join(".fiko_backups/workspace.operation.lock");
        fs::remove_file(&path).unwrap();
        symlink(root.join("src/a.rs"), &path).unwrap();
        let error = try_lock_operation(&root).err().unwrap();
        assert!(error.contains("normal bir dosya"), "{error}");
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }

    #[test]
    fn writes_marker_before_target_and_audits_result_without_modifying() {
        let project = Fixture::new("audit");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let output = audit(&project.0).unwrap();
        assert!(output.contains("updated"));
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
        finish_verified(&project.0, &record).unwrap();
        assert!(!project.0.join(".fiko_backups").join(MARKER).exists());
        assert!(audit(&project.0).unwrap().contains("kaydi yok"));
    }

    #[test]
    fn forgotten_in_memory_record_leaves_read_only_recovery_evidence() {
        let project = Fixture::new("record_lost");
        let plan = workspace_patch::prepare(
            &project.0,
            &[
                FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 2 }".into(),
                },
                FileChange::Create {
                    path: "src/new.rs".into(),
                    content: "pub fn new_file() {}\n".into(),
                },
            ],
        )
        .unwrap();

        {
            let _record = workspace_patch::apply(&project.0, &plan).unwrap();
            // Simulated restart boundary: intentionally discard the in-memory record.
            // This is not an actual OS process kill.
        }
        let before = fs::read(project.0.join("src/a.rs")).unwrap();
        let report = audit(&project.0).unwrap();
        assert!(report.contains("src/a.rs\": updated"), "{report}");
        assert!(report.contains("src/new.rs\": updated"), "{report}");
        assert!(report.contains("backup=intact"), "{report}");
        assert!(report.contains("backup=not_required"), "{report}");
        assert_eq!(fs::read(project.0.join("src/a.rs")).unwrap(), before);

        let next = workspace_patch::prepare(
            &project.0,
            &[FileChange::Create {
                path: "src/later.rs".into(),
                content: "new".into(),
            }],
        )
        .unwrap();
        let blocked = workspace_patch::apply(&project.0, &next).unwrap_err();
        assert!(blocked.message.contains("belirsiz"), "{blocked}");
        assert!(!project.0.join("src/later.rs").exists());
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn restoration_preview_is_read_only_and_classifies_edit_and_create() {
        let project = Fixture::new("preview");
        let plan = workspace_patch::prepare(
            &project.0,
            &[
                FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 2 }".into(),
                },
                FileChange::Create {
                    path: "src/new.rs".into(),
                    content: "pub fn new_file() {}\n".into(),
                },
            ],
        )
        .unwrap();
        let _record = workspace_patch::apply(&project.0, &plan).unwrap();
        let marker = project.0.join(".fiko_backups").join(MARKER);
        let before = fs::read(&marker).unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["status"], "pending");
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(preview["automatic_restore"], false);
        assert_eq!(preview["requires_explicit_user_approval"], true);
        assert_eq!(
            preview["files"][0]["proposed_action"],
            "restore_original_from_backup"
        );
        assert_eq!(
            preview["files"][1]["proposed_action"],
            "remove_created_candidate"
        );
        assert_eq!(preview["files"][1]["backup_state"], "not_required");
        assert_eq!(fs::read(&marker).unwrap(), before);
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert!(project.0.join("src/new.rs").is_file());
    }

    #[test]
    fn restoration_preview_requires_review_for_external_modification_or_corrupt_backup() {
        let project = Fixture::new("drift_preview");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let backup = record.files[0].backup_path.as_ref().unwrap();
        fs::write(project.0.join("src/a.rs"), "external modification\n").unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], false);
        assert_eq!(preview["files"][0]["target_state"], "unexpected");
        assert_eq!(preview["files"][0]["proposed_action"], "manual_review");

        fs::write(project.0.join("src/a.rs"), "pub fn a() -> u8 { 2 }\n").unwrap();
        fs::write(backup, "tampered backup\n").unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], false);
        assert_eq!(preview["files"][0]["backup_state"], "mismatch");
        assert_eq!(preview["files"][0]["proposed_action"], "manual_review");
    }

    #[test]
    fn restoration_preview_flags_duplicate_and_forged_hash_records() {
        let project = Fixture::new("duplicate_preview");
        let _record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let path = project.0.join(".fiko_backups").join(MARKER);
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let duplicated = manifest["files"][0].clone();
        manifest["files"].as_array_mut().unwrap().push(duplicated);
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], false);
        assert_eq!(
            preview["files"][1]["target_state"],
            "invalid_or_duplicate_target"
        );
        assert_eq!(preview["files"][1]["proposed_action"], "manual_review");

        manifest["files"].as_array_mut().unwrap().pop();
        manifest["files"][0]["updated_sha256"] = json!("forged");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], false);
        assert_eq!(
            preview["files"][0]["target_state"],
            "invalid_or_duplicate_target"
        );
        assert!(path.is_file());

        manifest["files"][0]["updated_sha256"] = json!(sha(b"pub fn a() -> u8 { 2 }\n"));
        manifest["files"][0]
            .as_object_mut()
            .unwrap()
            .remove("backup_name");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], false);
        assert_eq!(preview["files"][0]["proposed_action"], "manual_review");
    }

    #[test]
    fn restoration_preview_marks_original_and_missing_marker_without_mutation() {
        let project = Fixture::new("preview_original");
        let absent: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(absent["status"], "no_pending");
        assert_eq!(absent["eligible_for_review"], false);
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        fs::write(project.0.join("src/a.rs"), "pub fn a() -> u8 { 1 }\n").unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(preview["files"][0]["proposed_action"], "already_original");
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
        // A plan never removes the marker or alters its backup.
        assert!(record.files[0].backup_path.as_ref().unwrap().is_file());
    }

    fn approved_fingerprint(root: &Path) -> String {
        let preview: Value = serde_json::from_str(&restoration_preview(root).unwrap()).unwrap();
        preview["marker_fingerprint_sha256"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn explicit_approval_restores_original_and_removes_created_file() {
        let project = Fixture::new("approved_restore");
        let plan = workspace_patch::prepare(
            &project.0,
            &[
                FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 2 }".into(),
                },
                FileChange::Create {
                    path: "src/new.rs".into(),
                    content: "pub fn new_file() {}\n".into(),
                },
            ],
        )
        .unwrap();
        let _record = workspace_patch::apply(&project.0, &plan).unwrap();
        let fingerprint = approved_fingerprint(&project.0);
        let outcome = restore_after_explicit_approval(&project.0, &fingerprint).unwrap();
        assert!(outcome.contains("RECOVERY_RESTORED"));
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert!(!project.0.join("src/new.rs").exists());
        assert!(!project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn incorrect_approval_never_mutates_targets() {
        let project = Fixture::new("wrong_approval");
        let _record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let error = restore_after_explicit_approval(&project.0, "not-the-approved-fingerprint")
            .unwrap_err();
        assert!(error.contains("parmak iziyle eslesmiyor"), "{error}");
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn external_change_in_one_target_refuses_entire_recovery() {
        let project = Fixture::new("restore_drift");
        let plan = workspace_patch::prepare(
            &project.0,
            &[
                FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 2 }".into(),
                },
                FileChange::Create {
                    path: "src/new.rs".into(),
                    content: "new\n".into(),
                },
            ],
        )
        .unwrap();
        let _record = workspace_patch::apply(&project.0, &plan).unwrap();
        let fingerprint = approved_fingerprint(&project.0);
        fs::write(project.0.join("src/new.rs"), "external edit\n").unwrap();
        assert!(restore_after_explicit_approval(&project.0, &fingerprint).is_err());
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert_eq!(
            fs::read_to_string(project.0.join("src/new.rs")).unwrap(),
            "external edit\n"
        );
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn missing_created_target_requires_fresh_approval_before_marker_cleanup() {
        let project = Fixture::new("created_absent_resume");
        let target = project.0.join("src/new.rs");
        let plan = workspace_patch::prepare(
            &project.0,
            &[FileChange::Create {
                path: "src/new.rs".into(),
                content: "new\\n".into(),
            }],
        )
        .unwrap();
        let _record = workspace_patch::apply(&project.0, &plan).unwrap();

        // Simulate a process interruption after deleting the created file but
        // before finishing the pending workspace record.
        fs::remove_file(&target).unwrap();
        let pending = project.0.join(".fiko_backups").join(MARKER);
        assert!(pending.is_file());

        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(preview["requires_explicit_user_approval"], true);
        assert_eq!(preview["files"][0]["proposed_action"], "already_absent");
        assert!(!target.exists());
        assert!(pending.is_file());

        let next = workspace_patch::prepare(
            &project.0,
            &[FileChange::Create {
                path: "src/later.rs".into(),
                content: "blocked".into(),
            }],
        )
        .unwrap();
        assert!(workspace_patch::apply(&project.0, &next).is_err());
        assert!(!project.0.join("src/later.rs").exists());

        let fingerprint = approved_fingerprint(&project.0);
        restore_after_explicit_approval(&project.0, &fingerprint).unwrap();
        assert!(!target.exists());
        assert!(!pending.exists());
    }

    #[test]
    fn partially_restored_targets_can_resume_after_new_explicit_approval() {
        let project = Fixture::new("restore_resume");
        let plan = workspace_patch::prepare(
            &project.0,
            &[
                FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 2 }".into(),
                },
                FileChange::Create {
                    path: "src/new.rs".into(),
                    content: "new\n".into(),
                },
            ],
        )
        .unwrap();
        let _record = workspace_patch::apply(&project.0, &plan).unwrap();
        fs::write(project.0.join("src/a.rs"), "pub fn a() -> u8 { 1 }\n").unwrap();
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(preview["files"][0]["proposed_action"], "already_original");
        let fingerprint = approved_fingerprint(&project.0);
        restore_after_explicit_approval(&project.0, &fingerprint).unwrap();
        assert!(!project.0.join("src/new.rs").exists());
        assert!(!project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "isolated child process only"]
    fn created_recovery_symlink_swap_child() {
        let Some(root) = std::env::var_os("FIKO_TEST_RECOVERY_CREATE_SYMLINK_RACE") else {
            return;
        };
        let root = PathBuf::from(root);
        let plan = workspace_patch::prepare(
            &root,
            &[FileChange::Create {
                path: "src/new.rs".into(),
                content: "new\n".into(),
            }],
        )
        .unwrap();
        let _record = workspace_patch::apply(&root, &plan).unwrap();
        let fingerprint = approved_fingerprint(&root);
        assert!(restore_after_explicit_approval(&root, &fingerprint).is_err());
        assert!(
            fs::symlink_metadata(root.join("src/new.rs"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(root.join("external_created_recovery.rs")).unwrap(),
            b"new\n"
        );
        assert!(root.join(".fiko_backups").join(MARKER).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn created_file_recovery_rejects_same_content_symlink_at_final_delete_gate() {
        use std::process::Command;

        let project = Fixture::new("recovery_created_symlink_race");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_recovery::tests::created_recovery_symlink_swap_child",
                "--ignored",
                "--nocapture",
            ])
            .env("FIKO_TEST_RECOVERY_CREATE_SYMLINK_RACE", &project.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated recovery test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn tampered_backup_after_review_refuses_restore_without_writing_sources() {
        let project = Fixture::new("late_backup_tampering");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let fingerprint = approved_fingerprint(&project.0);
        let backup = record.files[0].backup_path.as_ref().unwrap();
        fs::write(backup, "outside tampering\n").unwrap();
        let result = restore_after_explicit_approval(&project.0, &fingerprint);
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[cfg(unix)]
    #[test]
    fn linked_target_after_approval_never_writes_outside_project() {
        use std::os::unix::fs::symlink;
        let project = Fixture::new("late_symlink");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let fingerprint = approved_fingerprint(&project.0);
        let original_target = project.0.join("src/a.rs");
        let outside_target = project.0.parent().unwrap().join(format!(
            "fiko_recovery_external_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&outside_target, "do-not-touch\n").unwrap();
        fs::remove_file(&original_target).unwrap();
        symlink(&outside_target, &original_target).unwrap();

        let restore_result = restore_after_explicit_approval(&project.0, &fingerprint);
        let external_bytes = fs::read(&outside_target).unwrap();
        let _ = fs::remove_file(&outside_target);
        assert!(
            restore_result.is_err(),
            "symlink replacement must be blocked"
        );
        assert_eq!(external_bytes, b"do-not-touch\n");
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
        assert!(record.files[0].backup_path.as_ref().unwrap().is_file());
    }

    #[test]
    #[ignore = "invoked only by isolated process-kill parent test"]
    fn restore_child_wait_after_first_file() {
        let Some(root) = std::env::var_os("FIKO_TEST_RESTORE_CRASH_PROJECT") else {
            return;
        };
        let root = PathBuf::from(root).canonicalize().unwrap();
        let fingerprint = approved_fingerprint(&root);
        let result = restore_after_explicit_approval(&root, &fingerprint);
        panic!("restore child escaped process termination: {result:?}");
    }

    #[test]
    #[ignore = "run once in explicit Windows/Linux CI step, not every test suite"]
    fn restore_parent_kills_child_and_resumes() {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        let project = Fixture::new("restore_process_kill");
        fs::write(project.0.join("src/b.rs"), "pub fn b() -> u8 { 2 }\n").unwrap();
        let plan = workspace_patch::prepare(
            &project.0,
            &[
                FileChange::Edit {
                    path: "src/a.rs".into(),
                    old_text: "pub fn a() -> u8 { 1 }".into(),
                    new_text: "pub fn a() -> u8 { 3 }".into(),
                },
                FileChange::Edit {
                    path: "src/b.rs".into(),
                    old_text: "pub fn b() -> u8 { 2 }".into(),
                    new_text: "pub fn b() -> u8 { 4 }".into(),
                },
                FileChange::Create {
                    path: "src/c.rs".into(),
                    content: "pub fn c() {}\n".into(),
                },
            ],
        )
        .unwrap();
        let _record = workspace_patch::apply(&project.0, &plan).unwrap();
        let marker_path = project.0.join(".fiko_backups").join(MARKER);
        let original_marker = fs::read(&marker_path).unwrap();

        let signal = project.0.join(".fiko_test_restore_interrupt_ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_recovery::tests::restore_child_wait_after_first_file",
                "--ignored",
                "--nocapture",
            ])
            .env("FIKO_TEST_RESTORE_CRASH_PROJECT", &project.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to launch dedicated restore child");

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut ready = false;
        while Instant::now() < deadline {
            if fs::read(&signal).ok().as_deref() == Some(&b"ready"[..]) {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "restore child terminated early ({status}): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !ready {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "restore child did not reach first restored file: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        child.kill().expect("cannot kill restore child process");
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());

        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert_eq!(
            fs::read_to_string(project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 4 }\n"
        );
        assert!(project.0.join("src/c.rs").is_file());
        assert_eq!(fs::read(&marker_path).unwrap(), original_marker);

        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(preview["files"][0]["proposed_action"], "already_original");
        assert_eq!(
            preview["files"][1]["proposed_action"],
            "restore_original_from_backup"
        );
        assert_eq!(
            preview["files"][2]["proposed_action"],
            "remove_created_candidate"
        );

        // This explicit library call is a trusted test of host functionality.
        // The user-facing binary separately requires interactive confirmation.
        let fingerprint = approved_fingerprint(&project.0);
        let result = restore_after_explicit_approval(&project.0, &fingerprint).unwrap();
        assert!(result.contains("RECOVERY_RESTORED"));
        assert_eq!(
            fs::read_to_string(project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 2 }\n"
        );
        assert!(!project.0.join("src/c.rs").exists());
        assert!(!marker_path.exists());
    }

    #[test]
    #[ignore = "only invoked by the isolated staged-restore kill test"]
    fn staged_restore_child_waits_before_replace() {
        let Some(root) = std::env::var_os("FIKO_TEST_RESTORE_STAGE_KILL_PROJECT") else {
            return;
        };
        let root = PathBuf::from(root).canonicalize().unwrap();
        let fingerprint = approved_fingerprint(&root);
        let result = restore_after_explicit_approval(&root, &fingerprint);
        panic!("stage child reached completion without forced termination: {result:?}");
    }

    #[test]
    #[ignore = "dedicated cross-platform process kill regression"]
    fn process_kill_during_staging_preserves_candidate_and_allows_reapproved_restore() {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        let project = Fixture::new("restore_staged_process_kill");
        let plan = project.plan();
        let record = workspace_patch::apply(&project.0, &plan).unwrap();
        let marker = project.0.join(".fiko_backups").join(MARKER);
        let marker_before = fs::read(&marker).unwrap();
        let signal = project.0.join(".fiko_test_restore_stage_ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_recovery::tests::staged_restore_child_waits_before_replace",
                "--ignored",
                "--nocapture",
            ])
            .env("FIKO_TEST_RESTORE_STAGE_KILL_PROJECT", &project.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cannot launch staged restore child");

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut ready = false;
        while Instant::now() < deadline {
            if fs::read(&signal).ok().as_deref() == Some(&b"ready"[..]) {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "staged restore child exited before kill point ({status}): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !ready {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "staged restore child did not emit signal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        child
            .kill()
            .expect("failed to terminate staged restore child");
        let child_output = child.wait_with_output().unwrap();
        assert!(!child_output.status.success());

        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert_eq!(fs::read(&marker).unwrap(), marker_before);
        assert!(record.files[0].backup_path.as_ref().unwrap().is_file());
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(
            preview["files"][0]["proposed_action"],
            "restore_original_from_backup"
        );

        let stage_count = fs::read_dir(project.0.join("src"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".fiko_restore_stage_")
            })
            .count();
        assert_eq!(stage_count, 1, "killed child should leave one staged copy");

        // A fresh, separately approved host recovery can safely finish despite
        // the orphan stage. It does not silently delete orphan evidence.
        restore_after_explicit_approval(&project.0, &approved_fingerprint(&project.0)).unwrap();
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert!(!marker.exists());
    }

    #[test]
    #[ignore = "run only via dedicated real-process interruption parent"]
    fn partial_stage_child_waits_during_write() {
        let Some(root) = std::env::var_os("FIKO_TEST_PARTIAL_STAGE_KILL_PROJECT") else {
            return;
        };
        let root = PathBuf::from(root).canonicalize().unwrap();
        let fingerprint = approved_fingerprint(&root);
        let result = restore_after_explicit_approval(&root, &fingerprint);
        panic!("partial-stage child was not terminated: {result:?}");
    }

    #[test]
    #[ignore = "run once in cross-platform CI, not in repeated stress runs"]
    fn process_kill_mid_stage_write_keeps_target_and_marker_for_reapproval() {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        let project = Fixture::new("restore_partial_stage_kill");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let marker = project.0.join(".fiko_backups").join(MARKER);
        let previous_marker = fs::read(&marker).unwrap();
        let candidate_before = fs::read(project.0.join("src/a.rs")).unwrap();
        let original = fs::read(record.files[0].backup_path.as_ref().unwrap()).unwrap();
        let signal = project.0.join(".fiko_test_partial_stage_ready");

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_recovery::tests::partial_stage_child_waits_during_write",
                "--ignored",
                "--nocapture",
            ])
            .env("FIKO_TEST_PARTIAL_STAGE_KILL_PROJECT", &project.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cannot start isolated partial-stage child");

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut ready = false;
        while Instant::now() < deadline {
            if fs::read(&signal).ok().as_deref() == Some(&b"ready"[..]) {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "partial-stage child exited too early ({status}): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
        if !ready {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "partial-stage child did not reach ready signal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        child.kill().expect("cannot terminate partial-stage child");
        let output = child.wait_with_output().unwrap();
        assert!(
            !output.status.success(),
            "child must be forcefully terminated"
        );

        assert_eq!(fs::read(&marker).unwrap(), previous_marker);
        assert_eq!(
            fs::read(project.0.join("src/a.rs")).unwrap(),
            candidate_before
        );
        let stage_files: Vec<_> = fs::read_dir(project.0.join("src"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".fiko_restore_stage_")
            })
            .collect();
        assert_eq!(stage_files.len(), 1, "expected one orphan staged file");
        let orphan = fs::read(stage_files[0].path()).unwrap();
        assert_eq!(orphan.len(), (original.len() / 2).max(1));
        assert_ne!(sha(&orphan), sha(&original));
        assert_eq!(orphan, original[..orphan.len()]);

        let report: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(report["eligible_for_review"], true);
        assert_eq!(
            report["files"][0]["proposed_action"],
            "restore_original_from_backup"
        );
        // A new approval can restore from the verified backup, not the
        // untrusted, incomplete staged orphan left by the terminated process.
        let result =
            restore_after_explicit_approval(&project.0, &approved_fingerprint(&project.0)).unwrap();
        assert!(result.contains("RECOVERY_RESTORED"));
        assert_eq!(fs::read(project.0.join("src/a.rs")).unwrap(), original);
        assert!(!marker.exists());
        assert!(
            stage_files[0].path().exists(),
            "orphan evidence remains for review"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_restore_does_not_modify_external_hard_link() {
        let project = Fixture::new("restore_hardlink");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let external = project.0.parent().unwrap().join(format!(
            "fiko_external_hardlink_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::hard_link(project.0.join("src/a.rs"), &external).unwrap();
        assert_eq!(
            fs::read_to_string(&external).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        let result = restore_after_explicit_approval(&project.0, &approved_fingerprint(&project.0));
        let outside_content = fs::read_to_string(&external).unwrap();
        let _ = fs::remove_file(&external);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert_eq!(outside_content, "pub fn a() -> u8 { 2 }\n");
        assert!(!project.0.join(".fiko_backups").join(MARKER).exists());
        assert!(record.files[0].backup_path.as_ref().unwrap().is_file());
    }

    #[test]
    fn staged_restore_preserves_source_permissions_when_supported() {
        let project = Fixture::new("restore_permissions");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let source = project.0.join("src/a.rs");
            fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        restore_after_explicit_approval(&project.0, &approved_fingerprint(&project.0)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(project.0.join("src/a.rs"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn staged_copy_integrity_detects_same_length_corruption() {
        let project = Fixture::new("staging_hash");
        let stage = project.0.join("src/.staged.tmp");
        fs::write(&stage, b"good").unwrap();
        assert!(verify_staged_copy(&stage, b"good").is_ok());
        fs::write(&stage, b"evil").unwrap();
        assert!(verify_staged_copy(&stage, b"good").is_err());
        fs::write(&stage, b"oversized").unwrap();
        assert!(verify_staged_copy(&stage, b"good").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn staged_copy_integrity_rejects_symlink() {
        use std::os::unix::fs::symlink;
        let project = Fixture::new("staging_symlink");
        let safe = project.0.join("src/a.rs");
        let stage = project.0.join("src/.staged.tmp");
        symlink(&safe, &stage).unwrap();
        let original = fs::read(&safe).unwrap();
        assert!(verify_staged_copy(&stage, &original).is_err());
    }

    #[test]
    #[ignore = "only invoked as an isolated test subprocess"]
    fn staged_tamper_child_rejects_precommit_corruption() {
        let Some(root) = std::env::var_os("FIKO_TEST_TAMPER_STAGED_COPY") else {
            return;
        };
        let root = PathBuf::from(root).canonicalize().unwrap();
        let fingerprint = approved_fingerprint(&root);
        let err = restore_after_explicit_approval(&root, &fingerprint).unwrap_err();
        assert!(err.contains("SHA256"), "{err}");
    }

    #[test]
    fn tampered_stage_rejects_replacement_and_preserves_pending_marker() {
        use std::process::Command;

        let project = Fixture::new("stage_tampered_precommit");
        let _record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let source = project.0.join("src/a.rs");
        let marker = project.0.join(".fiko_backups").join(MARKER);
        let original_source = fs::read(&source).unwrap();
        let original_marker = fs::read(&marker).unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_recovery::tests::staged_tamper_child_rejects_precommit_corruption",
                "--ignored",
                "--nocapture",
            ])
            .env("FIKO_TEST_TAMPER_STAGED_COPY", &project.0)
            .output()
            .expect("cannot start dedicated tamper child");
        assert!(
            output.status.success(),
            "tamper child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(&source).unwrap(), original_source);
        assert_eq!(fs::read(&marker).unwrap(), original_marker);
        assert_eq!(
            fs::read_dir(project.0.join("src"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".fiko_restore_stage_"))
                .count(),
            0
        );
        let preview: Value =
            serde_json::from_str(&restoration_preview(&project.0).unwrap()).unwrap();
        assert_eq!(preview["eligible_for_review"], true);
        assert_eq!(
            preview["files"][0]["proposed_action"],
            "restore_original_from_backup"
        );
    }

    #[test]
    fn changed_marker_invalidates_prior_approval_without_writes() {
        let project = Fixture::new("restore_marker_changed");
        let _record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let fingerprint = approved_fingerprint(&project.0);
        let marker = project.0.join(".fiko_backups").join(MARKER);
        let mut json: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        json["comment"] = json!("modified marker after approval");
        fs::write(&marker, serde_json::to_vec(&json).unwrap()).unwrap();
        let error = restore_after_explicit_approval(&project.0, &fingerprint).unwrap_err();
        assert!(error.contains("parmak iziyle eslesmiyor"), "{error}");
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert!(marker.exists());
    }

    #[test]
    fn unclosed_marker_blocks_second_transaction() {
        let project = Fixture::new("blocked");
        let _record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let next = workspace_patch::prepare(
            &project.0,
            &[FileChange::Edit {
                path: "src/a.rs".into(),
                old_text: "pub fn a() -> u8 { 2 }".into(),
                new_text: "pub fn a() -> u8 { 3 }".into(),
            }],
        )
        .unwrap();
        let error = workspace_patch::apply(&project.0, &next).unwrap_err();
        assert!(error.restored);
        assert!(error.message.contains("belirsiz"));
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
    }

    #[test]
    fn pending_workspace_blocks_single_file_apply_before_backup_and_write() {
        let project = Fixture::new("single_guard");
        let root = project.0.canonicalize().unwrap();
        let record = workspace_patch::apply(&root, &project.plan()).unwrap();
        let backups = project.0.join(".fiko_backups");
        let entries_before = fs::read_dir(&backups).unwrap().count();

        let single = crate::tools::prepare_patch(
            &root,
            "src/a.rs",
            "pub fn a() -> u8 { 2 }",
            "pub fn a() -> u8 { 3 }",
        )
        .unwrap();
        let error = crate::tools::apply_approved_patch(&root, single)
            .err()
            .expect("pending marker must reject single-file write");
        assert!(error.contains("belirsiz"), "{error}");
        assert_eq!(fs::read_dir(&backups).unwrap().count(), entries_before);
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert!(backups.join(MARKER).exists());

        workspace_patch::rollback(&root, &record).unwrap();
        let cleared = crate::tools::prepare_patch(
            &root,
            "src/a.rs",
            "pub fn a() -> u8 { 1 }",
            "pub fn a() -> u8 { 3 }",
        )
        .unwrap();
        let single_record = crate::tools::apply_approved_patch(&root, cleared).unwrap();
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 3 }\n"
        );
        crate::tools::rollback_patch(&single_record).unwrap();
    }

    #[test]
    fn invalid_pending_marker_blocks_single_file_apply() {
        let project = Fixture::new("single_invalid_marker");
        let root = project.0.canonicalize().unwrap();
        let backups = project.0.join(".fiko_backups");
        fs::create_dir(&backups).unwrap();
        fs::write(backups.join(MARKER), b"{not-json").unwrap();

        let single = crate::tools::prepare_patch(
            &root,
            "src/a.rs",
            "pub fn a() -> u8 { 1 }",
            "pub fn a() -> u8 { 3 }",
        )
        .unwrap();
        let error = crate::tools::apply_approved_patch(&root, single)
            .err()
            .expect("pending marker must reject single-file write");
        assert!(error.contains("belirsiz"), "{error}");
        assert_eq!(fs::read_dir(&backups).unwrap().count(), 1);
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }

    #[test]
    fn rollback_closes_only_matching_and_fully_restored_marker() {
        let project = Fixture::new("rollback");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        workspace_patch::rollback(&project.0, &record).unwrap();
        assert!(!project.0.join(".fiko_backups").join(MARKER).exists());
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }

    #[test]
    fn changed_target_keeps_marker_and_fails_closed() {
        let project = Fixture::new("drift");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        fs::write(project.0.join("src/a.rs"), "external change\n").unwrap();
        assert!(finish_verified(&project.0, &record).is_err());
        assert!(workspace_patch::rollback(&project.0, &record).is_err());
        assert!(audit(&project.0).unwrap().contains("unexpected"));
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn create_only_workspace_has_marker_and_no_backup_requirement() {
        let project = Fixture::new("create_only");
        let plan = workspace_patch::prepare(
            &project.0,
            &[FileChange::Create {
                path: "src/added.rs".into(),
                content: "pub fn added() -> bool { true }\n".into(),
            }],
        )
        .unwrap();
        let record = workspace_patch::apply(&project.0, &plan).unwrap();
        let path = project.0.join("src/added.rs");
        assert!(path.is_file());
        assert!(audit(&project.0).unwrap().contains("backup=not_required"));
        finish_verified(&project.0, &record).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "pub fn added() -> bool { true }\n"
        );
        assert!(!project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn corrupted_backup_keeps_recovery_marker_and_does_not_overwrite_target() {
        let project = Fixture::new("backup_mismatch");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let backup = record.files[0].backup_path.as_ref().unwrap();
        fs::write(backup, b"backup tampered").unwrap();
        assert!(audit(&project.0).unwrap().contains("backup=mismatch"));
        assert!(finish_verified(&project.0, &record).is_err());
        assert!(workspace_patch::rollback(&project.0, &record).is_err());
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 2 }\n"
        );
        assert!(project.0.join(".fiko_backups").join(MARKER).exists());
    }

    #[test]
    fn malformed_marker_blocks_new_writes_without_changing_sources() {
        let project = Fixture::new("invalid_marker");
        let folder = project.0.join(".fiko_backups");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(MARKER), b"{invalid-json").unwrap();
        assert!(audit(&project.0).is_err());
        let err = workspace_patch::apply(&project.0, &project.plan()).unwrap_err();
        assert!(err.restored);
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert_eq!(fs::read(folder.join(MARKER)).unwrap(), b"{invalid-json");
    }

    #[test]
    fn forged_backup_path_is_rejected_without_reading_external_files() {
        let project = Fixture::new("forged_reference");
        let record = workspace_patch::apply(&project.0, &project.plan()).unwrap();
        let marker = project.0.join(".fiko_backups").join(MARKER);
        let mut value: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        value["files"][0]["backup_name"] = json!("../outside.bak");
        fs::write(&marker, serde_json::to_vec(&value).unwrap()).unwrap();
        let audit_before = fs::read(&marker).unwrap();
        assert!(
            audit(&project.0)
                .unwrap()
                .contains("backup=unsafe_reference")
        );
        assert_eq!(fs::read(&marker).unwrap(), audit_before);
        assert!(finish_verified(&project.0, &record).is_err());
        assert!(marker.exists());
        // A Windows drive-relative or ADS name must never escape the backup directory.
        value["files"][0]["backup_name"] = json!("C:outside.bak");
        fs::write(&marker, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            audit(&project.0)
                .unwrap()
                .contains("backup=unsafe_reference")
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_backup_directory_is_rejected_without_writing_source() {
        use std::os::unix::fs::symlink;
        let project = Fixture::new("linked_dir");
        let external = Fixture::new("external");
        symlink(&external.0, project.0.join(".fiko_backups")).unwrap();
        assert!(audit(&project.0).is_err());
        let err = workspace_patch::apply(&project.0, &project.plan()).unwrap_err();
        assert!(err.restored);
        assert_eq!(
            fs::read_to_string(project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }
}
