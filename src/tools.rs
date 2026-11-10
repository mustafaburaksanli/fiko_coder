//! Dosya araclari, cargo calistirma, patch hazirlama, yedekleme ve geri alma.
//!
//! Bu modul v0.6'daki davranisi korur; v0.7 yalnizca `run_cargo` ile
//! hedef dizin / zaman asimi parametrelerini ve `tool_definitions`i ekler.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

pub const MAX_SEARCH_RESULTS: usize = 20;
pub const MAX_SEARCH_FILES: usize = 150;
pub const CARGO_TIMEOUT_SECS: u64 = 120;

// ============================================================
// ORTAK DOSYA YOLU KONTROLU
// ============================================================
pub fn check_relative_path(relative: &str) -> Result<(), String> {
    if relative.trim().is_empty() {
        return Err("Dosya veya klasor yolu bos olamaz.".into());
    }

    for component in Path::new(relative).components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err("Mutlak veya ust klasore cikan yol yasak.".into());
            }
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.starts_with('.') || name == "target" {
                    return Err("Gizli ya da korunan klasor/dosya yasak.".into());
                }
            }
            Component::CurDir => {}
        }
    }
    Ok(())
}

// ============================================================
// TOOL 1: DOSYA OKUMA
// ============================================================
pub fn read_file(root: &Path, relative: &str) -> Result<String, String> {
    check_relative_path(relative)?;
    let full = root
        .join(relative)
        .canonicalize()
        .map_err(|e| format!("Dosya bulunamadi: {e}"))?;

    if !full.starts_with(root) {
        return Err("Proje disina erisim yasak.".into());
    }
    // Sembolik baglanti gizli bir dosyaya gidiyorsa da engelle.
    let resolved = full.strip_prefix(root).map_err(|e| e.to_string())?;
    check_relative_path(&resolved.to_string_lossy())?;
    let metadata = fs::metadata(&full).map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Bu yol bir dosya degil.".into());
    }
    if metadata.len() > 64_000 {
        return Err("Dosya 64 KB sinirini asiyor.".into());
    }
    fs::read_to_string(full).map_err(|e| format!("Okuma hatasi: {e}"))
}

// ============================================================
// TOOL 2: KLASOR LISTELEME
// ============================================================
pub fn list_files(root: &Path, relative: &str) -> Result<String, String> {
    check_relative_path(relative)?;
    let directory = root
        .join(relative)
        .canonicalize()
        .map_err(|e| format!("Klasor bulunamadi: {e}"))?;

    if !directory.starts_with(root) {
        return Err("Proje disina erisim yasak.".into());
    }
    let resolved = directory.strip_prefix(root).map_err(|e| e.to_string())?;
    // Proje kokunde bos goreli yol da guvenlidir.
    if !resolved.as_os_str().is_empty() {
        check_relative_path(&resolved.to_string_lossy())?;
    }
    if !directory.is_dir() {
        return Err("Bu yol bir klasor degil.".into());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let label = if kind.is_dir() { "[DIR]" } else { "[FILE]" };
        files.push(format!("{label} {name}"));
    }
    files.sort();
    files.truncate(100);
    Ok(files.join("\n"))
}

// ============================================================
// TOOL 3: SATIR BAZLI OKUMA
// ============================================================
pub fn read_lines(root: &Path, relative: &str, start: usize, end: usize) -> Result<String, String> {
    if start == 0 || end < start || end - start >= 60 {
        return Err("Satir araligi 1 tabanli olmali ve en fazla 60 satir icermeli.".into());
    }
    let content = read_file(root, relative)?;
    let lines: Vec<&str> = content.lines().collect();
    if start > lines.len() || end > lines.len() {
        return Err(format!(
            "Dosya {} satir; istenen aralik {}-{}.",
            lines.len(),
            start,
            end
        ));
    }
    Ok(lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>4} | {}", start + i, line))
        .collect::<Vec<_>>()
        .join("\n"))
}

