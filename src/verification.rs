//! v0.7 Code Verification Engine.
//!
//! Bir patch adayini ana projeye dokunmadan gecici bir kopyada derler
//! (`cargo check`, istege bagli `cargo test`) ve derleyici ciktisini
//! model icin yapilandirilmis bir rapora cevirir.
//!
//! GUVENLIK NOTU: cargo check / cargo test, build script ve proc-macro gibi
//! proje kodunu calistirir. Burada gercek bir islem izolasyonu (sandbox)
//! YOKTUR; yalnizca dosya duzeyinde ayrim vardir. Bu yuzden gecici kopyada
//! cargo calistirmak kullanici iznine baglidir (bkz. agent::VerifyConsent).

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::tools::{PatchPlan, TestResult, check_relative_path, run_cargo};

/// Ilk dogrulama bagimliliklari sifirdan derleyebilir; bu yuzden run_tests'ten uzun.
pub const VERIFY_TIMEOUT_SECS: u64 = 300;
pub const MAX_COPY_FILES: usize = 2_000;
pub const MAX_COPY_BYTES: u64 = 32 * 1024 * 1024;
/// Gecici kopyanin derleme ciktilari ana projenin target/ klasoru altinda,
/// ana derleme ciktilarindan ayri bir alt klasorde tutulur (bagimliliklar bir kez derlenir).
pub const VERIFY_TARGET_SUBDIR: &str = "fiko_verify";

const MAX_DIAGNOSTICS: usize = 8;
const MAX_DIAGNOSTIC_CHARS: usize = 1_200;
const MAX_REPORT_CHARS: usize = 6_000;
const CLEANUP_DELAYS_MS: [u64; 5] = [50, 100, 200, 400, 800];

// ============================================================
// GECICI CALISMA ALANI
// ============================================================
/// Bu surecte su an kullanimda olan calisma alanlari. Ayni proje icin es zamanli
/// ikinci bir dogrulama, canli bir alani asla silmez; yedek ada duser.
static LIVE_WORKSPACES: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn live_workspaces() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    LIVE_WORKSPACES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sistem gecici klasorunde benzersiz adli bir calisma alani.
/// `cleanup` basarisiz olursa `Drop` tek seferlik son bir deneme yapar.
pub struct TempWorkspace {
    path: PathBuf,
    cleaned: bool,
}

