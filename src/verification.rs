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
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::repair_controller::VerifyMode;
use crate::tools::{
    CommandObservation, ExecutionStatus, PatchPlan, TestResult, check_relative_path, run_cargo,
};
use crate::workspace_patch::WorkspacePlan;

/// Ilk dogrulama bagimliliklari sifirdan derleyebilir; bu yuzden run_tests'ten uzun.
pub const VERIFY_TIMEOUT_SECS: u64 = 300;
pub const MAX_COPY_FILES: usize = 2_000;
pub const MAX_COPY_BYTES: u64 = 32 * 1024 * 1024;
/// Gecici kopyanin derleme ciktilari ana projenin target/ klasoru altinda,
/// ana derleme ciktilarindan ayri, canli calisma alani basina bir alt klasorde tutulur.
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
/// Ilk surum yalnizca Cargo projelerini dogrular. Diger turler, komut
/// calistirilmadan acikca desteklenmiyor olarak bildirilir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationKind {
    Cargo,
    Unsupported,
}

pub fn verification_kind(root: &Path) -> Result<VerificationKind, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    match fs::symlink_metadata(root.join("Cargo.toml")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(VerificationKind::Unsupported),
        Err(error) => Err(format!("Cargo.toml okunamadi: {error}")),
        Ok(_) => {
            let manifest = crate::discovery::resolve_existing(&root, "Cargo.toml")?;
            if !manifest.is_file() {
                return Err("Cargo.toml normal bir dosya olmali.".into());
            }
            Ok(VerificationKind::Cargo)
        }
    }
}

fn require_cargo(root: &Path) -> Result<(), String> {
    match verification_kind(root)? {
        VerificationKind::Cargo => Ok(()),
        VerificationKind::Unsupported => Err(
            "Cargo.toml bulunamadi; proje turu desteklenmiyor, dogrulama CALISTIRILMADI.".into(),
        ),
    }
}

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
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if let Some(dest) = dest {
        let dest = dest.canonicalize().map_err(|e| e.to_string())?;
        if dest.starts_with(&root) {
            return Err("Gecici kopya kaynak proje icinde olamaz.".into());
        }
    }
    let mut stats = CopyStats::default();
    let mut hasher = DefaultHasher::new();
    let mut entries_left = MAX_COPY_FILES * 4;
    walk_dir(
        &root,
        &root,
        "",
        dest,
        &mut stats,
        &mut hasher,
        &mut entries_left,
    )?;
    stats.fingerprint = hasher.finish();
    Ok(stats)
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn walk_dir(
    root: &Path,
    from: &Path,
    rel_prefix: &str,
    dest: Option<&Path>,
    stats: &mut CopyStats,
    hasher: &mut DefaultHasher,
    entries_left: &mut usize,
) -> Result<(), String> {
    if Path::new(rel_prefix).components().count() > 64 {
        return Err("Proje dizin derinligi 64 sinirini asiyor.".into());
    }
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(from)
        .map_err(|e| format!("{} okunamadi: {e}", from.display()))?
        .take(entries_left.saturating_add(1))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if entries.len() > *entries_left {
        return Err("Proje dizin girdisi tarama sinirini asiyor.".into());
    }
    *entries_left -= entries.len();
    // Parmak izi siraya bagli; dosya sistemi sirasina guvenme.
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink()
            || is_reparse_point(&fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?)
        {
            continue;
        }
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        let relative = if rel_prefix.is_empty() {
            name_text.to_string()
        } else {
            format!("{rel_prefix}/{name_text}")
        };
        if check_relative_path(&relative).is_err() {
            continue;
        }
        let source = crate::discovery::resolve_existing(root, &relative)?;
        let target = dest.map(|d| d.join(&name));

        if kind.is_dir() {
            if let Some(target) = &target {
                fs::create_dir(target)
                    .map_err(|e| format!("{} olusturulamadi: {e}", target.display()))?;
            }
            walk_dir(
                root,
                &source,
                &relative,
                target.as_deref(),
                stats,
                hasher,
                entries_left,
            )?;
        } else if kind.is_file() {
            if stats.files >= MAX_COPY_FILES {
                return Err(format!(
                    "Proje {MAX_COPY_FILES} dosya sinirini asiyor; gecici kopya alinamadi."
                ));
            }
            let remaining = MAX_COPY_BYTES.saturating_sub(stats.bytes);
            if fs::metadata(&source).map_err(|e| e.to_string())?.len() > remaining {
                return Err(format!(
                    "Proje {} MB kopya sinirini asiyor; gecici kopya alinamadi.",
                    MAX_COPY_BYTES / (1024 * 1024)
                ));
            }
            let mut bytes = Vec::new();
            fs::File::open(&source)
                .and_then(|file| file.take(remaining + 1).read_to_end(&mut bytes))
                .map_err(|e| format!("{} okunamadi: {e}", source.display()))?;
            if bytes.len() as u64 > remaining {
                return Err("Proje okunurken kopya byte sinirini asti.".into());
            }
            stats.files += 1;
            stats.bytes += bytes.len() as u64;
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
    pub processes: Vec<CommandObservation>,
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
/// Baslatilamayan veya tamamlanmayan komut, adayin derlenmedigi/testten
/// kaldigi anlami tasimaz. Eski hata yolunu ve asil surec kanitini koru.
fn completed_result(result: TestResult) -> Result<TestResult, String> {
    match result.status {
        ExecutionStatus::Succeeded | ExecutionStatus::Failed => Ok(result),
        ExecutionStatus::ExecutionError
        | ExecutionStatus::TimedOut
        | ExecutionStatus::Interrupted => Err(result.report),
    }
}

/// Her canli dogrulama ayri bir Cargo target dizini kullanir. Bu, paralel
/// Windows linker'larin ayni cikti dosyasi uzerinde yarisini azaltir.
fn verification_target(root: &Path, group: &str, tag: &str) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mut target = root.clone();
    for component in ["target", group, tag] {
        target.push(component);
        match fs::create_dir(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("Cargo target dizini olusturulamadi: {error}")),
        }
        let metadata = fs::symlink_metadata(&target).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || is_reparse_point(&metadata) {
            return Err("Cargo target yolu normal bir dizin olmali; symlink/reparse yasak.".into());
        }
        target = target.canonicalize().map_err(|e| e.to_string())?;
        if !target.starts_with(&root) {
            return Err("Cargo target dizini proje disina cikiyor.".into());
        }
    }
    Ok(strip_verbatim(&target))
}

