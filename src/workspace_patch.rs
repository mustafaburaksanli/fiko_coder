//! Sinirli cok dosyali patch plani, yedekleme ve geri alma.
//!
//! Butun hedefler yazmadan once denetlenir. Her dosyanin yazimi ve tum islem
//! birlikte crash-safe/atomik degildir; disk yedekleri ve Rust kaydi kurtarma
//! icindir. Harici sureclerle dosya sistemi kilidi saglanmaz.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::{discovery, tools, workspace_recovery};

pub const MAX_WORKSPACE_FILES: usize = 16;
pub const MAX_FILE_BYTES: usize = 128_000;
pub const MAX_CHANGE_BYTES: usize = 8_000;
pub const MAX_PLAN_BYTES: usize = 512_000;
pub const MAX_DIFF_BYTES: usize = 64_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileChange {
    Edit {
        path: String,
        old_text: String,
        new_text: String,
    },
    Create {
        path: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlannedFile {
    pub relative: String,
    pub path: PathBuf,
    /// None: hedefin onaydan once bulunmamasi gerekir.
    pub original: Option<String>,
    pub updated: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspacePlan {
    pub files: Vec<PlannedFile>,
}

impl WorkspacePlan {
    /// Tam degisiklikler gosterilir; prepare/revalidate, kesmek yerine cok
    /// buyuk diff'i reddeder. Degismeyen dosya bolumleri modele yuklenmez.
    pub fn diff_summary(&self) -> String {
        let mut output = format!("[WORKSPACE PLANI] {} dosya\n", self.files.len());
        for file in &self.files {
            let before = file.original.as_deref().unwrap_or("");
            output.push_str(&format!(
                "--- {}\n+++ {}\n",
                if file.original.is_some() {
                    file.relative.as_str()
                } else {
                    "/dev/null"
                },
                file.relative
            ));
            append_diff(&mut output, before, &file.updated);
        }
        output
    }
}

fn append_diff(output: &mut String, before: &str, after: &str) {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = prefix.saturating_sub(2);
    let old_end = (old.len() - suffix + 2).min(old.len());
    let new_end = (new.len() - suffix + 2).min(new.len());
    output.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        if old.is_empty() { 0 } else { start + 1 },
        old_end - start,
        if new.is_empty() { 0 } else { start + 1 },
        new_end - start
    ));
    for line in &old[start..prefix] {
        diff_line(output, ' ', line);
    }
    for line in &old[prefix..old.len() - suffix] {
        diff_line(output, '-', line);
    }
    for line in &new[prefix..new.len() - suffix] {
        diff_line(output, '+', line);
    }
    for line in &old[old.len() - suffix..old_end] {
        diff_line(output, ' ', line);
    }
}

fn diff_line(output: &mut String, marker: char, line: &str) {
    output.push(marker);
    // Onizleme terminali temizleyen/gizleyen diziler calistiramaz. Gercek
    // aday byte'lari degismez; yalniz insan gorunumu gorunur kacis kullanir.
    for character in line.chars() {
        if (character.is_control() && character != '\n')
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    if !line.ends_with('\n') {
        output.push_str("\n\\ No newline at end of file\n");
    }
}

/// Yalniz UTF-8 metin kabul edilir; buyuyen dosya da sinirli okunur.
fn read_text(path: &Path) -> Result<String, String> {
    if !fs::metadata(path)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Hedef normal bir dosya olmali.".into());
    }
    let mut text = String::new();
    File::open(path)
        .map_err(|e| format!("{} acilamadi: {e}", path.display()))?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|e| format!("{} okunamadi: {e}", path.display()))?;
    if text.len() > MAX_FILE_BYTES {
        return Err(format!("Dosya {MAX_FILE_BYTES} byte sinirini asiyor."));
    }
    Ok(text)
}

fn root_path(root: &Path) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err("Proje koku bir dizin olmali.".into());
    }
    Ok(root)
}

fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|_| "Hedef proje koku disina cikiyor.".into())
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn check_shape(plan: &WorkspacePlan) -> Result<(), String> {
    if plan.files.is_empty() || plan.files.len() > MAX_WORKSPACE_FILES {
        return Err(format!("Plan 1-{MAX_WORKSPACE_FILES} dosya icermeli."));
    }
    let mut paths = HashSet::new();
    let mut bytes = 0usize;
    for file in &plan.files {
        if !paths.insert(path_key(&file.path)) {
            return Err("Ayni dosya icin cakisan islemler reddedildi.".into());
        }
        if file.updated.len() > MAX_FILE_BYTES
            || file
                .original
                .as_ref()
                .is_some_and(|s| s.len() > MAX_FILE_BYTES)
        {
            return Err(format!("Dosya {MAX_FILE_BYTES} byte sinirini asiyor."));
        }
        if file.original.as_ref() == Some(&file.updated) {
            return Err("Plan degisiklik icermeyen bir duzenleme iceriyor.".into());
        }
        bytes += file.updated.len() + file.original.as_ref().map_or(0, String::len);
        if bytes > MAX_PLAN_BYTES {
            return Err(format!("Plan {MAX_PLAN_BYTES} byte sinirini asiyor."));
        }
    }
    if plan.diff_summary().len() > MAX_DIFF_BYTES {
        return Err(format!(
            "Tam diff {MAX_DIFF_BYTES} byte sinirini asiyor; daha kucuk plan hazirlayin."
        ));
    }
    Ok(())
}

