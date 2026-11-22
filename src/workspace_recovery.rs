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
        lines.push(format!(
            "{}: {state}",
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
}
