//! v1.1: goreve ait sinirli baglam butcesi ve kaynak okumayan kesif onerileri.
//!
//! Metadata, dosyanin degismedigini kanitlamaz. Bu modul disk icerigi icin
//! metadata-only cache tutmaz; mevcut orkestrator defteri yalniz gercekten
//! yeniden gozlenen ayni icerigin modele tekrar gonderilmesini onler.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path};

use crate::discovery;

pub const MAX_CONTEXT_BYTES: usize = 64_000;
pub const MAX_HINT_CHARS: usize = 1_500;
const MAX_HINT_ENTRIES: usize = 12;
const LARGE_CONTEXT_FILE_BYTES: u64 = 8_000;

/// Modele ilk kez sunulan okuma ciktilarinin gorev basina byte butcesi.
/// Butce izin/dogrulama vermez ve kaynak icerigini kirparak uygulamaz.
#[derive(Debug, Default, Clone)]
pub struct ContextBudget {
    used: usize,
}

impl ContextBudget {
    pub fn used(&self) -> usize {
        self.used
    }

    pub fn remaining(&self) -> usize {
        MAX_CONTEXT_BYTES.saturating_sub(self.used)
    }

    /// Okuma oncesi bilinen boyut icin kontrol; rezervasyon yapmaz.
    pub fn admit(&self, estimated_bytes: usize) -> Result<(), String> {
        if estimated_bytes > self.remaining() {
            return Err(format!(
                "BAGLAM_SINIRI: Gorev icin {} byte kaldi; {estimated_bytes} byte yeni baglam alinamaz. search_in_file ve daha dar read_lines araligi kullan.",
                self.remaining()
            ));
        }
        Ok(())
    }

    /// Gercekten modele sunulacak byte sayisini kaydeder. Red butceyi degistirmez.
    pub fn record(&mut self, actual_bytes: usize) -> Result<(), String> {
        self.admit(actual_bytes)?;
        self.used += actual_bytes;
        Ok(())
    }
}