// ============================================================
// TOOL 4: KOD ARAMA
// ============================================================
pub fn search_code(root: &Path, query: &str) -> Result<String, String> {
    if query.trim().is_empty() || query.len() > 128 || query.contains('\n') {
        return Err("Arama sorgusu 1-128 byte ve tek satir olmali.".into());
    }
    let src = root.join("src").canonicalize().map_err(|e| e.to_string())?;
    if !src.starts_with(root) {
        return Err("src klasoru proje disina cikiyor.".into());
    }
    let mut pending = vec![(src, 0usize)];
    let mut scanned = 0usize;
    let mut hits = Vec::new();

    while let Some((folder, depth)) = pending.pop() {
        for entry in fs::read_dir(&folder).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                if depth < 5 {
                    pending.push((path, depth + 1));
                }
            } else if kind.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                scanned += 1;
                if scanned > MAX_SEARCH_FILES {
                    return Ok(format!(
                        "{}\n[ARAMA SINIRI] 150 dosyaya ulasildi.",
                        hits.join("\n")
                    ));
                }
                let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
                let rel_str = rel.to_string_lossy();
                let content = match read_file(root, &rel_str) {
                    Ok(text) => text,
                    Err(_) => continue,
                };
                for (i, line) in content.lines().enumerate() {
                    if line.contains(query) {
                        let excerpt: String = line.chars().take(180).collect();
                        hits.push(format!("{}:{}: {}", rel.display(), i + 1, excerpt));
                        if hits.len() == MAX_SEARCH_RESULTS {
                            return Ok(hits.join("\n"));
                        }
                    }
                }
            }
        }
    }
    if hits.is_empty() {
        Ok("Eslesen Rust kodu bulunamadi.".into())
    } else {
        Ok(hits.join("\n"))
    }
}

// ============================================================
// TOOL 5: CARGO CHECK / CARGO TEST
// ============================================================
pub struct TestResult {
    pub success: bool,
    /// Model icin kisaltilmis birlesik rapor (en fazla 10.000 karakter).
    pub report: String,
    pub stdout: String,
    pub stderr: String,
}

/// `cargo test` altinda calisirken ayni cargo ikilisini kullan; yoksa PATH'teki cargo.
pub fn cargo_bin() -> String {
    std::env::var("CARGO")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "cargo".into())
}

/// Belirtilen dizinde `cargo <mode>` calistirir. `target_dir` verilirse
/// CARGO_TARGET_DIR olarak kullanilir (gecici kopya dogrulamasi icin).
pub async fn run_cargo(
    dir: &Path,
    mode: &str,
    target_dir: Option<&Path>,
    limit: Duration,
) -> Result<TestResult, String> {
    if mode != "check" && mode != "test" {
        return Err("Gecersiz komut. Sadece check veya test kullanilabilir.".into());
    }

    let mut command = Command::new(cargo_bin());
    command
        .arg(mode)
        .current_dir(dir)
        .env("CARGO_TERM_COLOR", "never")
        .kill_on_drop(true);
    if let Some(target) = target_dir {
        command.env("CARGO_TARGET_DIR", target);
    }

    let result = timeout(limit, command.output())
        .await
        .map_err(|_| "Cargo komutu zaman asimina ugradi.".to_string())?
        .map_err(|e| format!("Cargo calistirilamadi: {e}"))?;

    let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    let text = format!(
        "Komut: cargo {mode}\nBasarili: {}\nExit status: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
        result.status.success(),
        result.status,
        stdout,
        stderr
    );
    let report: String = text.chars().take(10_000).collect();
    Ok(TestResult {
        success: result.status.success(),
        report,
        stdout,
        stderr,
    })
}

pub async fn run_tests(root: &Path, mode: &str) -> Result<TestResult, String> {
    if mode != "check" && mode != "test" {
        return Err("Gecersiz komut. Sadece check veya test kullanilabilir.".into());
    }
    println!("[RUN] cargo {mode}");
    run_cargo(root, mode, None, Duration::from_secs(CARGO_TIMEOUT_SECS)).await
}