async fn cargo_sequence(
    root: &Path,
    mode: VerifyMode,
    target: &Path,
) -> Result<Vec<TestResult>, String> {
    require_cargo(root)?;
    let mut results = Vec::new();
    for command in [VerifyMode::Check, VerifyMode::Test] {
        if command > mode {
            break;
        }
        let result = run_cargo(
            root,
            command.as_str(),
            Some(target),
            Duration::from_secs(VERIFY_TIMEOUT_SECS),
        )
        .await?;
        let success = result.status == ExecutionStatus::Succeeded;
        results.push(result);
        if !success {
            break;
        }
    }
    Ok(results)
}

/// Modelin raporu degil, gecici kopyada gozlenen gercek surec kayitlari.
#[derive(Debug)]
pub struct WorkspaceVerification {
    pub results: Vec<TestResult>,
    pub mode: VerifyMode,
    pub project_fingerprint: u64,
    pub cleanup_error: Option<String>,
}

impl WorkspaceVerification {
    pub fn success(&self) -> bool {
        self.results.len() == (if self.mode == VerifyMode::Test { 2 } else { 1 })
            && self
                .results
                .iter()
                .all(|result| result.status == ExecutionStatus::Succeeded)
    }

    pub fn render(&self) -> String {
        let status = if self.success() {
            "VERIFY_WORKSPACE_OK"
        } else {
            "VERIFY_WORKSPACE_FAILED"
        };
        let mut report = format!("{status}\nkaynak: gecici kopya (ana proje DEGISMEDI)\n");
        if self.results.is_empty() {
            report.push_str("Dogrulama CALISTIRILMADI; surec kaydi yok.\n");
        }
        for result in &self.results {
            report.push_str(&result.report);
            report.push('\n');
        }
        if let Some(error) = &self.cleanup_error {
            report.push_str(&format!("uyari: gecici klasor silinemedi: {error}\n"));
        }
        report
    }
}

fn matches_candidate(path: &Path, expected: &str) -> Result<bool, String> {
    if !fs::metadata(path)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Aday hedefi normal bir dosya olmali.".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(expected.len() as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes == expected.as_bytes())
}