/// Gorunur goreli yol normalizasyonu. Dosya sistemi erisim yetkisi degildir;
/// okuma/yazmada resolve_existing/resolve_target yeniden kullanilmalidir.
pub fn normalize_path(relative: &str) -> Result<String, String> {
    discovery::check_relative_path(relative)?;
    let slash_path = relative.replace('\\', "/");
    let components: Vec<_> = Path::new(&slash_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    Ok(if components.is_empty() {
        ".".into()
    } else {
        components.join("/")
    })
}

/// Acik hedeflerde yalniz guvenli metadata, hedef yoksa sinirli klasor listesi.
/// Kaynak govdesi okunmaz; model icin ek bir tur veya islem izni olusturulmaz.
pub fn hint(root: &Path, paths: &[String]) -> String {
    let mut lines = vec![
        "[BAGLAM] Once ilgili sembolu search_code/search_in_file ile bul; buyuk dosyada gereken read_lines araligini sec. Ayni icerigi tekrar isteme.".to_owned(),
    ];
    if paths.is_empty() {
        let listing =
            discovery::list_files(root, "src").or_else(|_| discovery::list_files(root, "."));
        match listing {
            Ok(listing) => {
                lines.push("Izinli yapi onizlemesi (kaynak icerigi okunmadi):".into());
                lines.extend(listing.lines().take(MAX_HINT_ENTRIES).map(str::to_owned));
                if listing.lines().count() > MAX_HINT_ENTRIES {
                    lines.push("[YAPI ONIZLEMESI KISALTILDI]".into());
                }
            }
            Err(_) => lines.push(
                "Proje yapisi henuz gozlenemedi; erisilebilir kokte list_files kullan.".into(),
            ),
        }
    } else {
        let mut seen = HashSet::new();
        for relative in paths.iter().take(MAX_HINT_ENTRIES) {
            let Ok(relative) = normalize_path(relative) else {
                continue;
            };
            let key = if cfg!(windows) {
                relative.to_lowercase()
            } else {
                relative.clone()
            };
            if !seen.insert(key) {
                continue;
            }
            match discovery::resolve_existing(root, &relative) {
                Ok(path) => {
                    match fs::metadata(path) {
                        Ok(metadata)
                            if metadata.is_file() && metadata.len() > LARGE_CONTEXT_FILE_BYTES =>
                        {
                            lines.push(format!("{relative}: {} byte; tum dosya yerine search_in_file, sonra sinirli read_lines kullan.", metadata.len()));
                        }
                        Ok(metadata) if metadata.is_file() => {
                            lines.push(format!("{relative}: {} byte; ilgili icerik read_file ile sinirli alinabilir.", metadata.len()));
                        }
                        Ok(metadata) if metadata.is_dir() => lines.push(format!(
                            "{relative}: dizin; list_files ile ilgili dosyayi sec."
                        )),
                        _ => lines.push(format!("{relative}: normal dosya olarak gozlenemedi.")),
                    }
                }
                Err(_) => {
                    // resolve_target da symlink/reparse, hassas dosya ve dis-kok
                    // yollarini reddeder. Yalniz guvenli yeni hedefi belirt.
                    if discovery::resolve_target(root, &relative).is_ok() {
                        lines.push(format!(
                            "{relative}: yeni dosya hedefi; once mevcut ebeveyn yapisini incele."
                        ));
                    }
                }
            }
        }
        if lines.len() == 1 {
            lines.push("Acik hedefler izinli normal dosya olarak henuz gozlenemedi; hassas/kapsam disi girdiler baglama alinmadi.".into());
        }
        if paths.len() > MAX_HINT_ENTRIES {
            lines.push("[HEDEF ONIZLEMESI KISALTILDI]".into());
        }
    }
    bounded_hint(&lines.join("\n"))
}

fn bounded_hint(text: &str) -> String {
    const MARKER: &str = "\n[BAGLAM ONERISI KISALTILDI]";
    if text.chars().count() <= MAX_HINT_CHARS {
        return text.to_owned();
    }
    let mut output: String = text
        .chars()
        .take(MAX_HINT_CHARS - MARKER.chars().count())
        .collect();
    output.push_str(MARKER);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "fiko_context_{}_{}_{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("src")).unwrap();
            Self(root.canonicalize().unwrap())
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
                    .starts_with("fiko_context_")
            );
            for delay in [0, 25, 50, 100, 200] {
                if delay > 0 {
                    std::thread::sleep(Duration::from_millis(delay));
                }
                match fs::remove_dir_all(&self.0) {
                    Ok(()) => return,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                    Err(_) => {}
                }
            }
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn baglam_butcesi_red_ve_tasmada_degismemeli() {
        let mut budget = ContextBudget::default();
        assert_eq!(budget.remaining(), MAX_CONTEXT_BYTES);
        budget.admit(MAX_CONTEXT_BYTES).unwrap();
        assert_eq!(budget.used(), 0, "On kontrol rezervasyon degildir");
        budget.record(40_000).unwrap();
        assert_eq!(budget.remaining(), 24_000);
        assert!(
            budget
                .record(usize::MAX)
                .unwrap_err()
                .contains("BAGLAM_SINIRI")
        );
        assert_eq!(budget.used(), 40_000);
        budget.record(24_000).unwrap();
        assert!(budget.admit(1).is_err());
        assert!(budget.record(1).is_err());
        assert_eq!(budget.used(), MAX_CONTEXT_BYTES);
        budget.record(0).unwrap();
    }

    #[test]
    fn buyuk_dosya_onerisi_icerigi_okumadan_dar_aralik_istemeli() {
        let project = Project::new();
        fs::File::create(project.0.join("src/large.rs"))
            .unwrap()
            .set_len(100_000)
            .unwrap();
        let note = hint(&project.0, &["src/large.rs".into()]);
        assert!(note.contains("100000 byte"));
        assert!(note.contains("search_in_file") && note.contains("read_lines"));
        assert!(!note.contains("read_file"));
        assert!(note.chars().count() <= MAX_HINT_CHARS);
        assert!(!project.0.join("target").exists());
    }

    #[test]
    fn oneriler_hassas_ve_dis_kok_icerigini_aciga_cikarmamali() {
        let project = Project::new();
        fs::write(project.0.join(".env"), "SECRET_PAYLOAD=1").unwrap();
        fs::write(project.0.join("src/secret.pem"), "PRIVATE_KEY_PAYLOAD").unwrap();
        let note = hint(
            &project.0,
            &[
                ".env".into(),
                "src/secret.pem".into(),
                "../secret.txt".into(),
                "src/main_v05_backup.txt".into(),
            ],
        );
        for secret in [
            "SECRET_PAYLOAD",
            "PRIVATE_KEY_PAYLOAD",
            ".env",
            "secret.pem",
            "main_v05_backup",
        ] {
            assert!(!note.contains(secret), "{note}");
        }
        assert!(note.contains("baglama alinmadi"));
        assert!(!project.0.join("target").exists());
    }

    #[test]
    fn kesif_onerisi_sirali_sinirli_ve_hassas_girdilerden_arindirilmis_olmali() {
        let project = Project::new();
        for index in 0..30 {
            fs::write(
                project.0.join(format!("src/file_{index:02}.rs")),
                "SOURCE_BODY_MUST_NOT_APPEAR",
            )
            .unwrap();
        }
        fs::write(project.0.join("src/.secret"), "PRIVATE").unwrap();
        fs::create_dir(project.0.join("src/target")).unwrap();
        let note = hint(&project.0, &[]);
        assert_eq!(note.matches("[FILE]").count(), MAX_HINT_ENTRIES);
        assert!(note.contains("file_00.rs"));
        assert!(!note.contains("file_12.rs") && !note.contains(".secret"));
        assert!(!note.contains("[DIR] target"));
        assert!(!note.contains("SOURCE_BODY_MUST_NOT_APPEAR"));
        assert!(note.chars().count() <= MAX_HINT_CHARS);
    }

    #[test]
    fn eksik_hedef_ve_kisa_dosya_bilgisi_yetki_ve_icerik_uretmemeli() {
        let project = Project::new();
        fs::write(
            project.0.join("src/math.rs"),
            "pub fn double(v: i32) -> i32 { v * 2 }\n",
        )
        .unwrap();
        let note = hint(
            &project.0,
            &[
                "./src/math.rs".into(),
                "src\\math.rs".into(),
                "src/new.rs".into(),
            ],
        );
        assert_eq!(note.matches("src/math.rs:").count(), 1);
        assert!(note.contains("src/new.rs: yeni dosya hedefi"));
        assert!(note.contains("read_file"));
        assert!(!note.contains("pub fn double"));
        assert!(!project.0.join("src/new.rs").exists());
    }

    #[test]
    fn yol_normalizasyonu_guvenlik_kontrolunu_gevmemeli() {
        assert_eq!(normalize_path("./src/./Math.rs").unwrap(), "src/Math.rs");
        assert_eq!(normalize_path("src\\Math.rs").unwrap(), "src/Math.rs");
        for path in [
            "../src/math.rs",
            "src/../../math.rs",
            "C:\\secret.rs",
            "src/file.rs:secret",
            ".env",
            "src/private.key",
        ] {
            assert!(normalize_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn unicode_uzun_hedef_listesi_karakter_butcesini_asmamali() {
        let project = Project::new();
        let paths: Vec<_> = (0..16)
            .map(|index| format!("src/{}_{}.rs", "ş".repeat(80), index))
            .collect();
        for path in &paths {
            fs::write(project.0.join(path), "x").unwrap();
        }
        let note = hint(&project.0, &paths);
        assert!(note.chars().count() <= MAX_HINT_CHARS);
        assert!(note.contains("KISALTILDI"));
    }

    #[test]
    fn symlink_ve_junction_hedefleri_oneriye_girmemeli() {
        let project = Project::new();
        fs::create_dir(project.0.join("outside")).unwrap();
        fs::write(project.0.join("outside/private.rs"), "PRIVATE_SOURCE").unwrap();
        let link = project.0.join("src/link");
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(project.0.join("outside"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "Teste ait junction olusturulamadi");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(project.0.join("outside"), &link).unwrap();
        // Bu denemede proje koku src'dir; junction'in hedefi kok disindadir.
        let note = hint(&project.0.join("src"), &["link/private.rs".into()]);
        assert!(
            !note.contains("private.rs") && !note.contains("PRIVATE_SOURCE"),
            "{note}"
        );
        #[cfg(windows)]
        fs::remove_dir(&link).unwrap();
        #[cfg(unix)]
        fs::remove_file(&link).unwrap();
    }
}