pub fn prepare(root: &Path, changes: &[FileChange]) -> Result<WorkspacePlan, String> {
    let options: Vec<_> = changes
        .iter()
        .take(MAX_WORKSPACE_FILES)
        .map(|_| tools::PatchMatchOptions::default())
        .collect();
    prepare_with_options(root, changes, &options)
}

/// Paylasilan eslesme tanisina yalniz degisiklik dizisinin Rust indeksini ekler.
pub(crate) fn error_for_change(message: String, change_index: usize) -> String {
    if let Ok(mut diagnostic) = serde_json::from_str::<serde_json::Value>(&message)
        && diagnostic.is_object()
    {
        diagnostic["change_index"] = serde_json::json!(change_index);
        return diagnostic.to_string();
    }
    message
}

pub fn prepare_with_options(
    root: &Path,
    changes: &[FileChange],
    options: &[tools::PatchMatchOptions],
) -> Result<WorkspacePlan, String> {
    if changes.is_empty() || changes.len() > MAX_WORKSPACE_FILES {
        return Err(format!("Plan 1-{MAX_WORKSPACE_FILES} dosya icermeli."));
    }
    if options.len() != changes.len() {
        return Err("Her degisiklik icin tam bir eslesme secenegi gerekir.".into());
    }
    let root = root_path(root)?;
    let mut files = Vec::with_capacity(changes.len());
    for (change_index, (change, options)) in changes.iter().zip(options).enumerate() {
        let (path, original, updated) = match change {
            FileChange::Edit {
                path,
                old_text,
                new_text,
            } => {
                if old_text.is_empty()
                    || old_text.len() > MAX_CHANGE_BYTES
                    || new_text.len() > MAX_CHANGE_BYTES
                    || old_text == new_text
                {
                    return Err(format!(
                        "Edit icin bos olmayan old_text ve farkli new_text gerekir; her biri en fazla {MAX_CHANGE_BYTES} byte."
                    ));
                }
                let full = discovery::resolve_existing(&root, path)?;
                let original = read_text(&full)?;
                let updated = tools::replace_line_block(&original, old_text, new_text, options)
                    .map_err(|error| error_for_change(error, change_index))?;
                (full, Some(original), updated)
            }
            FileChange::Create { path, content } => {
                if options.mode != tools::PatchMatchMode::Exact || options.expected_sha256.is_some()
                {
                    return Err(
                        "Yeni dosya olustururken eslesme modu veya kaynak SHA256 kullanilamaz."
                            .into(),
                    );
                }
                if content.len() > MAX_CHANGE_BYTES {
                    return Err(format!(
                        "Yeni dosya icerigi {MAX_CHANGE_BYTES} byte sinirini asiyor."
                    ));
                }
                let full = discovery::resolve_target(&root, path)?;
                if fs::symlink_metadata(&full).is_ok() {
                    return Err(format!("{path}: Yeni dosya hedefi zaten mevcut."));
                }
                (full, None, content.clone())
            }
        };
        files.push(PlannedFile {
            relative: relative_path(&root, &path)?,
            path,
            original,
            updated,
        });
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    let plan = WorkspacePlan { files };
    revalidate(&root, &plan)?;
    Ok(plan)
}

fn revalidate_file(root: &Path, file: &PlannedFile) -> Result<(), String> {
    let current_path = if file.original.is_some() {
        discovery::resolve_existing(root, &file.relative)?
    } else {
        discovery::resolve_target(root, &file.relative)?
    };
    if current_path != file.path {
        return Err(format!("{}: Hedef yolu degisti.", file.relative));
    }
    match &file.original {
        Some(original) => {
            if read_text(&current_path)? != *original {
                return Err(format!(
                    "{}: Dosya plan hazirlandiktan sonra degisti.",
                    file.relative
                ));
            }
        }
        None => match fs::symlink_metadata(&current_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
            Ok(_) => {
                return Err(format!(
                    "{}: Yeni dosya hedefi artik mevcut.",
                    file.relative
                ));
            }
        },
    }
    Ok(())
}

pub fn revalidate(root: &Path, plan: &WorkspacePlan) -> Result<(), String> {
    check_shape(plan)?;
    let root = root_path(root)?;
    for file in &plan.files {
        revalidate_file(&root, file)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RecordedFile {
    pub file: PlannedFile,
    pub backup_path: Option<PathBuf>,
    // Yalniz bu islemin basariyla yazdigi byte'lar. Kismi yazimda da rollback
    // baska bir surecin farkli icerigini ezmeden beklenen prefix'i denetler.
    written: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub files: Vec<RecordedFile>,
}

impl WorkspaceRecord {
    /// Yalniz bu islemin gercekte yazdigi byte'lar; model hedef listesinden turetilmez.
    pub(crate) fn modified_paths(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|record| {
                record.written.as_ref().is_some_and(|written| {
                    record
                        .file
                        .original
                        .as_ref()
                        .is_none_or(|original| written != original.as_bytes())
                })
            })
            .map(|record| record.file.relative.clone())
            .collect()
    }
    /// Geri alma raporu, yazma baslamadan olusan hatayi uygulama saymaz.
    pub(crate) fn has_writes(&self) -> bool {
        self.files.iter().any(|record| {
            record.written.as_ref().is_some_and(|written| {
                record
                    .file
                    .original
                    .as_ref()
                    .is_none_or(|original| written != original.as_bytes())
            })
        })
    }
}

#[derive(Debug)]
pub struct ApplyError {
    pub message: String,
    /// Bu islemin hedef yazimlari olmadi veya hepsi geri alindi.
    pub restored: bool,
    pub record: Option<WorkspaceRecord>,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApplyError {}

fn before_write_error(message: String) -> ApplyError {
    ApplyError {
        message,
        restored: true,
        record: None,
    }
}

pub fn apply(root: &Path, plan: &WorkspacePlan) -> Result<WorkspaceRecord, ApplyError> {
    apply_with(root, plan, |_, _| Ok(()))
}

fn apply_with(
    root: &Path,
    plan: &WorkspacePlan,
    mut before_write: impl FnMut(usize, &PlannedFile) -> Result<(), String>,
) -> Result<WorkspaceRecord, ApplyError> {
    let root = root_path(root).map_err(before_write_error)?;
    revalidate(&root, plan).map_err(before_write_error)?;
    workspace_recovery::ensure_clear(&root).map_err(before_write_error)?;
    let mut record = WorkspaceRecord {
        files: Vec::with_capacity(plan.files.len()),
    };
    // Butun eski icerik yedeklenmeden hicbir hedef dosya yazilmaz.
    for file in &plan.files {
        let backup_path = file
            .original
            .as_ref()
            .map(|original| tools::save_backup(&root, &file.path, original))
            .transpose()
            .map_err(before_write_error)?;
        record.files.push(RecordedFile {
            file: file.clone(),
            backup_path,
            written: None,
        });
    }
    // The durable marker is written before the first target mutation.
    // If a previous crash left one behind, the new transaction fails closed.
    workspace_recovery::start(&root, &record).map_err(before_write_error)?;
    for index in 0..record.files.len() {
        let result = before_write(index, &record.files[index].file)
            .and_then(|_| revalidate_file(&root, &record.files[index].file))
            .and_then(|_| write_file(&mut record.files[index]));
        if let Err(error) = result {
            let recovery = rollback(&root, &record);
            let restored = recovery.is_ok();
            let message = match recovery {
                Ok(()) => format!("Uygulama basarisiz: {error}. Tum hedef yazimlari geri alindi."),
                Err(recovery) => format!(
                    "Uygulama basarisiz: {error}. Geri alma tamamlanamadi; proje durumu BELIRSIZ. {recovery}"
                ),
            };
            return Err(ApplyError {
                message,
                restored,
                record: Some(record),
            });
        }
    }
    Ok(record)
}

fn write_file(record: &mut RecordedFile) -> Result<(), String> {
    let file = &record.file;
    let mut options = OpenOptions::new();
    options.write(true);
    if file.original.is_none() {
        options.create_new(true);
    }
    let mut handle = options
        .open(&file.path)
        .map_err(|e| format!("{} yazmak icin acilamadi: {e}", file.relative))?;
    // truncate, kismi write ve sync hatalari da kurtarma kaydina dahildir.
    record.written = Some(file.original.as_deref().unwrap_or("").as_bytes().to_vec());
    handle
        .set_len(0)
        .map_err(|e| format!("{} kisaltilamadi: {e}", file.relative))?;
    let expected = record.written.as_mut().unwrap();
    expected.clear();
    let bytes = file.updated.as_bytes();
    while expected.len() < bytes.len() {
        let count = handle
            .write(&bytes[expected.len()..])
            .map_err(|e| format!("{} yazilamadi: {e}", file.relative))?;
        if count == 0 {
            return Err(format!("{} yazimi ilerlemedi.", file.relative));
        }
        expected.extend_from_slice(&bytes[expected.len()..expected.len() + count]);
    }
    handle
        .sync_all()
        .map_err(|e| format!("{} kaydedilemedi: {e}", file.relative))?;
    if read_bytes(&file.path)? != bytes {
        return Err(format!("{} yazilan icerik dogrulanamadi.", file.relative));
    }
    Ok(())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err("Dosya geri alma boyut sinirini asiyor.".into());
    }
    Ok(bytes)
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

fn backup_content(root: &Path, record: &RecordedFile) -> Result<Vec<u8>, String> {
    let backup = record
        .backup_path
        .as_ref()
        .ok_or("Mevcut dosyanin yedek kaydi yok.")?;
    let directory = root.join(".fiko_backups");
    let directory_meta = fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
    let backup_meta = fs::symlink_metadata(backup).map_err(|e| e.to_string())?;
    if is_link(&directory_meta)
        || !directory_meta.is_dir()
        || is_link(&backup_meta)
        || !backup_meta.is_file()
        || backup.parent() != Some(directory.as_path())
        || !backup
            .canonicalize()
            .map_err(|e| e.to_string())?
            .starts_with(root)
    {
        return Err("Yedek yolu guvenli bir proje yedegi degil.".into());
    }
    let content = read_bytes(backup)?;
    if record.file.original.as_deref().map(str::as_bytes) != Some(content.as_slice()) {
        return Err("Yedek icerigi beklenen orijinal icerikle eslesmiyor.".into());
    }
    Ok(content)
}

fn rollback_file(root: &Path, record: &RecordedFile) -> Result<(), String> {
    let Some(expected) = &record.written else {
        return Ok(());
    };
    let file = &record.file;
    let target = discovery::resolve_target(root, &file.relative)?;
    if target != file.path {
        return Err("Hedef yolu degisti; geri alma reddedildi.".into());
    }
    match &file.original {
        Some(original) => {
            let backup = backup_content(root, record)?;
            let current = read_bytes(&target)?;
            if current == original.as_bytes() {
                return Ok(());
            }
            if current != *expected {
                return Err("Dosya islem sonrasinda degismis; dis degisiklik ezilmedi.".into());
            }
            let mut handle = OpenOptions::new()
                .write(true)
                .open(&target)
                .map_err(|e| e.to_string())?;
            handle.set_len(0).map_err(|e| e.to_string())?;
            handle.write_all(&backup).map_err(|e| e.to_string())?;
            handle.sync_all().map_err(|e| e.to_string())?;
            if read_bytes(&target)? != original.as_bytes() {
                return Err("Geri alinan dosya dogrulanamadi.".into());
            }
            Ok(())
        }
        None => {
            match fs::symlink_metadata(&target) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.to_string()),
                Ok(_) => {}
            }
            if read_bytes(&target)? != *expected {
                return Err("Yeni dosya islem sonrasinda degismis; dosya silinmedi.".into());
            }
            fs::remove_file(&target).map_err(|e| e.to_string())?;
            if fs::symlink_metadata(&target).is_ok() {
                return Err("Yeni dosyanin kaldirildigi dogrulanamadi.".into());
            }
            Ok(())
        }
    }
}

/// Bir hedefteki hata diger hedeflerin geri alinmasini engellemez. Kayit veya
/// yedekler temizlenmez; belirsiz durumda kullaniciya kurtarma kaniti kalir.
pub fn rollback(root: &Path, record: &WorkspaceRecord) -> Result<(), String> {
    let root = root_path(root)?;
    workspace_recovery::assert_rollback_record(&root, record)?;
    let mut errors = Vec::new();
    for file in record.files.iter().rev() {
        if let Err(error) = rollback_file(&root, file) {
            errors.push(format!("{}: {error}", file.file.relative));
        }
    }
    if errors.is_empty() {
        workspace_recovery::finish_rollback(&root, record)
    } else {
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_terminal_kontrol_dizilerini_gorunur_yapmali() {
        let text = "baslik\u{1b}[2J\r\u{202e}gizli\n";
        let mut output = String::new();
        diff_line(&mut output, '+', text);
        assert!(!output.contains(['\u{1b}', '\r', '\u{202e}']));
        assert!(output.contains("\\u{1b}[2J\\r\\u{202e}"));
        assert!(output.starts_with("+baslik"));
        assert!(output.ends_with("gizli\n"));
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fiko_workspace_patch_{}_{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(path.join("src/a.rs"), "pub fn a() -> u8 { 1 }\n").unwrap();
            fs::write(path.join("src/b.rs"), "pub fn b() -> u8 { 2 }\n").unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let temporary_root = std::env::temp_dir().canonicalize().unwrap();
            assert!(self.0.starts_with(temporary_root));
            assert!(
                self.0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("fiko_workspace_patch_")
            );
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn edit(path: &str, old_text: &str, new_text: &str) -> FileChange {
        FileChange::Edit {
            path: path.into(),
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }

    fn create(path: &str, content: &str) -> FileChange {
        FileChange::Create {
            path: path.into(),
            content: content.into(),
        }
    }

    fn edit_a() -> FileChange {
        edit(
            "src/a.rs",
            "pub fn a() -> u8 { 1 }",
            "pub fn a() -> u8 { 3 }",
        )
    }

    fn line_ending_options(source: &str) -> tools::PatchMatchOptions {
        use sha2::Digest;
        tools::PatchMatchOptions {
            mode: tools::PatchMatchMode::LineEndings,
            expected_sha256: Some(format!("{:x}", sha2::Sha256::digest(source.as_bytes()))),
        }
    }

    #[test]
    fn optin_mixed_eol_ve_yeni_dosya_somut_bytelarla_uygulanip_geri_alinmali() {
        let project = Project::new();
        let original = "// prefix\nfn a() {\r\n    1\n}\r\n// tail\r\n";
        let old_text = "fn a() {\n    1\n}\n";
        let new_text = "fn a() {\r\n    3\r\n}\n";
        fs::write(project.0.join("src/a.rs"), original).unwrap();
        let changes = [
            edit("src/a.rs", old_text, new_text),
            create("src/c.rs", "pub fn c() {}\r\n"),
        ];
        assert!(prepare(&project.0, &changes).is_err());
        let plan = prepare_with_options(
            &project.0,
            &changes,
            &[
                line_ending_options(original),
                tools::PatchMatchOptions::default(),
            ],
        )
        .unwrap();
        assert_eq!(plan.files[0].original.as_deref(), Some(original));
        assert_eq!(
            plan.files[0].updated,
            format!("// prefix\n{new_text}// tail\r\n")
        );
        assert_eq!(plan.files[1].updated, "pub fn c() {}\r\n");
        assert_eq!(
            fs::read(project.0.join("src/a.rs")).unwrap(),
            original.as_bytes()
        );
        assert!(!project.0.join("src/c.rs").exists());
        assert!(!project.0.join(".fiko_backups").exists());
        let record = apply(&project.0, &plan).unwrap();
        assert_eq!(
            fs::read(project.0.join("src/a.rs")).unwrap(),
            plan.files[0].updated.as_bytes()
        );
        assert_eq!(
            fs::read(project.0.join("src/c.rs")).unwrap(),
            b"pub fn c() {}\r\n"
        );
        rollback(&project.0, &record).unwrap();
        assert_eq!(
            fs::read(project.0.join("src/a.rs")).unwrap(),
            original.as_bytes()
        );
        assert!(!project.0.join("src/c.rs").exists());
    }

    #[test]
    fn optin_gecersiz_ikinci_dosya_ve_eski_sha_tum_plani_yazmadan_reddetmeli() {
        let project = Project::new();
        let original = "fn a() {\r\n    1\n}\r\n";
        let second = read_text(&project.0.join("src/b.rs")).unwrap();
        fs::write(project.0.join("src/a.rs"), original).unwrap();
        let error = prepare_with_options(
            &project.0,
            &[
                edit("src/a.rs", "fn a() {\n    1\n}\n", "fn changed() {}\n"),
                edit("src/b.rs", "fn absent() {}", "fn replacement() {}"),
            ],
            &[line_ending_options(original), line_ending_options(&second)],
        )
        .unwrap_err();
        let diagnostic: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic["change_index"], 1);
        assert_eq!(diagnostic["field"], "old_text");
        assert_eq!(
            fs::read(project.0.join("src/a.rs")).unwrap(),
            original.as_bytes()
        );
        assert_eq!(read_text(&project.0.join("src/b.rs")).unwrap(), second);
        assert!(!project.0.join(".fiko_backups").exists());
        let stale_options = line_ending_options(original);
        let drifted = format!("{original}// user drift\n");
        fs::write(project.0.join("src/a.rs"), &drifted).unwrap();
        let error = prepare_with_options(
            &project.0,
            &[edit(
                "src/a.rs",
                "fn a() {\n    1\n}\n",
                "fn changed() {}\n",
            )],
            &[stale_options],
        )
        .unwrap_err();
        let diagnostic: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic["change_index"], 0);
        assert_eq!(diagnostic["field"], "expected_sha256");
        assert_eq!(
            fs::read(project.0.join("src/a.rs")).unwrap(),
            drifted.as_bytes()
        );
        assert!(!project.0.join(".fiko_backups").exists());
    }

    #[test]
    fn workspace_eslesme_secenekleri_islemlerle_birebir_ve_create_icin_default_olmali() {
        let project = Project::new();
        assert!(prepare_with_options(&project.0, &[edit_a()], &[]).is_err());
        assert!(
            prepare_with_options(
                &project.0,
                &[create("src/c.rs", "new")],
                &[line_ending_options("unused")]
            )
            .is_err()
        );
        assert!(!project.0.join("src/c.rs").exists());
        assert!(!project.0.join(".fiko_backups").exists());
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }

    #[test]
    fn tek_ve_cok_dosyali_prepare_ayni_patch_match_tanisini_yazmadan_vermeli() {
        let project = Project::new();
        let cases = [
            ("fn demo() {}\n", "fn absent() {}", "not_found"),
            ("    let value = 1;\n", "let value = 1", "partial_line"),
            ("fn demo() {}\n", "fn demo() {", "partial_line"),
            ("fn demo() {}\nfn demo() {}\n", "fn demo() {}", "ambiguous"),
            (
                "fn a() {}\r\nfn b() {}\r\n",
                "fn a() {}\nfn b() {}\n",
                "line_endings_mismatch",
            ),
            (
                "fn a() {}\nfn b() {}\n",
                "fn a() {}\r\nfn b() {}\r\n",
                "line_endings_mismatch",
            ),
            (
                "fn a() {}\nfn b() {}\n",
                "fn a() {}\nfn b()",
                "partial_line",
            ),
        ];
        for (content, old_text, reason) in cases {
            fs::write(project.0.join("src/a.rs"), content).unwrap();
            let single =
                tools::prepare_patch(&project.0, "src/a.rs", old_text, "fn replacement() {}")
                    .err()
                    .unwrap();
            let multi = prepare(
                &project.0,
                &[edit("src/a.rs", old_text, "fn replacement() {}")],
            )
            .err()
            .unwrap();
            let diagnostic: serde_json::Value = serde_json::from_str(&single).unwrap();
            let mut workspace_diagnostic: serde_json::Value = serde_json::from_str(&multi).unwrap();
            assert_eq!(workspace_diagnostic["change_index"], 0);
            workspace_diagnostic
                .as_object_mut()
                .unwrap()
                .remove("change_index");
            assert_eq!(diagnostic, workspace_diagnostic);
            assert!(diagnostic.get("change_index").is_none());
            assert_eq!(diagnostic["error"], "PATCH_MATCH");
            assert_eq!(diagnostic["reason"], reason);
            assert_eq!(
                fs::read(project.0.join("src/a.rs")).unwrap(),
                content.as_bytes()
            );
            assert!(!project.0.join(".fiko_backups").exists());
        }
    }

    #[test]
    fn tek_ve_cok_dosyali_prepare_ayni_unique_anchor_ve_byte_bicimini_korumali() {
        let project = Project::new();
        for ending in ["\n", "\r\n"] {
            let content = format!("fn demo() {{{ending}    let value = 1;{ending}}}{ending}");
            fs::write(project.0.join("src/a.rs"), &content).unwrap();
            let old_text = "let value = 1;";
            let new_text = format!("let value = 2;{ending}    let appended = 3;");
            let single = tools::prepare_patch(&project.0, "src/a.rs", old_text, &new_text).unwrap();
            let multi = prepare(&project.0, &[edit("src/a.rs", old_text, &new_text)]).unwrap();
            assert_eq!(single.original, content);
            assert_eq!(multi.files[0].original.as_ref(), Some(&single.original));
            assert_eq!(multi.files[0].updated, single.updated);
            assert_eq!(single.updated, content.replace(old_text, &new_text));
            assert_eq!(
                fs::read(project.0.join("src/a.rs")).unwrap(),
                content.as_bytes()
            );
            assert!(!project.0.join(".fiko_backups").exists());
        }
    }

    #[test]
    fn iki_dosya_ve_yeni_dosya_birlikte_uygulanip_geri_alinmali() {
        let project = Project::new();
        let plan = prepare(
            &project.0,
            &[
                create("src/c.rs", "pub fn c() {}\n"),
                edit(
                    "src/b.rs",
                    "pub fn b() -> u8 { 2 }",
                    "pub fn b() -> u8 { 4 }",
                ),
                edit_a(),
            ],
        )
        .unwrap();
        assert_eq!(plan.files[0].relative, "src/a.rs");
        assert_eq!(plan.files[2].original, None);
        let diff = plan.diff_summary();
        assert!(diff.contains("-pub fn a() -> u8 { 1 }"));
        assert!(diff.contains("+pub fn a() -> u8 { 3 }"));
        assert!(diff.contains("--- /dev/null\n+++ src/c.rs"));
        let record = apply(&project.0, &plan).unwrap();
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 3 }\n"
        );
        assert_eq!(
            read_text(&project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 4 }\n"
        );
        assert_eq!(
            read_text(&project.0.join("src/c.rs")).unwrap(),
            "pub fn c() {}\n"
        );
        rollback(&project.0, &record).unwrap();
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert_eq!(
            read_text(&project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 2 }\n"
        );
        assert!(!project.0.join("src/c.rs").exists());
    }

    #[test]
    fn gecersiz_ikinci_dosya_tum_plani_yazmadan_reddetmeli() {
        let project = Project::new();
        let before = read_text(&project.0.join("src/a.rs")).unwrap();
        let error = prepare(&project.0, &[edit_a(), edit("src/b.rs", "yok", "baska")])
            .err()
            .unwrap();
        let diagnostic: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic["error"], "PATCH_MATCH");
        assert_eq!(diagnostic["field"], "old_text");
        assert_eq!(diagnostic["change_index"], 1);
        assert_eq!(diagnostic["reason"], "not_found");
        assert!(!error.contains("src/b.rs"));
        assert!(!error.contains("yok"));
        assert_eq!(read_text(&project.0.join("src/a.rs")).unwrap(), before);
        assert!(!project.0.join(".fiko_backups").exists());
        assert!(prepare(&project.0, &[create("src/a.rs", "ezme")]).is_err());
        assert!(prepare(&project.0, &[create("yeni/dosya.rs", "x")]).is_err());
    }

    #[test]
    fn ayni_hedefe_cakisan_islemler_ve_traversal_reddedilmeli() {
        let project = Project::new();
        assert!(
            prepare(
                &project.0,
                &[
                    edit_a(),
                    edit("./src/a.rs", "pub fn a() -> u8 { 1 }", "pub fn a() {}")
                ]
            )
            .is_err()
        );
        assert!(
            prepare(
                &project.0,
                &[create("src/c.rs", "a"), create("./src/c.rs", "b")]
            )
            .is_err()
        );
        assert!(prepare(&project.0, &[create("../disari.rs", "a")]).is_err());
        assert!(prepare(&project.0, &[create("src/main_v05_backup.txt", "a")]).is_err());
        assert!(!project.0.join(".fiko_backups").exists());
    }

    #[test]
    fn degisen_hedef_ve_yeni_dosya_yarisi_eski_plani_gecersiz_kilmali() {
        let project = Project::new();
        let plan = prepare(&project.0, &[edit_a(), create("src/c.rs", "yeni")]).unwrap();
        fs::write(project.0.join("src/a.rs"), "kullanicinin degisikligi\n").unwrap();
        let error = apply(&project.0, &plan).unwrap_err();
        assert!(error.restored);
        assert!(error.record.is_none());
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "kullanicinin degisikligi\n"
        );
        assert!(!project.0.join("src/c.rs").exists());
        let plan = prepare(&project.0, &[create("src/c.rs", "yeni")]).unwrap();
        fs::write(project.0.join("src/c.rs"), "kullanicinin yeni dosyasi").unwrap();
        assert!(revalidate(&project.0, &plan).is_err());
        assert!(apply(&project.0, &plan).is_err());
        assert_eq!(
            read_text(&project.0.join("src/c.rs")).unwrap(),
            "kullanicinin yeni dosyasi"
        );
    }

    #[test]
    fn ucuncu_yazma_hatasi_onceki_iki_editi_geri_almali_ve_kaydi_kapatmali() {
        let project = Project::new();
        let plan = prepare(
            &project.0,
            &[
                edit_a(),
                edit(
                    "src/b.rs",
                    "pub fn b() -> u8 { 2 }",
                    "pub fn b() -> u8 { 4 }",
                ),
                create("src/c.rs", "yeni\n"),
            ],
        )
        .unwrap();
        let error = apply_with(&project.0, &plan, |index, _| {
            if index == 2 {
                Err("kontrollu ucuncu yazma hatasi".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert!(error.restored, "{error}");
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert_eq!(
            read_text(&project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 2 }\n"
        );
        assert!(!project.0.join("src/c.rs").exists());
        assert!(
            !project
                .0
                .join(".fiko_backups/workspace.pending.json")
                .exists()
        );
        // The recovered workspace must be writable by a newly authorized plan.
        let subsequent = prepare(&project.0, &[edit_a()]).unwrap();
        let record = apply(&project.0, &subsequent).unwrap();
        rollback(&project.0, &record).unwrap();
    }

    #[test]
    fn ucuncu_yazmada_dis_degisiklik_kalirsa_kayit_ve_durumu_korumali() {
        let project = Project::new();
        let plan = prepare(
            &project.0,
            &[
                edit_a(),
                edit(
                    "src/b.rs",
                    "pub fn b() -> u8 { 2 }",
                    "pub fn b() -> u8 { 4 }",
                ),
                create("src/c.rs", "yeni\n"),
            ],
        )
        .unwrap();
        let error = apply_with(&project.0, &plan, |index, _| {
            if index == 2 {
                fs::write(project.0.join("src/a.rs"), "kullanicinin dis degisikligi\n").unwrap();
                Err("kontrollu ucuncu yazma hatasi".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert!(!error.restored);
        assert!(error.message.contains("BELIRSIZ"));
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "kullanicinin dis degisikligi\n"
        );
        assert_eq!(
            read_text(&project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 2 }\n"
        );
        assert!(!project.0.join("src/c.rs").exists());
        assert!(project.0.join(".fiko_backups/workspace.pending.json").is_file());
        let audit = workspace_recovery::audit(&project.0).unwrap();
        assert!(audit.contains("src/a.rs\": unexpected"), "{audit}");
        assert!(audit.contains("src/b.rs\": original"), "{audit}");
        let next = prepare(&project.0, &[create("src/next.rs", "new")]).unwrap();
        let blocked = apply(&project.0, &next).unwrap_err();
        assert!(blocked.message.contains("belirsiz"), "{blocked}");
        assert!(!project.0.join("src/next.rs").exists());
    }

    #[test]
    fn ikinci_yazma_hatasinda_onceki_edit_ve_create_geri_alinmali() {
        for initial in [edit_a(), create("src/aa.rs", "yeni\n")] {
            let project = Project::new();
            let plan = prepare(&project.0, &[initial, create("src/z.rs", "son\n")]).unwrap();
            let error = apply_with(&project.0, &plan, |index, _| {
                if index == 1 {
                    Err("kontrollu ikinci yazma hatasi".into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(error.restored, "{error}");
            assert!(error.message.contains("Tum hedef yazimlari geri alindi"));
            assert_eq!(
                read_text(&project.0.join("src/a.rs")).unwrap(),
                "pub fn a() -> u8 { 1 }\n"
            );
            assert!(!project.0.join("src/aa.rs").exists());
            assert!(!project.0.join("src/z.rs").exists());
        }
    }

    #[test]
    fn bozuk_yedek_diger_hedeflerin_geri_alinmasini_engellememeli() {
        let project = Project::new();
        let plan = prepare(
            &project.0,
            &[
                edit_a(),
                edit(
                    "src/b.rs",
                    "pub fn b() -> u8 { 2 }",
                    "pub fn b() -> u8 { 4 }",
                ),
                create("src/c.rs", "yeni"),
            ],
        )
        .unwrap();
        let record = apply(&project.0, &plan).unwrap();
        fs::write(record.files[1].backup_path.as_ref().unwrap(), "bozuk yedek").unwrap();
        let error = rollback(&project.0, &record).unwrap_err();
        assert!(error.contains("src/b.rs"));
        assert!(error.contains("eslesmiyor"));
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
        assert_eq!(
            read_text(&project.0.join("src/b.rs")).unwrap(),
            "pub fn b() -> u8 { 4 }\n"
        );
        assert!(!project.0.join("src/c.rs").exists());
    }

    #[test]
    fn uygulama_hatasi_ve_eksik_rollback_belirsiz_olarak_donmeli() {
        let project = Project::new();
        let plan = prepare(&project.0, &[edit_a(), create("src/z.rs", "son\n")]).unwrap();
        let error = apply_with(&project.0, &plan, |index, _| {
            if index == 1 {
                fs::write(project.0.join("src/a.rs"), "harici degisiklik").unwrap();
                Err("kontrollu ikinci yazma hatasi".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(!error.restored);
        assert!(error.record.is_some());
        assert!(error.message.contains("BELIRSIZ"));
        assert!(error.message.contains("src/a.rs"));
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "harici degisiklik"
        );
        assert!(!project.0.join("src/z.rs").exists());
    }

    #[test]
    fn dis_degisiklikler_rollback_ile_ezilmemeli() {
        let project = Project::new();
        let plan = prepare(&project.0, &[edit_a(), create("src/c.rs", "yeni")]).unwrap();
        let record = apply(&project.0, &plan).unwrap();
        fs::write(project.0.join("src/a.rs"), "kullanici editi").unwrap();
        fs::write(project.0.join("src/c.rs"), "kullanicinin yeni icerigi").unwrap();
        let error = rollback(&project.0, &record).unwrap_err();
        assert!(error.contains("src/a.rs"));
        assert!(error.contains("src/c.rs"));
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "kullanici editi"
        );
        assert_eq!(
            read_text(&project.0.join("src/c.rs")).unwrap(),
            "kullanicinin yeni icerigi"
        );
    }

    #[test]
    fn kismi_yazilan_byte_prefixi_geri_alinabilmeli() {
        let project = Project::new();
        let plan = prepare(&project.0, &[edit_a()]).unwrap();
        let backup = tools::save_backup(
            &project.0,
            &plan.files[0].path,
            plan.files[0].original.as_ref().unwrap(),
        )
        .unwrap();
        let partial = vec![b'p', b'u', 0xc4];
        fs::write(&plan.files[0].path, &partial).unwrap();
        let record = WorkspaceRecord {
            files: vec![RecordedFile {
                file: plan.files[0].clone(),
                backup_path: Some(backup),
                written: Some(partial),
            }],
        };
        rollback(&project.0, &record).unwrap();
        assert_eq!(
            read_text(&project.0.join("src/a.rs")).unwrap(),
            "pub fn a() -> u8 { 1 }\n"
        );
    }

    #[test]
    fn sahte_public_plan_alani_kok_disina_yazamamali() {
        let project = Project::new();
        let mut plan = prepare(&project.0, &[edit_a()]).unwrap();
        plan.files[0].path = project.0.join("../disari.rs");
        assert!(revalidate(&project.0, &plan).is_err());
        assert!(apply(&project.0, &plan).is_err());
        assert!(!project.0.join(".fiko_backups").exists());
    }

    #[test]
    fn bos_yeni_dosyanin_diffi_ve_yokluk_geri_almasi_acik_olmali() {
        let project = Project::new();
        let plan = prepare(&project.0, &[create("src/empty.rs", "")]).unwrap();
        assert!(
            plan.diff_summary()
                .contains("--- /dev/null\n+++ src/empty.rs")
        );
        let record = apply(&project.0, &plan).unwrap();
        assert!(project.0.join("src/empty.rs").is_file());
        rollback(&project.0, &record).unwrap();
        assert!(!project.0.join("src/empty.rs").exists());
    }
}