/// Once kopyadaki tum eski/yok durumlarini kontrol et, sonra adaylari yaz.
/// Kopyalama sirasinda degisen hedef hicbir zaman dogrulanmis sayilmaz.
fn stage_workspace_candidate(workspace: &Path, plan: &WorkspacePlan) -> Result<(), String> {
    let mut targets = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        let target = crate::discovery::resolve_target(workspace, &file.relative)?;
        match &file.original {
            Some(original) => {
                if !matches_candidate(&target, original)? {
                    return Err(format!(
                        "Kopyalama sirasinda hedef degisti: {}",
                        file.relative
                    ));
                }
            }
            None => match fs::symlink_metadata(&target) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
                Ok(_) => {
                    return Err(format!(
                        "Yeni dosya kopyada zaten mevcut: {}",
                        file.relative
                    ));
                }
            },
        }
        targets.push(target);
    }
    for (file, target) in plan.files.iter().zip(targets) {
        fs::write(&target, &file.updated)
            .map_err(|e| format!("Aday kopyaya yazilamadi ({}): {e}", file.relative))?;
    }
    Ok(())
}

/// Bu fonksiyonu cagiran agent, VerifyConsent iznini onceden almis olmalidir.
/// Gecici kopya bir isletim sistemi sandbox'i degildir.
pub async fn verify_workspace(
    root: &Path,
    plan: &WorkspacePlan,
    mode: VerifyMode,
) -> Result<WorkspaceVerification, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    require_cargo(&root)?;
    crate::workspace_patch::revalidate(&root, plan)?;
    let mut workspace = TempWorkspace::create(&root)?;
    let outcome: Result<(Vec<TestResult>, u64), String> = async {
        let stats = copy_project(&root, workspace.path())?;
        crate::workspace_patch::revalidate(&root, plan)?;
        if project_fingerprint(&root)? != stats.fingerprint {
            return Err("Proje kopyalama sirasinda degisti; dogrulama baslatilmadi.".into());
        }
        stage_workspace_candidate(workspace.path(), plan)?;
        let tag = workspace.path().file_name().unwrap().to_string_lossy();
        let target = verification_target(&root, "fiko_workspace_verify", &tag)?;
        let results = cargo_sequence(workspace.path(), mode, &target).await?;
        if crate::workspace_patch::revalidate(&root, plan).is_err()
            || project_fingerprint(&root)? != stats.fingerprint
        {
            let evidence = results.iter().map(|result| result.report.as_str()).collect::<Vec<_>>().join("\n");
            return Err(format!("Kaynak proje dogrulama sirasinda degisti; Verified kaydi uretilmedi. Proje durumu yeniden incelenmeli.\n{evidence}"));
        }
        // Build script/proc-macro kodu aday dosyalarini da degistirebilir.
        // Boyle bir kopya, modelin onerdigi aday icin dogrulama kaniti olamaz.
        for file in &plan.files {
            let target = crate::discovery::resolve_existing(workspace.path(), &file.relative)
                .and_then(|path| matches_candidate(&path, &file.updated));
            if target != Ok(true) {
                let evidence = results.iter().map(|result| result.report.as_str()).collect::<Vec<_>>().join("\n");
                return Err(format!("Dogrulama sirasinda aday degisti/okunamadi: {}. Verified kaydi uretilmedi.\n{evidence}", file.relative));
            }
        }
        Ok((results, stats.fingerprint))
    }
    .await;
    let cleanup_error = workspace.cleanup().await.err();
    let (results, project_fingerprint) = outcome?;
    Ok(WorkspaceVerification {
        results,
        mode,
        project_fingerprint,
        cleanup_error,
    })
}