impl TempWorkspace {
    pub fn create(root: &Path) -> Result<Self, String> {
        let base = std::env::temp_dir();
        if !base.is_dir() {
            return Err(format!(
                "Sistem gecici klasoru bulunamadi: {}",
                base.display()
            ));
        }
        let mut hasher = DefaultHasher::new();
        root.hash(&mut hasher);
        let tag = format!(
            "fiko_verify_{:016x}_{}",
            hasher.finish(),
            std::process::id()
        );

        // Birincil ad: proje + pid. Ayni adli bir artik, bu surecte canli degilse
        // ya eski bir basarisiz temizliktir ya da olmus bir surecten kalmistir;
        // her iki durumda da guvenle silinebilir. Kayit, dosya islemlerinden once
        // kilit altinda yapilir; boylece iki is parcacigi ayni yolu paylasamaz.
        let primary = base.join(&tag);
        let claimed_primary = live_workspaces().insert(primary.clone());
        if claimed_primary {
            if fs::symlink_metadata(&primary).is_ok() {
                let _ = fs::remove_dir_all(&primary);
            }
            if fs::create_dir(&primary).is_ok() {
                return Ok(Self {
                    path: primary,
                    cleaned: false,
                });
            }
            live_workspaces().remove(&primary);
        }

        // Yedek ad: nanosaniye damgasi + surec ici sayac ile benzersizlik.
        let mut last_error = String::new();
        for _ in 0..5 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos();
            let seq = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let fallback = base.join(format!("{tag}_{nanos}_{seq}"));
            if !live_workspaces().insert(fallback.clone()) {
                continue;
            }
            match fs::create_dir(&fallback) {
                Ok(()) => {
                    return Ok(Self {
                        path: fallback,
                        cleaned: false,
                    });
                }
                Err(e) => {
                    live_workspaces().remove(&fallback);
                    last_error = e.to_string();
                }
            }
        }
        Err(format!("Gecici calisma alani olusturulamadi: {last_error}"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Windows'ta cargo/rustc cikisindan hemen sonra dosya tutamaclari kisa sure
    /// acik kalabilir; bu yuzden birkac kez artan beklemeyle denenir.
    pub async fn cleanup(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        let mut last_error = String::new();
        for delay in CLEANUP_DELAYS_MS {
            match fs::remove_dir_all(&self.path) {
                Ok(()) => {
                    self.mark_cleaned();
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.mark_cleaned();
                    return Ok(());
                }
                Err(e) => {
                    last_error = e.to_string();
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
        Err(format!("{} silinemedi: {last_error}", self.path.display()))
    }

    fn mark_cleaned(&mut self) {
        self.cleaned = true;
        live_workspaces().remove(&self.path);
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = fs::remove_dir_all(&self.path);
        }
        live_workspaces().remove(&self.path);
    }
}

// ============================================================
// PROJE KOPYALAMA VE PARMAK IZI
// ============================================================
#[derive(Debug, Default, Clone, Copy)]
pub struct CopyStats {
    pub files: usize,
    pub bytes: u64,
    /// Kopyalanan (ya da taranan) dosyalarin yol + icerik ozeti.
    pub fingerprint: u64,
}

/// Metin iceriginin surec icinde kararli ozeti (dogrulama kaydi icin).
pub fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(text.as_bytes());
    hasher.finish()
}

/// Proje kokunu gecici alana kopyalar ve ayni gecisle parmak izini cikarir.
/// Gizli girdiler (.git, .env, .fiko_backups), `target` ve sembolik baglantilar atlanir.
pub fn copy_project(root: &Path, dest: &Path) -> Result<CopyStats, String> {
    walk_project(root, Some(dest))
}

/// Projenin su anki parmak izi: kopyalama ile ayni dosya kumesi ve ayni sira.
/// Dogrulamadan sonra herhangi bir proje dosyasi degisirse bu deger degisir.
pub fn project_fingerprint(root: &Path) -> Result<u64, String> {
    walk_project(root, None).map(|stats| stats.fingerprint)
}

fn walk_project(root: &Path, dest: Option<&Path>) -> Result<CopyStats, String> {
    let mut stats = CopyStats::default();
    let mut hasher = DefaultHasher::new();
    walk_dir(root, "", dest, &mut stats, &mut hasher)?;
    stats.fingerprint = hasher.finish();
    Ok(stats)
}

fn walk_dir(
    from: &Path,
    rel_prefix: &str,
    dest: Option<&Path>,
    stats: &mut CopyStats,
    hasher: &mut DefaultHasher,
) -> Result<(), String> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(from)
        .map_err(|e| format!("{} okunamadi: {e}", from.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    // Parmak izi siraya bagli; dosya sistemi sirasina guvenme.
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text.starts_with('.') || name_text == "target" {
            continue;
        }
        let relative = if rel_prefix.is_empty() {
            name_text.to_string()
        } else {
            format!("{rel_prefix}/{name_text}")
        };
        let source = entry.path();
        let target = dest.map(|d| d.join(&name));

        if kind.is_dir() {
            if let Some(target) = &target {
                fs::create_dir(target)
                    .map_err(|e| format!("{} olusturulamadi: {e}", target.display()))?;
            }
            walk_dir(&source, &relative, target.as_deref(), stats, hasher)?;
        } else if kind.is_file() {
            let bytes =
                fs::read(&source).map_err(|e| format!("{} okunamadi: {e}", source.display()))?;
            stats.files += 1;
            stats.bytes += bytes.len() as u64;
            if stats.files > MAX_COPY_FILES {
                return Err(format!(
                    "Proje {MAX_COPY_FILES} dosya sinirini asiyor; gecici kopya alinamadi."
                ));
            }
            if stats.bytes > MAX_COPY_BYTES {
                return Err(format!(
                    "Proje {} MB kopya sinirini asiyor; gecici kopya alinamadi.",
                    MAX_COPY_BYTES / (1024 * 1024)
                ));
            }
            hasher.write(relative.as_bytes());
            hasher.write_u64(bytes.len() as u64);
            hasher.write(&bytes);
            if let Some(target) = &target {
                fs::write(target, &bytes)
                    .map_err(|e| format!("{} kopyalanamadi: {e}", target.display()))?;
            }
        }
    }
    Ok(())
}

/// `\\?\C:\...` biciminde kanonik yolu cargo/rustc icin sade yola cevirir.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\")
        && !rest.starts_with("UNC\\")
    {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

// ============================================================
// DERLEYICI / TEST CIKTISI OZETI
// ============================================================
fn is_error_start(line: &str) -> bool {
    line.starts_with("error[") || line.starts_with("error:")
}

/// Cargo/rustc'nin toplam ozet satirlari gercek bir hata degildir.
fn counts_as_error(line: &str) -> bool {
    if line.starts_with("error[") {
        return true;
    }
    line.starts_with("error:")
        && !line.contains("could not compile")
        && !line.contains("aborting due to")
        && !line.contains("build failed")
        && !line.contains("process didn't exit successfully")
        && !line.contains("test failed")
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("\n[... kisaltildi]");
    out
}

/// rustc stderr ciktisindan hata bloklarini ayiklar. Donus: (hata sayisi, bloklar).
pub fn extract_diagnostics(stderr: &str) -> (usize, Vec<String>) {
    let mut blocks: Vec<String> = Vec::new();
    let mut error_count = 0usize;
    let mut current: Vec<&str> = Vec::new();
    let mut keep_current = false;

    let flush = |current: &mut Vec<&str>, keep: bool, blocks: &mut Vec<String>| {
        if keep && !current.is_empty() {
            blocks.push(clip(&current.join("\n"), MAX_DIAGNOSTIC_CHARS));
        }
        current.clear();
    };

    for raw in stderr.lines() {
        let line = raw.trim_end();
        if is_error_start(line) {
            flush(&mut current, keep_current, &mut blocks);
            keep_current = counts_as_error(line);
            if keep_current {
                error_count += 1;
            }
            current.push(line);
        } else if line.is_empty() {
            flush(&mut current, keep_current, &mut blocks);
            keep_current = false;
        } else if !current.is_empty() {
            current.push(line);
        }
    }
    flush(&mut current, keep_current, &mut blocks);

    if blocks.len() > MAX_DIAGNOSTICS {
        let hidden = blocks.len() - MAX_DIAGNOSTICS;
        blocks.truncate(MAX_DIAGNOSTICS);
        blocks.push(format!("[+{hidden} diagnostik daha gizlendi]"));
    }
    (error_count, blocks)
}

/// libtest stdout ciktisindan `failures:` bolumunu `test result:` satirina kadar alir.
pub fn extract_test_failures(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buffer: Vec<&str> = Vec::new();
    let mut capturing = false;
    for line in stdout.lines() {
        if !capturing && line.trim() == "failures:" {
            capturing = true;
            buffer.push(line);
            continue;
        }
        if capturing {
            buffer.push(line);
            if line.starts_with("test result:") {
                out.push(clip(&buffer.join("\n"), MAX_DIAGNOSTIC_CHARS * 2));
                buffer.clear();
                capturing = false;
            }
        }
    }
    out
}

fn summary_line(mode: &str, success: bool, result: &TestResult) -> String {
    if success {
        return format!("cargo {mode} gecici kopyada basarili");
    }
    if let Some(line) = result
        .stderr
        .lines()
        .rev()
        .map(str::trim_end)
        .find(|line| line.starts_with("error"))
    {
        return line.to_string();
    }
    if let Some(line) = result
        .stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("test result:"))
    {
        return line.trim_end().to_string();
    }
    format!("cargo {mode} basarisiz (ayrinti yok)")
}

fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let skipped = count - limit;
    let body: String = text.chars().skip(skipped).collect();
    format!("[... ilk {skipped} karakter atlandi]\n{body}")
}

// ============================================================
// DOGRULAMA RAPORU
// ============================================================
#[derive(Debug)]
pub struct VerificationReport {
    pub success: bool,
    pub mode: String,
    pub relative_path: String,
    pub workspace: PathBuf,
    pub files_copied: usize,
    pub error_count: usize,
    pub diagnostics: Vec<String>,
    pub summary: String,
    pub cleanup_error: Option<String>,
    /// v0.8 kilidi icin: dogrulanan orijinal icerik, yeni icerik ve proje durumu ozetleri.
    pub original_hash: u64,
    pub updated_hash: u64,
    pub project_fingerprint: u64,
}

impl VerificationReport {
    /// Model icin yapilandirilmis metin. Ilk satir makine tarafindan okunabilir durumdur.
    pub fn render(&self, attempt: usize, max_attempts: usize) -> String {
        let status = if self.success {
            "VERIFY_OK"
        } else {
            "VERIFY_FAILED"
        };
        let mut out = format!(
            "{status} (deneme {attempt}/{max_attempts})\nmode: cargo {}\ndosya: {}\nkaynak: gecici kopya (ana proje DEGISMEDI)\n",
            self.mode, self.relative_path
        );
        if self.success {
            out.push_str(&format!("sonuc: {}\n", self.summary));
        } else {
            out.push_str(&format!(
                "hata_sayisi: {}\nozet: {}\n",
                self.error_count, self.summary
            ));
            if !self.diagnostics.is_empty() {
                out.push_str("--- derleyici/test ciktisi ---\n");
                for block in &self.diagnostics {
                    out.push_str(block);
                    out.push('\n');
                }
                out.push_str("--- son ---\n");
            }
        }
        if let Some(err) = &self.cleanup_error {
            out.push_str(&format!("uyari: gecici klasor silinemedi: {err}\n"));
        }
        clip(&out, MAX_REPORT_CHARS)
    }
}

// ============================================================
// ADAY DOGRULAMA
// ============================================================
async fn run_in_workspace(
    root: &Path,
    workspace: &Path,
    plan: &PatchPlan,
    relative: &Path,
    mode: &str,
) -> Result<(TestResult, CopyStats), String> {
    let stats = copy_project(root, workspace)?;
    let candidate = workspace.join(relative);
    if !candidate.is_file() {
        return Err("Hedef dosya gecici kopyada bulunamadi.".into());
    }
    fs::write(&candidate, &plan.updated)
        .map_err(|e| format!("Aday gecici kopyaya yazilamadi: {e}"))?;

    let target_dir = strip_verbatim(&root.join("target").join(VERIFY_TARGET_SUBDIR));
    let result = run_cargo(
        workspace,
        mode,
        Some(&target_dir),
        Duration::from_secs(VERIFY_TIMEOUT_SECS),
    )
    .await?;
    Ok((result, stats))
}

/// `plan` prepare_patch'ten gecmis olmali. Ana projedeki dosya hic degistirilmez;
/// aday icerik yalnizca gecici kopyaya yazilir ve orada cargo calistirilir.
pub async fn verify_candidate(
    root: &Path,
    plan: &PatchPlan,
    mode: &str,
) -> Result<VerificationReport, String> {
    if mode != "check" && mode != "test" {
        return Err("Gecersiz dogrulama modu. Sadece check veya test kullanilabilir.".into());
    }
    let root = root
        .canonicalize()
        .map_err(|e| format!("Proje koku cozulemedi: {e}"))?;
    if !root.join("Cargo.toml").is_file() {
        return Err(
            "Cargo.toml bulunamadi; cargo dogrulamasi yalnizca Rust projelerinde yapilabilir."
                .into(),
        );
    }
    let relative = plan
        .path
        .strip_prefix(&root)
        .map_err(|_| "Patch hedefi proje koku altinda degil.".to_string())?
        .to_path_buf();
    // Ikinci savunma hatti: prepare_patch zaten dogruladi, yine de kontrol et.
    check_relative_path(&relative.to_string_lossy())?;
    if relative.extension().and_then(|e| e.to_str()) != Some("rs") {
        return Err("Sadece .rs adaylari dogrulanabilir.".into());
    }

    let mut workspace = TempWorkspace::create(&root)?;
    let workspace_path = workspace.path().to_path_buf();
    let outcome = run_in_workspace(&root, &workspace_path, plan, &relative, mode).await;
    let cleanup_error = workspace.cleanup().await.err();
    let (result, stats) = outcome?;

    let success = result.success;
    let (mut error_count, mut diagnostics) = extract_diagnostics(&result.stderr);
    if !success && mode == "test" {
        let failures = extract_test_failures(&result.stdout);
        if !failures.is_empty() {
            if error_count == 0 {
                error_count = failures.len();
            }
            diagnostics.extend(failures);
        }
    }
    if !success && diagnostics.is_empty() {
        let raw = format!("{}\n{}", result.stdout.trim_end(), result.stderr.trim_end());
        diagnostics.push(tail(raw.trim(), MAX_DIAGNOSTIC_CHARS));
    }
    let summary = summary_line(mode, success, &result);

    Ok(VerificationReport {
        success,
        mode: mode.to_string(),
        relative_path: relative.to_string_lossy().replace('\\', "/"),
        workspace: workspace_path,
        files_copied: stats.files,
        error_count,
        diagnostics,
        summary,
        cleanup_error,
        original_hash: content_hash(&plan.original),
        updated_hash: content_hash(&plan.updated),
        project_fingerprint: stats.fingerprint,
    })
}

// ============================================================
// TESTLER
// ============================================================
#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    pub const MINI_MAIN: &str =
        "fn main() {\n    let value: i32 = 1;\n    println!(\"{value}\");\n}\n";
    pub const MINI_OLD: &str = "let value: i32 = 1;";
    pub const MINI_GOOD: &str = "let value: i32 = 2;";
    pub const MINI_BAD: &str = "let value: i32 = \"iki\";";

    /// Bagimliligi olmayan, derlenebilir bir mini crate olusturur ve kanonik kokunu dondurur.
    pub fn mini_crate(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fiko_mini_{tag}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fiko_mini\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), MINI_MAIN).unwrap();
        root.canonicalize().unwrap()
    }

    pub fn remove(root: &std::path::Path) {
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::tools::prepare_patch;

    #[tokio::test]
    async fn gecerli_aday_patch_dogrulanmali() {
        let root = mini_crate("valid");
        let plan = prepare_patch(&root, "src/main.rs", MINI_OLD, MINI_GOOD).unwrap();
        let report = verify_candidate(&root, &plan, "check").await.unwrap();
        assert!(report.success, "rapor: {}", report.render(1, 3));
        assert_eq!(report.error_count, 0);
        assert_eq!(report.relative_path, "src/main.rs");
        assert!(report.render(1, 3).starts_with("VERIFY_OK"));
        assert!(report.cleanup_error.is_none());
        assert!(!report.workspace.exists());
        // Ana dosya degismedi.
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            MINI_MAIN
        );
        remove(&root);
    }

    #[tokio::test]
    async fn derlenmeyen_aday_hata_dondurmeli() {
        let root = mini_crate("broken");
        let plan = prepare_patch(&root, "src/main.rs", MINI_OLD, MINI_BAD).unwrap();
        let report = verify_candidate(&root, &plan, "check").await.unwrap();
        assert!(!report.success);
        assert!(report.error_count >= 1);
        let joined = report.diagnostics.join("\n");
        assert!(joined.contains("E0308"), "diagnostik: {joined}");
        let text = report.render(1, 3);
        assert!(text.starts_with("VERIFY_FAILED"));
        assert!(text.contains("hata_sayisi:"));
        remove(&root);
    }

    #[tokio::test]
    async fn basarisiz_dogrulamada_ana_dosya_korunmali() {
        let root = mini_crate("protect");
        let plan = prepare_patch(&root, "src/main.rs", MINI_OLD, MINI_BAD).unwrap();
        let report = verify_candidate(&root, &plan, "check").await.unwrap();
        assert!(!report.success);
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            MINI_MAIN
        );
        // Gecici dogrulama yedek klasoru olusturmaz; yedek yalnizca gercek patch icindir.
        assert!(!root.join(".fiko_backups").exists());
        remove(&root);
    }

    #[tokio::test]
    async fn gecici_calisma_alani_temizlenmeli() {
        let root = mini_crate("cleanup");
        let plan = prepare_patch(&root, "src/main.rs", MINI_OLD, MINI_BAD).unwrap();
        let report = verify_candidate(&root, &plan, "check").await.unwrap();
        assert!(report.cleanup_error.is_none(), "{:?}", report.cleanup_error);
        assert!(!report.workspace.exists());
        assert!(
            report
                .workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fiko_verify_")
        );
        remove(&root);
    }

    #[tokio::test]
    async fn path_traversal_aday_reddedilmeli() {
        let root = mini_crate("traversal");
        // prepare_patch ilk savunma hattidir.
        assert!(prepare_patch(&root, "../src/main.rs", MINI_OLD, MINI_GOOD).is_err());
        assert!(prepare_patch(&root, "src/../../main.rs", MINI_OLD, MINI_GOOD).is_err());
        // Ikinci savunma hatti: proje disini gosteren sahte bir plan da reddedilir.
        let outside = std::env::temp_dir().join("fiko_outside.rs");
        let forged = PatchPlan {
            path: outside,
            original: MINI_MAIN.into(),
            updated: MINI_MAIN.replace(MINI_OLD, MINI_GOOD),
        };
        let err = verify_candidate(&root, &forged, "check").await.unwrap_err();
        assert!(err.contains("proje koku altinda degil"), "{err}");
        remove(&root);
    }

    #[tokio::test]
    async fn gecici_calisma_alani_benzersiz_olmali() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let mut first = TempWorkspace::create(&root).unwrap();
        let mut second = TempWorkspace::create(&root).unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir() && second.path().is_dir());
        first.cleanup().await.unwrap();
        second.cleanup().await.unwrap();
        assert!(!first.path().exists() && !second.path().exists());
    }

    #[test]
    fn kopya_gizli_ve_target_atlamali() {
        let root = mini_crate("copy");
        fs::write(root.join(".env"), "SECRET=1").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/artifact.bin"), "x").unwrap();
        let dest = std::env::temp_dir().join(format!("fiko_copy_dest_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir(&dest).unwrap();
        let stats = copy_project(&root, &dest).unwrap();
        assert_eq!(stats.files, 2, "Cargo.toml ve src/main.rs beklenir");
        assert!(dest.join("Cargo.toml").is_file());
        assert!(dest.join("src/main.rs").is_file());
        assert!(!dest.join(".env").exists());
        assert!(!dest.join("target").exists());
        // Kopyanin parmak izi, kaynagin dogrudan taramasiyla ayni olmali.
        assert_eq!(stats.fingerprint, project_fingerprint(&root).unwrap());
        remove(&dest);
        remove(&root);
    }

    #[test]
    fn proje_parmak_izi_degisimi_yakalamali() {
        let root = mini_crate("fingerprint");
        let before = project_fingerprint(&root).unwrap();
        assert_eq!(
            before,
            project_fingerprint(&root).unwrap(),
            "kararli olmali"
        );
        // Hedef disindaki bir proje dosyasi degisince de parmak izi degisir.
        let cargo = root.join("Cargo.toml");
        let mut text = fs::read_to_string(&cargo).unwrap();
        text.push_str("\n# degisti\n");
        fs::write(&cargo, text).unwrap();
        assert_ne!(before, project_fingerprint(&root).unwrap());
        // target/ ve gizli dosyalar parmak izine girmez.
        let after = project_fingerprint(&root).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/x.bin"), "x").unwrap();
        fs::write(root.join(".env"), "SECRET=1").unwrap();
        assert_eq!(after, project_fingerprint(&root).unwrap());
        assert_ne!(content_hash("a"), content_hash("b"));
        assert_eq!(content_hash("ayni"), content_hash("ayni"));
        remove(&root);
    }

    #[test]
    fn derleyici_ciktisi_ozetlenmeli() {
        let stderr = "   Checking demo v0.1.0 (C:\\demo)\n\
error[E0308]: mismatched types\n \
 --> src/main.rs:2:22\n  |\n2 |     let value: i32 = \"iki\";\n  |                      ^^^^^ expected `i32`, found `&str`\n\
\n\
error[E0425]: cannot find value `x` in this scope\n --> src/main.rs:3:5\n\
\n\
error: aborting due to 2 previous errors\n\
\n\
error: could not compile `demo` (bin \"demo\") due to 2 previous errors\n";
        let (count, blocks) = extract_diagnostics(stderr);
        assert_eq!(count, 2);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("error[E0308]"));
        assert!(blocks[0].contains("src/main.rs:2:22"));
        assert!(blocks[1].starts_with("error[E0425]"));
    }

    #[test]
    fn test_hatasi_ozetlenmeli() {
        let stdout = "running 2 tests\ntest tests::ok ... ok\ntest tests::bad ... FAILED\n\n\
failures:\n\n---- tests::bad stdout ----\nassertion failed: 1 == 2\n\n\
failures:\n    tests::bad\n\n\
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let failures = extract_test_failures(stdout);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("tests::bad"));
        assert!(failures[0].contains("assertion failed"));
        assert!(failures[0].contains("test result: FAILED"));
    }
}