// ============================================================
// TOOL 6: PATCH HAZIRLAMA
// ============================================================
// old_text eksik ';' iceren bir satir alt-dizisi olmamali.
// Eslesme, yalnizca yatay bosluklar haric tam satir/blok kapsadiginda gecerlidir.
pub fn exact_line_block_locations(content: &str, needle: &str) -> Vec<usize> {
    content
        .match_indices(needle)
        .filter_map(|(start, _)| {
            let prefix = content[..start].rsplit('\n').next().unwrap_or("");
            let end = start + needle.len();
            let suffix = content[end..].split('\n').next().unwrap_or("");
            let only_indent = prefix.chars().all(|c| c == ' ' || c == '\t' || c == '\r');
            let only_tail = suffix.chars().all(|c| c == ' ' || c == '\t' || c == '\r');
            if only_indent && only_tail {
                Some(start)
            } else {
                None
            }
        })
        .collect()
}

pub struct PatchPlan {
    /// Gercek projedeki kanonik hedef yol.
    pub path: PathBuf,
    pub original: String,
    pub updated: String,
}

pub fn prepare_patch(
    root: &Path,
    relative: &str,
    old_text: &str,
    new_text: &str,
) -> Result<PatchPlan, String> {
    check_relative_path(relative)?;
    if old_text.is_empty() || new_text.is_empty() {
        return Err("Eski ve yeni kod bos olamaz.".into());
    }
    if old_text.len() > 8_000 || new_text.len() > 8_000 {
        return Err("Degisiklik 8000 byte sinirini asiyor.".into());
    }
    if old_text == new_text {
        return Err("Eski ve yeni kod ayni.".into());
    }

    let src_root = root
        .join("src")
        .canonicalize()
        .map_err(|e| format!("src klasoru bulunamadi: {e}"))?;

    if !src_root.starts_with(root) {
        return Err("root parametresi src klasörünün bir alt dizini olmalı.".into());
    }
    let full = root
        .join(relative)
        .canonicalize()
        .map_err(|e| format!("Dosya bulunamadi: {e}"))?;

    if !full.starts_with(&src_root) {
        return Err("Sadece src klasorundeki dosyalar duzenlenebilir.".into());
    }
    if full.extension().and_then(|e| e.to_str()) != Some("rs") {
        return Err("Sadece .rs dosyalari duzenlenebilir.".into());
    }
    if !full.is_file() {
        return Err("Hedef bir dosya degil.".into());
    }

    let original = fs::read_to_string(&full).map_err(|e| e.to_string())?;
    if original.len() > 128_000 {
        return Err("Dosya 128 KB sinirini asiyor.".into());
    }
    let matches = exact_line_block_locations(&original, old_text);
    if matches.len() != 1 {
        return Err("Eski metin tam bir satir/blok olarak dosyada tam bir kez bulunmali. Girintiyi atlayabilirsiniz; satir sonundaki ';' gibi karakterleri atlayamazsiniz.".into());
    }

    let pos = matches[0];
    let mut updated = original.clone();
    updated.replace_range(pos..pos + old_text.len(), new_text);
    if updated.len() > 128_000 {
        return Err("Yeni dosya 128 KB sinirini asiyor.".into());
    }
    Ok(PatchPlan {
        path: full,
        original,
        updated,
    })
}

// ============================================================
// YEDEKLEME VE GERI ALMA
// ============================================================
pub struct PatchRecord {
    pub path: PathBuf,
    pub original: String,
    pub updated: String,
    pub backup_path: PathBuf,
}

