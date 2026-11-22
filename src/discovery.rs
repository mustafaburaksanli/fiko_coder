//! Sinirli workspace okuma ve hedef cozumleme. Bu kontroller bir OS sandbox'i
//! veya dis sureclere karsi atomik bir dosya sistemi kilidi saglamaz.

use std::fs::{self, File, Metadata};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

pub const MAX_SEARCH_RESULTS: usize = 20;
pub const MAX_SEARCH_FILES: usize = 150;
const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 4 * 1024;
const MAX_OUTPUT_BYTES: usize = 16_000;
const READ_LINES_METADATA_BYTES: usize = 512;
const MAX_DIRECTORY_ENTRIES: usize = 2_000;

/// Gizli/hedef/anahtar dosyalari ile Windows ADS ve isim takma yollarini engeller.
pub fn check_relative_path(relative: &str) -> Result<(), String> {
    if relative.trim().is_empty() || relative.chars().any(char::is_control) {
        return Err("Dosya veya klasor yolu bos ya da kontrol karakterli olamaz.".into());
    }
    let normalized = relative.replace('\\', "/");
    if normalized.contains(':') {
        return Err("Surucu veya alternatif veri akisi yolu yasak.".into());
    }
    for component in Path::new(&normalized).components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err("Mutlak veya ust klasore cikan yol yasak.".into());
            }
            Component::Normal(name) => {
                let name = name.to_str().ok_or("Dosya yolu UTF-8 olmali.")?;
                let lower = name.to_ascii_lowercase();
                let base = lower.split('.').next().unwrap_or("");
                let reserved = ["con", "prn", "aux", "nul"].contains(&base)
                    || (base.len() == 4
                        && (base.starts_with("com") || base.starts_with("lpt"))
                        && matches!(base.as_bytes()[3], b'1'..=b'9'));
                let sensitive = [
                    "credentials",
                    "credentials.json",
                    "secrets",
                    "secrets.json",
                    "id_rsa",
                    "id_ed25519",
                    "main_v05_backup.txt",
                ]
                .contains(&lower.as_str())
                    || ["pem", "key", "p12", "pfx", "keystore"]
                        .contains(&lower.rsplit('.').next().unwrap_or(""));
                if lower.starts_with('.')
                    || ["target", "node_modules", "__pycache__"].contains(&lower.as_str())
                    || name.ends_with(['.', ' '])
                    || name.contains(['*', '?', '"', '<', '>', '|'])
                    || reserved
                    || sensitive
                {
                    return Err("Gizli, hassas ya da korunan klasor/dosya yasak.".into());
                }
            }
            Component::CurDir => {}
        }
    }
    Ok(())
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junction ve diger reparse point turleri de kapsam disidir.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn resolve(root: &Path, relative: &str, allow_missing: bool) -> Result<PathBuf, String> {
    check_relative_path(relative)?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("Proje koku okunamadi: {e}"))?;
    if !root.is_dir() {
        return Err("Proje koku bir dizin olmali.".into());
    }
    let normalized = relative.replace('\\', "/");
    let components: Vec<_> = Path::new(&normalized)
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect();
    let mut full = root.clone();
    for (index, component) in components.iter().enumerate() {
        full.push(component.as_os_str());
        match fs::symlink_metadata(&full) {
            Ok(metadata) if is_link(&metadata) => {
                return Err("Symlink/reparse point uzerinden erisim yasak.".into());
            }
            Ok(metadata) if index + 1 < components.len() && !metadata.is_dir() => {
                return Err("Hedefin ebeveyni mevcut bir dizin olmali.".into());
            }
            Ok(_) => {}
            Err(error)
                if allow_missing
                    && index + 1 == components.len()
                    && error.kind() == std::io::ErrorKind::NotFound =>
            {
                // Yalniz dosya yeni olabilir; ebeveyn her zaman mevcut olmalidir.
                let parent = full
                    .parent()
                    .ok_or("Hedef ebeveyni yok.")?
                    .canonicalize()
                    .map_err(|e| e.to_string())?;
                if !parent.starts_with(&root) || !parent.is_dir() {
                    return Err("Proje disina erisim yasak.".into());
                }
                let resolved_parent = parent.strip_prefix(&root).map_err(|e| e.to_string())?;
                if !resolved_parent.as_os_str().is_empty() {
                    check_relative_path(&resolved_parent.to_string_lossy())?;
                }
                return Ok(parent.join(full.file_name().ok_or("Hedef dosya adi yok.")?));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("Dosya bulunamadi: {error}"));
            }
            Err(error) => return Err(format!("Hedef okunamadi: {error}")),
        }
    }
    let full = full.canonicalize().map_err(|e| e.to_string())?;
    let relative = full
        .strip_prefix(&root)
        .map_err(|_| "Proje disina erisim yasak.")?;
    if !relative.as_os_str().is_empty() {
        check_relative_path(&relative.to_string_lossy())?;
    }
    Ok(full)
}