/// Gecici dogrulama izni ve gercek cargo check+test'i acikca kapsayan tek
/// workspace onayindan sonra cagirilir.
/// Tum dosyalar uygulandiktan sonra gercek projeyi bir butun olarak dogrular.
pub async fn verify_workspace_applied(root: &Path) -> Result<Vec<TestResult>, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    require_cargo(&root)?;
    let tag = format!(
        "{}_{}",
        std::process::id(),
        WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let target = verification_target(&root, "fiko_workspace_applied", &tag)?;
    cargo_sequence(&root, VerifyMode::Test, &target).await
}

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

    let tag = workspace.file_name().unwrap().to_string_lossy();
    let target_dir = verification_target(root, VERIFY_TARGET_SUBDIR, &tag)?;
    let result = run_cargo(
        workspace,
        mode,
        Some(&target_dir),
        Duration::from_secs(VERIFY_TIMEOUT_SECS),
    )
    .await?;
    Ok((completed_result(result)?, stats))
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
    require_cargo(&root)?;
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
        processes: vec![result.observation()],
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
    use crate::workspace_patch::{FileChange, prepare};

    #[tokio::test]
    async fn bilinmeyen_projede_cargo_dogrulamasi_calismamali() {
        let root = mini_crate("unsupported_workspace");
        fs::remove_file(root.join("Cargo.toml")).unwrap();
        let plan = prepare(
            &root,
            &[FileChange::Edit {
                path: "src/main.rs".into(),
                old_text: MINI_OLD.into(),
                new_text: MINI_GOOD.into(),
            }],
        )
        .unwrap();
        assert_eq!(
            verification_kind(&root).unwrap(),
            VerificationKind::Unsupported
        );
        let error = verify_workspace(&root, &plan, VerifyMode::Test)
            .await
            .unwrap_err();
        assert!(error.contains("CALISTIRILMADI"), "{error}");
        assert!(verify_workspace_applied(&root).await.is_err());
        assert!(!root.join("target").exists());
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            MINI_MAIN
        );
        remove(&root);
    }

    #[test]
    fn manifest_dizin_ise_proje_cargo_sayilmamali() {
        let root = mini_crate("manifest_directory");
        fs::remove_file(root.join("Cargo.toml")).unwrap();
        fs::create_dir(root.join("Cargo.toml")).unwrap();
        assert!(
            verification_kind(&root)
                .unwrap_err()
                .contains("normal bir dosya")
        );
        remove(&root);
    }

    #[tokio::test]
    async fn kopyadaki_ikinci_hedef_degisirse_hicbir_aday_yazilmamali() {
        let root = mini_crate("copy_drift");
        let original_other = "pub fn value() -> u32 { 1 }\n";
        fs::write(root.join("src/other.rs"), original_other).unwrap();
        let plan = prepare(
            &root,
            &[
                FileChange::Edit {
                    path: "src/main.rs".into(),
                    old_text: MINI_OLD.into(),
                    new_text: MINI_GOOD.into(),
                },
                FileChange::Edit {
                    path: "src/other.rs".into(),
                    old_text: original_other.into(),
                    new_text: "pub fn value() -> u32 { 2 }\n".into(),
                },
            ],
        )
        .unwrap();
        let mut workspace = TempWorkspace::create(&root).unwrap();
        copy_project(&root, workspace.path()).unwrap();
        fs::write(
            workspace.path().join("src/other.rs"),
            "pub fn value() -> u32 { 3 }\n",
        )
        .unwrap();
        assert!(
            stage_workspace_candidate(workspace.path(), &plan)
                .unwrap_err()
                .contains("hedef degisti")
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("src/main.rs")).unwrap(),
            MINI_MAIN
        );
        workspace.cleanup().await.unwrap();
        remove(&root);
    }

    #[tokio::test]
    async fn cok_dosyali_aday_butun_proje_olarak_dogrulanmali() {
        let root = mini_crate("workspace_verify");
        let plan = prepare(&root, &[
            FileChange::Edit {
                path: "src/main.rs".into(), old_text: MINI_MAIN.into(),
                new_text: "mod helper;\nfn main() { assert_eq!(helper::value(), 2); }\n#[test]\nfn value_test() { assert_eq!(helper::value(), 2); }\n".into(),
            },
            FileChange::Create { path: "src/helper.rs".into(), content: "pub fn value() -> u32 { 2 }\n".into() },
        ]).unwrap();
        let before = project_fingerprint(&root).unwrap();
        let report = verify_workspace(&root, &plan, VerifyMode::Test)
            .await
            .unwrap();
        assert!(report.success(), "{}", report.render());
        assert_eq!(report.results.len(), 2);
        assert!(
            report
                .results
                .iter()
                .all(|result| result.exit_code == Some(0))
        );
        assert_eq!(report.project_fingerprint, before);
        assert_eq!(project_fingerprint(&root).unwrap(), before);
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            MINI_MAIN
        );
        assert!(!root.join("src/helper.rs").exists());
        assert!(report.cleanup_error.is_none());
        remove(&root);
    }

    #[tokio::test]
    async fn build_script_adayi_degistirirse_basarili_surec_kilit_uretememeli() {
        let root = mini_crate("workspace_mutating_build");
        let original = "pub fn value() -> u32 { 1 }\n";
        fs::write(root.join("src/helper.rs"), original).unwrap();
        fs::write(root.join("build.rs"), "fn main() { std::fs::write(\"src/helper.rs\", \"pub fn value() -> u32 { 3 }\\n\").unwrap(); }\n").unwrap();
        let plan = prepare(
            &root,
            &[FileChange::Edit {
                path: "src/helper.rs".into(),
                old_text: original.into(),
                new_text: "pub fn value() -> u32 { 2 }\n".into(),
            }],
        )
        .unwrap();
        let error = verify_workspace(&root, &plan, VerifyMode::Check)
            .await
            .unwrap_err();
        assert!(error.contains("Verified kaydi uretilmedi"), "{error}");
        assert!(
            error.contains("Durum: succeeded") && error.contains("Cikis kodu: 0"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/helper.rs")).unwrap(),
            original
        );
        remove(&root);
    }

    #[test]
    fn eksik_test_sureci_dogrulanmis_sayilmamali() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        let result = TestResult::from_output(
            "cargo check".into(),
            std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        );
        let mut report = WorkspaceVerification {
            results: vec![result],
            mode: VerifyMode::Test,
            project_fingerprint: 1,
            cleanup_error: None,
        };
        assert!(
            !report.success(),
            "Test modu icin sadece cargo check yeterli degil"
        );
        report.results.clear();
        report.mode = VerifyMode::Check;
        assert!(!report.success());
        assert!(report.render().contains("CALISTIRILMADI"));
    }

    #[tokio::test]
    async fn gecici_build_script_kaynak_projeyi_degistirirse_kilit_uretilmemeli() {
        let root = mini_crate("workspace_source_drift");
        let changed = MINI_MAIN.replace(MINI_OLD, "let value: i32 = 99;");
        fs::write(
            root.join("build.rs"),
            format!(
                "fn main() {{ std::fs::write({:?}, {:?}).unwrap(); }}",
                root.join("src/main.rs"),
                changed
            ),
        )
        .unwrap();
        let plan = prepare(
            &root,
            &[FileChange::Edit {
                path: "src/main.rs".into(),
                old_text: MINI_OLD.into(),
                new_text: MINI_GOOD.into(),
            }],
        )
        .unwrap();
        let result = verify_workspace(&root, &plan, VerifyMode::Check).await;
        let actual = fs::read_to_string(root.join("src/main.rs")).unwrap();
        remove(&root);
        let error = result.unwrap_err();
        assert!(
            error.contains("Kaynak proje dogrulama sirasinda degisti"),
            "{error}"
        );
        assert!(
            error.contains("Cikis kodu: 0"),
            "Gercek surec kaniti korunmali: {error}"
        );
        assert_eq!(actual, changed);
    }

    #[tokio::test]
    async fn kopya_limiti_dosya_okunmadan_denetlenmeli() {
        let root = mini_crate("copy_byte_limit");
        fs::File::create(root.join("too_large.bin"))
            .unwrap()
            .set_len(MAX_COPY_BYTES + 1)
            .unwrap();
        let mut workspace = TempWorkspace::create(&root).unwrap();
        let error = copy_project(&root, workspace.path()).unwrap_err();
        assert!(error.contains("kopya sinirini"), "{error}");
        assert!(!workspace.path().join("too_large.bin").exists());
        workspace.cleanup().await.unwrap();
        remove(&root);
    }

    #[test]
    fn tamamlanmamis_komut_aday_hatasi_sayilmamali_ve_kanit_korunmali() {
        for result in [
            TestResult::execution_error("cargo test".into(), "OS hatasi: dosya bulunamadi".into()),
            TestResult::timed_out("cargo test".into()),
        ] {
            let evidence = result.report.clone();
            assert_eq!(completed_result(result).unwrap_err(), evidence);
        }
    }

    #[tokio::test]
    async fn gecerli_aday_patch_dogrulanmali() {
        let root = mini_crate("valid");
        let plan = prepare_patch(&root, "src/main.rs", MINI_OLD, MINI_GOOD).unwrap();
        let report = verify_candidate(&root, &plan, "check").await.unwrap();
        assert!(report.success, "rapor: {}", report.render(1, 3));
        assert_eq!(
            report.processes,
            vec![CommandObservation {
                command: format!("{} check", crate::tools::cargo_bin()),
                status: ExecutionStatus::Succeeded,
                exit_code: Some(0),
            }]
        );
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