pub fn save_backup(root: &Path, file: &Path, original: &str) -> Result<PathBuf, String> {
    let backup_dir = root.join(".fiko_backups");
    if backup_dir.exists() {
        let metadata = fs::symlink_metadata(&backup_dir).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Yedek klasoru guvenli bir dizin degil.".into());
        }
    } else {
        fs::create_dir(&backup_dir).map_err(|e| format!("Yedek klasoru olusturulamadi: {e}"))?;
    }

    let real_dir = backup_dir.canonicalize().map_err(|e| e.to_string())?;
    if !real_dir.starts_with(root) {
        return Err("Yedek klasoru proje disina cikiyor.".into());
    }

    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let base = file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("kaynak.rs");
    let backup_path = real_dir.join(format!("{tick}_{}_{}.bak", std::process::id(), base));

    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup_path)
        .map_err(|e| format!("Yedek dosyasi acilamadi: {e}"))?;
    backup
        .write_all(original.as_bytes())
        .map_err(|e| format!("Yedek yazilamadi: {e}"))?;
    // sync_all diski zorlar ama elektrik kesintisine karsi mutlak garanti degildir.
    backup
        .sync_all()
        .map_err(|e| format!("Yedek kaydedilemedi: {e}"))?;
    Ok(backup_path)
}

/// Hedef dosyaya yazim atomik degildir (Windows'ta rename/kilit davranislari
/// farklidir); kurtarma mekanizmasi yedek dosyasi ve geri okuma kontroludur.
pub fn apply_approved_patch(root: &Path, plan: PatchPlan) -> Result<PatchRecord, String> {
    let current = fs::read_to_string(&plan.path).map_err(|e| e.to_string())?;
    if current != plan.original {
        return Err("Dosya onay beklenirken degisti; patch iptal edildi.".into());
    }

    let backup_path = save_backup(root, &plan.path, &plan.original)?;
    fs::write(&plan.path, &plan.updated).map_err(|e| {
        format!(
            "Yazma hatasi: {e}. Orijinal yedek burada: {}",
            backup_path.display()
        )
    })?;

    let check = fs::read_to_string(&plan.path).map_err(|e| e.to_string())?;
    if check != plan.updated {
        return Err(format!(
            "Dosyaya yazilan icerik dogrulanamadi. Yedek: {}",
            backup_path.display()
        ));
    }
    Ok(PatchRecord {
        path: plan.path,
        original: plan.original,
        updated: plan.updated,
        backup_path,
    })
}

pub fn rollback_patch(record: &PatchRecord) -> Result<(), String> {
    let current = fs::read_to_string(&record.path).map_err(|e| e.to_string())?;
    if current != record.updated {
        return Err("Dosya patch sonrasinda degismis; otomatik geri alma reddedildi.".into());
    }
    let backup =
        fs::read_to_string(&record.backup_path).map_err(|e| format!("Yedek okunamadi: {e}"))?;
    if backup != record.original {
        return Err("Yedek icerigi beklenen orijinal kodla eslesmiyor.".into());
    }
    fs::write(&record.path, backup).map_err(|e| format!("Geri alma yazma hatasi: {e}"))?;
    let restored = fs::read_to_string(&record.path).map_err(|e| e.to_string())?;
    if restored != record.original {
        return Err("Geri alinan dosya dogrulanamadi.".into());
    }
    Ok(())
}

/// Her iki Cargo adimi da onaylanmis bir patch sonrasinda GERCEK projede calisir.
pub async fn verify_patch(root: &Path) -> (bool, String) {
    let check = match run_tests(root, "check").await {
        Ok(result) => result,
        Err(err) => return (false, format!("cargo check baslatilamadi: {err}")),
    };
    if !check.success {
        return (false, check.report);
    }

    let tests = match run_tests(root, "test").await {
        Ok(result) => result,
        Err(err) => {
            return (
                false,
                format!("cargo check gecti, cargo test hatasi: {err}"),
            );
        }
    };
    let report = format!("{}\n\n{}", check.report, tests.report);
    (tests.success, report)
}