pub fn resolve_existing(root: &Path, relative: &str) -> Result<PathBuf, String> {
    resolve(root, relative, false)
}

pub fn resolve_target(root: &Path, relative: &str) -> Result<PathBuf, String> {
    resolve(root, relative, true)
}

fn open_source(root: &Path, relative: &str) -> Result<File, String> {
    let full = resolve_existing(root, relative)?;
    if !fs::metadata(&full)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Bu yol normal bir dosya olmali.".into());
    }
    let file = File::open(full).map_err(|e| format!("Okuma hatasi: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Bu yol bir dosya degil.".into());
    }
    Ok(file)
}

pub fn read_file(root: &Path, relative: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    open_source(root, relative)?
        .take(64_001)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 64_000 {
        return Err("Dosya 64 KB sinirini asiyor; read_lines ile sinirli aralik okuyun.".into());
    }
    String::from_utf8(bytes).map_err(|e| format!("Dosya UTF-8 metin degil: {e}"))
}

struct Lines {
    reader: BufReader<File>,
    scanned: usize,
    limit: usize,
}

impl Lines {
    fn new(file: File, limit: usize) -> Self {
        Self {
            reader: BufReader::new(file),
            scanned: 0,
            limit,
        }
    }

    /// Uzun satirin yalnizca ilk MAX_LINE_BYTES byte'i bellekte tutulur.
    fn next(&mut self) -> Result<Option<(String, bool)>, String> {
        let mut line = Vec::new();
        let mut truncated = false;
        let mut saw_bytes = false;
        loop {
            let buffer = self.reader.fill_buf().map_err(|e| e.to_string())?;
            if buffer.is_empty() {
                if !saw_bytes {
                    return Ok(None);
                }
                break;
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(buffer.len(), |index| index + 1);
            if consumed > self.limit.saturating_sub(self.scanned) {
                return Err(
                    "[TARAMA SINIRI] 8 MiB okuma butcesi asildi; sonuc tamamlanmadi.".into(),
                );
            }
            let text_len = newline.unwrap_or(consumed);
            let retained = text_len.min(MAX_LINE_BYTES.saturating_sub(line.len()));
            line.extend_from_slice(&buffer[..retained]);
            truncated |= retained < text_len;
            saw_bytes = true;
            self.scanned += consumed;
            self.reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.contains(&0) {
            return Err("Dosya ikili veri iceriyor.".into());
        }
        let text = if truncated {
            String::from_utf8_lossy(&line).into_owned()
        } else {
            String::from_utf8(line).map_err(|e| format!("Dosya UTF-8 metin degil: {e}"))?
        };
        Ok(Some((text, truncated)))
    }
}

fn line_range_error(
    code: &str,
    message: &str,
    start: usize,
    end: usize,
    total_lines: Option<usize>,
) -> String {
    let suggested_range = total_lines.filter(|total| *total > 0).map(|total| {
        serde_json::json!({
            "start_line": if start <= total { start } else { total.saturating_sub(59).max(1) },
            "end_line": total,
        })
    });
    serde_json::json!({
        "status":"error", "error":code, "message":message,
        "requested_range":{"start_line":start,"end_line":end},
        "returned_range":null, "total_lines":total_lines,
        "suggested_range":suggested_range, "range_adjusted":false,
    })
    .to_string()
}

pub fn read_lines(root: &Path, relative: &str, start: usize, end: usize) -> Result<String, String> {
    if start == 0 || end < start || end - start >= 60 {
        return Err(line_range_error(
            "invalid_range",
            "Satir araligi 1 tabanli olmali ve en fazla 60 satir icermeli.",
            start,
            end,
            None,
        ));
    }
    let file = open_source(root, relative)?;
    let small_file = file.metadata().map_err(|error| error.to_string())?.len() <= 64_000;
    let mut lines = Lines::new(file, MAX_SCAN_BYTES);
    let mut output = String::new();
    let mut returned_end = start;
    let mut output_truncated = false;
    let mut line_truncated = false;
    for number in 1..=end {
        let Some((text, truncated)) = lines
            .next()
            .map_err(|message| line_range_error("read_incomplete", &message, start, end, None))?
        else {
            return Err(line_range_error(
                "range_exceeds_eof",
                "Istenen aralik dosyanin sonunu asiyor; suggested_range ile yeniden okuyun.",
                start,
                end,
                Some(number - 1),
            ));
        };
        if number >= start && !output_truncated {
            line_truncated |= truncated;
            let suffix = if truncated {
                " [SATIR KISALTILDI; ilk 4096 byte]"
            } else {
                ""
            };
            let entry = format!("{number:>4} | {text}{suffix}");
            if output.len() + entry.len() + 1 > MAX_OUTPUT_BYTES - READ_LINES_METADATA_BYTES - 100 {
                output.push_str("\n[CIKTI KISALTILDI; kalan satirlar icin daha dar aralik okuyun]");
                output_truncated = true;
                // Istenen araligin kalani EOF'yi asiyorsa kisaltilmis ciktiyi
                // basarili okuma olarak sunma. Tarama yine 8 MiB ile sinirli.
                continue;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&entry);
            returned_end = number;
        }
    }
    // Yalniz kucuk kaynakta ayni reader ile kalan satirlari say. Bu govdeler
    // modele verilmez. Dosya buyurse/okunamazsa toplam bilinmez; istenen ve
    // basariyla okunan aralik korunur. Buyuk dosyaya tam tarama eklenmez.
    let mut total_lines = None;
    if small_file && lines.scanned <= 64_000 {
        lines.limit = 64_000;
        let mut count = end;
        loop {
            match lines.next() {
                Ok(Some(_)) => count += 1,
                Ok(None) => {
                    total_lines = Some(count);
                    break;
                }
                Err(_) => break,
            }
        }
    }
    let metadata = serde_json::json!({
        "status":"ok", "requested_range":{"start_line":start,"end_line":end},
        "returned_range":{"start_line":start,"end_line":returned_end},
        "total_lines":total_lines,
        "total_lines_source": if total_lines.is_some() { "bounded_full_scan" } else { "not_fully_scanned" },
        "range_adjusted":false, "output_truncated":output_truncated,
        "line_truncated":line_truncated,
    });
    Ok(format!("[READ_LINES] {metadata}\n{output}"))
}

fn entries(
    directory: &Path,
    visited: &mut usize,
) -> Result<(Vec<(String, Metadata)>, bool), String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        if *visited >= MAX_DIRECTORY_ENTRIES {
            entries.sort_by(|left: &(String, Metadata), right| left.0.cmp(&right.0));
            return Ok((entries, true));
        }
        *visited += 1;
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if check_relative_path(&name).is_err() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if !is_link(&metadata) && (metadata.is_file() || metadata.is_dir()) {
            entries.push((name, metadata));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok((entries, false))
}

pub fn list_files(root: &Path, relative: &str) -> Result<String, String> {
    let directory = resolve_existing(root, relative)?;
    if !directory.is_dir() {
        return Err("Bu yol bir klasor degil.".into());
    }
    let (entries, limited) = entries(&directory, &mut 0)?;
    let truncated = limited || entries.len() > 100;
    let mut output = entries
        .into_iter()
        .take(100)
        .map(|(name, metadata)| {
            format!(
                "{} {name}",
                if metadata.is_dir() { "[DIR]" } else { "[FILE]" }
            )
        })
        .collect::<Vec<_>>();
    if truncated {
        output.push("[LISTELEME KISALTILDI; en fazla 100 sonuc / 2000 girdi]".into());
    }
    Ok(bounded_output(output))
}

fn check_query(query: &str) -> Result<(), String> {
    if query.trim().is_empty() || query.len() > 128 || query.contains(['\n', '\r', '\0']) {
        return Err("Arama sorgusu 1-128 byte ve tek satir olmali.".into());
    }
    Ok(())
}

fn search_file(
    root: &Path,
    relative: &str,
    query: &str,
    budget: &mut usize,
    hits: &mut Vec<String>,
) -> Result<bool, String> {
    let mut lines = Lines::new(open_source(root, relative)?, *budget);
    let mut number = 0;
    let mut truncated = false;
    let result = (|| {
        while let Some((text, clipped)) = lines.next()? {
            number += 1;
            truncated |= clipped;
            if text.contains(query) {
                let excerpt: String = text.chars().take(180).collect();
                hits.push(format!("{relative}:{number}: {excerpt}"));
                if hits.len() == MAX_SEARCH_RESULTS {
                    return Ok(true);
                }
            }
        }
        Ok(truncated)
    })();
    *budget = budget.saturating_sub(lines.scanned);
    result
}

pub fn search_in_file(root: &Path, relative: &str, query: &str) -> Result<String, String> {
    check_query(query)?;
    let mut hits = Vec::new();
    let mut budget = MAX_SCAN_BYTES;
    let truncated = search_file(root, relative, query, &mut budget, &mut hits)?;
    if hits.is_empty() {
        hits.push("Eslesen metin bulunamadi.".into());
    }
    if truncated {
        hits.push("[ARAMA KISALTILDI; satir veya sonuc siniri]".into());
    }
    Ok(bounded_output(hits))
}

fn bounded_output(lines: Vec<String>) -> String {
    let mut output = lines.join("\n");
    if output.len() > MAX_OUTPUT_BYTES {
        let marker = "\n[CIKTI KISALTILDI; 16000 byte siniri]";
        let mut end = MAX_OUTPUT_BYTES - marker.len();
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str(marker);
    }
    output
}

pub fn search_code(root: &Path, query: &str) -> Result<String, String> {
    check_query(query)?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mut pending = vec![(resolve_existing(&root, "src")?, 0usize)];
    let mut hits = Vec::new();
    let mut visited = 0;
    let mut scanned = 0;
    let mut budget = MAX_SCAN_BYTES;
    let mut limited = false;
    'folders: while let Some((folder, depth)) = pending.pop() {
        let (children, entry_limit) = entries(&folder, &mut visited)?;
        limited |= entry_limit;
        for (name, metadata) in children {
            let path = folder.join(name);
            if metadata.is_dir() {
                if depth < 5 {
                    pending.push((path, depth + 1));
                } else {
                    limited = true;
                }
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                if scanned == MAX_SEARCH_FILES || budget == 0 {
                    limited = true;
                    break 'folders;
                }
                scanned += 1;
                let relative = path
                    .strip_prefix(&root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy();
                match search_file(&root, &relative, query, &mut budget, &mut hits) {
                    Ok(clipped) => limited |= clipped,
                    Err(_) => limited = true,
                }
                if hits.len() == MAX_SEARCH_RESULTS {
                    limited = true;
                    break 'folders;
                }
            }
        }
        if entry_limit {
            break;
        }
    }
    if hits.is_empty() {
        hits.push("Eslesen Rust kodu bulunamadi.".into());
    }
    if limited {
        hits.push("[ARAMA KISALTILDI; dosya/girdi/byte/satir veya sonuc siniri]".into());
    }
    Ok(bounded_output(hits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Project {
        base: PathBuf,
        root: PathBuf,
    }

    impl Project {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!(
                "fiko_discovery_{}_{}_{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let root = base.join("root");
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir(base.join("outside")).unwrap();
            Self {
                base: base.canonicalize().unwrap(),
                root: root.canonicalize().unwrap(),
            }
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            assert!(
                self.base
                    .starts_with(std::env::temp_dir().canonicalize().unwrap())
            );
            assert!(
                self.base
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("fiko_discovery_")
            );
            fs::remove_dir_all(&self.base).unwrap();
        }
    }

    fn line_response(output: &str) -> (serde_json::Value, &str) {
        let (header, content) = output.split_once('\n').unwrap();
        let metadata: serde_json::Value =
            serde_json::from_str(header.strip_prefix("[READ_LINES] ").unwrap()).unwrap();
        assert_eq!(metadata["status"], "ok");
        (metadata, content)
    }

    #[test]
    fn eof_aralik_hatasi_dogru_toplam_ve_guvenli_kurtarma_onermeli() {
        let project = Project::new();
        let source = (1..=24)
            .map(|line| format!("source {line}\n"))
            .collect::<String>();
        fs::write(project.root.join("src/range.rs"), source).unwrap();
        let error: serde_json::Value =
            serde_json::from_str(&read_lines(&project.root, "src/range.rs", 21, 30).unwrap_err())
                .unwrap();
        assert_eq!(error["status"], "error");
        assert_eq!(error["error"], "range_exceeds_eof");
        assert_eq!(
            error["requested_range"],
            serde_json::json!({"start_line":21,"end_line":30})
        );
        assert!(error["returned_range"].is_null());
        assert_eq!(error["total_lines"], 24);
        assert_eq!(
            error["suggested_range"],
            serde_json::json!({"start_line":21,"end_line":24})
        );
        assert_eq!(error["range_adjusted"], false);
        let recovered = read_lines(&project.root, "src/range.rs", 21, 24).unwrap();
        let (metadata, content) = line_response(&recovered);
        assert_eq!(metadata["total_lines"], 24);
        assert_eq!(metadata["total_lines_source"], "bounded_full_scan");
        assert_eq!(
            metadata["returned_range"],
            serde_json::json!({"start_line":21,"end_line":24})
        );
        assert_eq!(
            content,
            "  21 | source 21\n  22 | source 22\n  23 | source 23\n  24 | source 24"
        );
        assert_eq!(metadata["output_truncated"], false);
    }

    #[test]
    fn tamamen_aralik_disi_istek_son_en_fazla_altmis_satiri_onermeli() {
        let project = Project::new();
        fs::write(project.root.join("src/range.rs"), "line\n".repeat(100)).unwrap();
        let error: serde_json::Value =
            serde_json::from_str(&read_lines(&project.root, "src/range.rs", 200, 210).unwrap_err())
                .unwrap();
        assert_eq!(error["total_lines"], 100);
        assert_eq!(
            error["suggested_range"],
            serde_json::json!({"start_line":41,"end_line":100})
        );
        let recovered = read_lines(&project.root, "src/range.rs", 41, 100).unwrap();
        let (metadata, content) = line_response(&recovered);
        assert_eq!(metadata["returned_range"], error["suggested_range"]);
        assert_eq!(content.lines().count(), 60);
    }

    #[test]
    fn gecersiz_bicim_ve_bos_dosya_olmayan_satirlari_onermemeli() {
        let project = Project::new();
        fs::write(project.root.join("src/empty.rs"), "").unwrap();
        for (start, end) in [(0, 1), (2, 1), (1, 61)] {
            let error: serde_json::Value = serde_json::from_str(
                &read_lines(&project.root, "src/empty.rs", start, end).unwrap_err(),
            )
            .unwrap();
            assert_eq!(error["error"], "invalid_range");
            assert!(error["total_lines"].is_null());
            assert!(error["suggested_range"].is_null());
        }
        let error: serde_json::Value =
            serde_json::from_str(&read_lines(&project.root, "src/empty.rs", 1, 2).unwrap_err())
                .unwrap();
        assert_eq!(error["error"], "range_exceeds_eof");
        assert_eq!(error["total_lines"], 0);
        assert!(error["suggested_range"].is_null());
    }

    #[test]
    fn karisik_satir_sonu_ve_son_yeni_satirsiz_icerik_dogru_sayilmali() {
        let project = Project::new();
        fs::write(project.root.join("src/mixed.rs"), "first\r\nsecond\nthird").unwrap();
        let output = read_lines(&project.root, "src/mixed.rs", 1, 2).unwrap();
        let (metadata, content) = line_response(&output);
        assert_eq!(metadata["total_lines"], 3);
        assert_eq!(content, "   1 | first\n   2 | second");
        let output = read_lines(&project.root, "src/mixed.rs", 3, 3).unwrap();
        let (metadata, content) = line_response(&output);
        assert_eq!(metadata["total_lines"], 3);
        assert_eq!(content, "   3 | third");
    }

    #[test]
    fn buyuk_dosyaya_toplam_icin_ek_tarama_yapilmamali() {
        let project = Project::new();
        let mut bytes = b"head\n".to_vec();
        bytes.extend(vec![b'x'; MAX_SCAN_BYTES + 1]);
        fs::write(project.root.join("src/budget.rs"), bytes).unwrap();
        let output = read_lines(&project.root, "src/budget.rs", 1, 1).unwrap();
        let (metadata, content) = line_response(&output);
        assert!(metadata["total_lines"].is_null());
        assert_eq!(metadata["total_lines_source"], "not_fully_scanned");
        assert_eq!(content, "   1 | head");
        let error: serde_json::Value =
            serde_json::from_str(&read_lines(&project.root, "src/budget.rs", 2, 2).unwrap_err())
                .unwrap();
        assert_eq!(error["error"], "read_incomplete");
        assert!(error["message"].as_str().unwrap().contains("TARAMA SINIRI"));
        assert!(error["total_lines"].is_null());
        assert!(error["suggested_range"].is_null());
    }

    #[test]
    fn toplam_sayilamayan_kucuk_dosyanin_gecerli_araligi_korunmali() {
        let project = Project::new();
        fs::write(project.root.join("src/tail.rs"), b"head\n\xff").unwrap();
        let output = read_lines(&project.root, "src/tail.rs", 1, 1).unwrap();
        let (metadata, content) = line_response(&output);
        assert_eq!(content, "   1 | head");
        assert_eq!(
            metadata["returned_range"],
            serde_json::json!({"start_line":1,"end_line":1})
        );
        assert!(metadata["total_lines"].is_null());
        assert_eq!(metadata["total_lines_source"], "not_fully_scanned");
    }

    #[test]
    fn cikti_kisalmasi_gecersiz_eof_araligini_basarili_yapmamali() {
        let project = Project::new();
        fs::write(
            project.root.join("src/clipped.rs"),
            format!("{}\n", "x".repeat(3_000)).repeat(10),
        )
        .unwrap();
        let error: serde_json::Value =
            serde_json::from_str(&read_lines(&project.root, "src/clipped.rs", 1, 30).unwrap_err())
                .unwrap();
        assert_eq!(error["error"], "range_exceeds_eof");
        assert_eq!(error["total_lines"], 10);
        assert!(error["returned_range"].is_null());
        let output = read_lines(&project.root, "src/clipped.rs", 1, 10).unwrap();
        let (metadata, content) = line_response(&output);
        assert_eq!(
            metadata["requested_range"],
            serde_json::json!({"start_line":1,"end_line":10})
        );
        assert_eq!(
            metadata["returned_range"],
            serde_json::json!({"start_line":1,"end_line":5})
        );
        assert_eq!(metadata["output_truncated"], true);
        assert_eq!(metadata["total_lines"], 10);
        assert!(content.contains("CIKTI KISALTILDI"));
        assert!(output.len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn paths_traversal_hassas_dosya_ve_windows_takma_adlarini_reddetmeli() {
        for path in [
            "",
            "../outside",
            "src/../outside",
            "src\\..\\outside",
            "/tmp/outside",
            "C:\\data",
            "src/code.rs:stream",
            ".env",
            ".git/config",
            "target/code.rs",
            ".fiko_backups/source.bak",
            "src/main_v05_backup.txt",
            "private.pem",
            "credentials.json",
            "src/CON.rs",
            "src/nul",
            "src/lpt1.txt",
            "src/code.rs.",
            "src/code.rs ",
            "src/line\n.rs",
        ] {
            assert!(check_relative_path(path).is_err(), "{path:?}");
        }
        for path in [
            ".",
            "src/main.rs",
            "./src/main.rs",
            "src\\main.rs",
            "Cargo.toml",
        ] {
            assert!(check_relative_path(path).is_ok(), "{path}");
        }
    }

    #[test]
    fn yeni_hedef_mevcut_guvenli_ebeveynle_sinirli_olmali() {
        let project = Project::new();
        let expected = project.root.join("src/new.rs");
        assert_eq!(
            resolve_target(&project.root, "src/new.rs").unwrap(),
            expected
        );
        assert_eq!(
            resolve_target(&project.root, "src\\new.rs").unwrap(),
            expected
        );
        assert!(!expected.exists());
        assert!(resolve_target(&project.root, "missing/new.rs").is_err());
        assert!(resolve_existing(&project.root, "src/new.rs").is_err());
        fs::write(&expected, "text").unwrap();
        assert_eq!(
            resolve_existing(&project.root, "src/new.rs").unwrap(),
            expected
        );
    }

    #[test]
    fn buyuk_dosya_satir_araligi_ve_dosya_aramasiyla_okunabilmeli() {
        let project = Project::new();
        let mut source = String::new();
        for line in 1..=10_000 {
            source.push_str(&format!("source line {line}\n"));
        }
        source.push_str("unique_late_token\n");
        fs::write(project.root.join("src/large.rs"), source).unwrap();
        assert!(
            read_file(&project.root, "src/large.rs")
                .unwrap_err()
                .contains("read_lines")
        );
        let lines = read_lines(&project.root, "src/large.rs", 8_000, 8_001).unwrap();
        let (metadata, content) = line_response(&lines);
        assert_eq!(content, "8000 | source line 8000\n8001 | source line 8001");
        assert_eq!(
            metadata["requested_range"],
            serde_json::json!({"start_line":8000,"end_line":8001})
        );
        assert_eq!(metadata["returned_range"], metadata["requested_range"]);
        assert!(metadata["total_lines"].is_null());
        assert_eq!(metadata["total_lines_source"], "not_fully_scanned");
        assert!(lines.len() < MAX_OUTPUT_BYTES);
        let hits = search_in_file(&project.root, "src/large.rs", "unique_late_token").unwrap();
        assert_eq!(hits, "src/large.rs:10001: unique_late_token");
        assert!(
            search_code(&project.root, "unique_late_token")
                .unwrap()
                .contains("large.rs:10001:")
        );
    }

    #[test]
    fn uzun_satir_ve_coklu_satir_ciktisi_acikca_sinirlanmali() {
        let project = Project::new();
        fs::write(
            project.root.join("src/long.rs"),
            format!("{}\nshort\n", "x".repeat(50_000)),
        )
        .unwrap();
        let output = read_lines(&project.root, "src/long.rs", 1, 2).unwrap();
        assert!(output.contains("SATIR KISALTILDI"));
        assert!(output.ends_with("   2 | short"));
        assert!(output.len() < MAX_OUTPUT_BYTES);
        assert!(
            search_in_file(&project.root, "src/long.rs", "missing")
                .unwrap()
                .contains("ARAMA KISALTILDI")
        );
        fs::write(
            project.root.join("src/many.rs"),
            format!("{}\n", "x".repeat(3_000)).repeat(60),
        )
        .unwrap();
        let output = read_lines(&project.root, "src/many.rs", 1, 60).unwrap();
        assert!(output.contains("CIKTI KISALTILDI"));
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        fs::write(
            project.root.join("src/huge.rs"),
            vec![b'x'; MAX_SCAN_BYTES + 1],
        )
        .unwrap();
        assert!(
            read_lines(&project.root, "src/huge.rs", 1, 1)
                .unwrap_err()
                .contains("TARAMA SINIRI")
        );
    }

    #[test]
    fn listeleme_hassas_ve_gereksiz_hedefleri_gizlemeli() {
        let project = Project::new();
        for directory in ["target", ".git", ".fiko_backups"] {
            fs::create_dir(project.root.join(directory)).unwrap();
        }
        fs::write(project.root.join(".env"), "secret").unwrap();
        fs::write(project.root.join("credentials.json"), "secret").unwrap();
        fs::write(
            project.root.join("src/main_v05_backup.txt"),
            "protected fixture",
        )
        .unwrap();
        fs::write(project.root.join("src/visible.rs"), "visible").unwrap();
        assert_eq!(list_files(&project.root, ".").unwrap(), "[DIR] src");
        assert_eq!(
            list_files(&project.root, "src").unwrap(),
            "[FILE] visible.rs"
        );
        assert!(read_file(&project.root, "src/main_v05_backup.txt").is_err());
        for index in 0..101 {
            fs::write(project.root.join(format!("file_{index:03}.txt")), "").unwrap();
        }
        let output = list_files(&project.root, ".").unwrap();
        assert!(output.contains("LISTELEME KISALTILDI"));
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with('[') && !line.contains("KISALTILDI"))
                .count(),
            100
        );
    }

    #[test]
    fn symlink_ve_reparse_ile_kok_disina_cikilamamali() {
        let project = Project::new();
        let outside = project.base.join("outside");
        fs::write(outside.join("code.rs"), "secret").unwrap();
        let link = project.root.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(resolve_existing(&project.root, "escape/code.rs").is_err());
        assert!(resolve_target(&project.root, "escape/new.rs").is_err());
        assert!(read_lines(&project.root, "escape/code.rs", 1, 1).is_err());
        assert!(!list_files(&project.root, ".").unwrap().contains("escape"));
        #[cfg(windows)]
        fs::remove_dir(&link).unwrap();
        #[cfg(unix)]
        fs::remove_file(&link).unwrap();
        assert_eq!(
            fs::read_to_string(outside.join("code.rs")).unwrap(),
            "secret"
        );
    }
}
