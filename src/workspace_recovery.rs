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
        "warning": "READ_ONLY preview. Recheck all file and backup hashes at restore time. No restoration is implemented here.",
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
    let (mut staged, stage_path) = created.ok_or("Gecici dosya adlari kullaniliyor.")?;
    // If the process dies, Drop is not invoked and an orphan staging file may
    // remain for manual review. On ordinary failures, clean up our own file.
    struct StageCleanup(PathBuf);
    impl Drop for StageCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let cleanup = StageCleanup(stage_path.clone());
    staged.write_all(bytes).map_err(|e| e.to_string())?;
    staged.sync_all().map_err(|e| e.to_string())?;
    // Preserve source permissions, including Unix executable bits. Timestamp
    // metadata may change and is not guaranteed to be preserved.
    fs::set_permissions(&stage_path, opened_metadata.permissions())
        .map_err(|e| e.to_string())?;
    let staged_metadata = fs::symlink_metadata(&stage_path).map_err(|e| e.to_string())?;
    if !staged_metadata.is_file() || is_link(&staged_metadata) {
        return Err("Gecici dosya normal bir dosya degil.".into());
    }
    drop(staged);

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
    if sha(&serde_json::to_vec(&marker).map_err(|e| e.to_string())?)
        != expected_fingerprint
    {
        return Err("Kurtarma kaydi gecici dosya hazirlanirken degisti.".into());
    }
    if discovery::resolve_existing(root, &step.relative)? != step.target
        || file_state(root, &step.relative, step.original_sha256.as_deref(), &step.updated_sha256)
            != "updated"
    {
        return Err("Hedef gecici dosya hazirlanirken degisti; degisiklik durduruldu.".into());
    }
    // Release the source handle before rename, especially on Windows.
    drop(opened);
    fs::rename(&stage_path, &step.target)
        .map_err(|e| format!("Gecici dosya hedefin yerine konulamadi: {e}"))?;
    drop(cleanup);
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
                let bytes = step
                    .backup_bytes
                    .as_ref()
                    .ok_or("Geri yukleme yedegi kayip.")?;
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