// ============================================================
// OLLAMA ARAC TANIMLARI
// ============================================================
pub fn tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Proje icindeki izinli bir dosyayi oku. 64KB siniri vardir.",
                "parameters": {
                    "type": "object", "properties": {
                        "path": {"type": "string", "description": "Ornek: src/main.rs"}
                    }, "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "Projedeki klasor ve dosyalari listele.",
                "parameters": {
                    "type": "object", "properties": {
                        "path": {"type": "string", "description": "Proje koku icin . kullan."}
                    }, "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_lines",
                "description": "Bir dosyanin 1 tabanli satirlarini oku, en fazla 60 satir. Satir numaralari kodun parcasi degildir.",
                "parameters": {
                    "type": "object", "properties": {
                        "path": {"type": "string"},
                        "start_line": {"type": "integer", "minimum": 1},
                        "end_line": {"type": "integer", "minimum": 1}
                    }, "required": ["path", "start_line", "end_line"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "src altindaki .rs dosyalarinda sabit bir ifadeyi ara. Maksimum 20 eslesme.",
                "parameters": {
                    "type": "object", "properties": {
                        "query": {"type": "string", "description": "Ornek: async fn main"}
                    }, "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_tests",
                "description": "Kullanici onayiyla GERCEK projede cargo check veya cargo test calistir. Bir patch adayini denemek icin bunu degil verify_candidate kullan.",
                "parameters": {
                    "type": "object", "properties": {
                        "mode": {"type": "string", "enum": ["check", "test"]}
                    }, "required": ["mode"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "verify_candidate",
                "description": "Onerilen patch'i ANA PROJEYE DOKUNMADAN gecici bir kopyada derle. Varsayilan mode=check (cargo check); mode=test ile cargo test de calisir (ayri kullanici izni gerekir). Ana dosya degismez. Sonuc VERIFY_OK ya da derleyici hatalarini iceren VERIFY_FAILED olur. VERIFY_OK sonrasi ayni path/old_text/new_text ile apply_patch cagir. En fazla 3 dogrulama denemesi vardir; ayni basarisiz patch tekrar denenemez.",
                "parameters": {
                    "type": "object", "properties": {
                        "path": {"type": "string", "description": "Ornek: src/main.rs"},
                        "old_text": {"type": "string", "description": "Satirin/blokun tam eski metni (girinti disinda hicbir karakter atlama)"},
                        "new_text": {"type": "string", "description": "Yerine konacak tam yeni metin"},
                        "mode": {"type": "string", "enum": ["check", "test"], "description": "Varsayilan check"}
                    }, "required": ["path", "old_text", "new_text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Rust dosyasinda yalnizca TAM SATIR veya TAM SATIRLARDAN olusan kodu degistir. old_text satir sonundaki noktalama karakterlerini de icermeli; satir basi bosluklari istege bagli. Once verify_candidate ile dogrula. Dogrulanmamis aday once gecici kopyada derlenir; ardindan kullanici onayi, yedekleme, gercek projede cargo check + cargo test; hatada geri alma.",
                "parameters": {
                    "type": "object", "properties": {
                        "path": {"type": "string"},
                        "old_text": {"type": "string", "description": "Satirin/blokun tam eski metni (girinti disinda hicbir karakter atlama)"},
                        "new_text": {"type": "string", "description": "Yerine konacak tam yeni metin"}
                    }, "required": ["path", "old_text", "new_text"]
                }
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn izinsiz_komut_engellenmeli() {
        assert!(run_tests(Path::new("."), "delete").await.is_err());
    }

    #[test]
    fn bos_patch_reddedilmeli() {
        assert!(prepare_patch(Path::new("."), "src/main.rs", "", "yeni").is_err());
    }

    #[test]
    fn mutlak_patch_yolu_reddedilmeli() {
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        let path = root.join("src/main.rs");
        assert!(prepare_patch(&root, &path.to_string_lossy(), "eski", "yeni").is_err());
    }

    #[test]
    fn gizli_dosya_okuma_engellenmeli() {
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(read_file(&root, ".env").is_err());
        assert!(read_file(&root, ".fiko_backups/backup.bak").is_err());
    }

    #[test]
    fn kopya_eski_metin_reddedilmeli() {
        let root = std::env::temp_dir().join(format!(
            "fiko_patch_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/test.rs"), "eski\neski\n").unwrap();
        let root = root.canonicalize().unwrap();
        assert!(prepare_patch(&root, "src/test.rs", "eski", "yeni").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_yedeklenip_geri_alinabilmeli() {
        let root = std::env::temp_dir().join(format!(
            "fiko_rollback_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/test.rs"), "fn old() {}\n").unwrap();
        let root = root.canonicalize().unwrap();
        let plan = prepare_patch(&root, "src/test.rs", "fn old() {}", "fn new() {}").unwrap();
        let record = apply_approved_patch(&root, plan).unwrap();
        assert_eq!(fs::read_to_string(&record.path).unwrap(), "fn new() {}\n");
        assert!(record.backup_path.is_file());
        rollback_patch(&record).unwrap();
        assert_eq!(fs::read_to_string(&record.path).unwrap(), "fn old() {}\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kismi_satir_patch_reddedilmeli() {
        let content = "    println!(\"FIKO\");\n";
        assert!(exact_line_block_locations(content, "println!(\"FIKO\")").is_empty());
        assert_eq!(
            exact_line_block_locations(content, "println!(\"FIKO\");"),
            vec![4]
        );
    }

    #[test]
    fn tam_satir_patch_girinti_olmadan_kabul_edilmeli() {
        let content = "fn main() {\n    println!(\"FIKO\");\n}\n";
        assert_eq!(
            exact_line_block_locations(content, "println!(\"FIKO\");"),
            vec![16]
        );
    }

    #[test]
    fn satir_okuma_ve_kod_arama_calismali() {
        let folder = std::env::temp_dir().join(format!(
            "fiko_search_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(folder.join("src")).unwrap();
        fs::write(folder.join("src/test.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        let root = folder.canonicalize().unwrap();
        assert!(
            read_lines(&root, "src/test.rs", 2, 2)
                .unwrap()
                .contains("2 | fn beta()")
        );
        assert!(search_code(&root, "beta").unwrap().contains("test.rs:2:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_lines_satir_siniri_testi() {
        let folder = std::env::temp_dir().join(format!(
            "fiko_readlines_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(folder.join("src")).unwrap();
        // En az 61 satirli dosya olustur
        let mut content = String::new();
        for i in 1..=65 {
            content.push_str(&format!("    println!(\"satir {}\", {});\n", i, i));
        }
        fs::write(folder.join("src/test.rs"), content).unwrap();
        let root = folder.canonicalize().unwrap();

        // 1-1 araligi: 1 satir dondurmeli
        let result_1_1 = read_lines(&root, "src/test.rs", 1, 1).unwrap();
        assert!(result_1_1.contains("1 |     println!(\"satir 1\", 1);"));
        assert_eq!(result_1_1.lines().count(), 1);

        // 1-60 araligi: 60 satir dondurmeli
        let result_1_60 = read_lines(&root, "src/test.rs", 1, 60).unwrap();
        assert!(result_1_60.contains("60 |     println!(\"satir 60\", 60);"));
        assert_eq!(result_1_60.lines().count(), 60);

        // 1-61 araligi: reddedilmeli (61 satir siniri asiyor)
        let err_1_61 = read_lines(&root, "src/test.rs", 1, 61).unwrap_err();
        assert!(err_1_61.contains("Satir araligi"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_eski_satirin_tamamini_istemeli() {
        let folder = std::env::temp_dir().join(format!(
            "fiko_exact_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(folder.join("src")).unwrap();
        fs::write(
            folder.join("src/test.rs"),
            "fn main() {\n    println!(\"FIKO\");\n}\n",
        )
        .unwrap();
        let root = folder.canonicalize().unwrap();
        assert!(prepare_patch(&root, "src/test.rs", "println!(\"FIKO\")", "yeni").is_err());
        assert!(
            prepare_patch(
                &root,
                "src/test.rs",
                "println!(\"FIKO\");",
                "println!(\"YENI\");"
            )
            .is_ok()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
