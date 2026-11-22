//! Dosya araclari, cargo calistirma, patch hazirlama, yedekleme ve geri alma.
//!
//! Bu modul v0.6'daki davranisi korur; v0.7 yalnizca `run_cargo` ile
//! hedef dizin / zaman asimi parametrelerini ve `tool_definitions`i ekler.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

pub const CARGO_TIMEOUT_SECS: u64 = 120;

// Workspace discovery sinirlari tek yerde uygulanir.
pub use crate::discovery::{
    MAX_SEARCH_FILES, MAX_SEARCH_RESULTS, check_relative_path, list_files, read_file, read_lines,
    search_code, search_in_file,
};

// ============================================================
// TOOL 5: CARGO CHECK / CARGO TEST
// ============================================================
/// Komut sonucu metinden degil, Rust'in gozledigi surec durumundan turetilir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Succeeded,
    Failed,
    ExecutionError,
    TimedOut,
    Interrupted,
}

impl ExecutionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::ExecutionError => "execution_error",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Surec ciktisindan ayri, yalniz Rust gozlemini tasiyan sinirli metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandObservation {
    pub command: String,
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
}

const MAX_COMMAND_REPORT_CHARS: usize = 10_000;
const OUTPUT_CLIP_MARKER: &str = "\n[CIKTI KISALTILDI; TAM CIKTI RUST BELLEGINDE]\n";

#[derive(Debug, Clone)]
pub struct TestResult {
    pub command: String,
    pub status: ExecutionStatus,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub output_truncated: bool,
    /// Eski dogrulama akisinin kullandigi alan; status ile birlikte uretilir.
    pub success: bool,
    /// Metadata korunur; uzun cikti acik bir kisaltma isaretiyle sunulur.
    pub report: String,
    /// Tam surec ciktisi. Modele giden kisaltilmis onizlemeden ayridir.
    pub stdout: String,
    pub stderr: String,
}

impl TestResult {
    pub fn observation(&self) -> CommandObservation {
        CommandObservation {
            command: self.command.clone(),
            status: self.status,
            exit_code: self.exit_code,
        }
    }

    pub fn from_output(command: String, output: std::process::Output) -> Self {
        let status = if output.status.success() {
            ExecutionStatus::Succeeded
        } else if output.status.code().is_some() {
            ExecutionStatus::Failed
        } else {
            ExecutionStatus::Interrupted
        };
        Self::new(
            command,
            status,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            None,
        )
    }

    pub fn execution_error(command: String, error: String) -> Self {
        Self::new(
            command,
            ExecutionStatus::ExecutionError,
            None,
            String::new(),
            String::new(),
            Some(error),
        )
    }

    pub fn timed_out(command: String) -> Self {
        Self::new(
            command,
            ExecutionStatus::TimedOut,
            None,
            String::new(),
            String::new(),
            Some("Komut zaman asimina ugradi; tamamlanma sonucu ve kesinti oncesi cikti mevcut degil.".into()),
        )
    }

    fn new(
        command: String,
        status: ExecutionStatus,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        error: Option<String>,
    ) -> Self {
        let mut result = Self {
            command,
            status,
            exit_code,
            error,
            output_truncated: false,
            success: status == ExecutionStatus::Succeeded,
            report: String::new(),
            stdout,
            stderr,
        };
        let (stdout, stderr, truncated) = result.output_preview();
        result.output_truncated = truncated;
        result.report = format!(
            "{}{}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
            result.report_header(),
            truncated
        );
        result
    }

    pub fn summary_line(&self) -> String {
        format!(
            "[RUST SUREC SONUCU] Komut: {} | Durum: {} | Cikis kodu: {} | Test sayilari: mevcut degil",
            self.command,
            self.status.label(),
            self.exit_code
                .map_or_else(|| "mevcut degil".into(), |code| code.to_string()),
        )
    }

    fn report_header(&self) -> String {
        format!(
            "{}\nYurutme hatasi: {}\nCikti kisaltildi: ",
            self.summary_line(),
            self.error.as_deref().unwrap_or("yok"),
        )
    }

    fn output_preview(&self) -> (String, String, bool) {
        // Metadata ve iki cikti etiketi icin yer ayrilir; onemli durum/exit
        // bilgisi stdout uzadigi icin kesilemez. Uc durum: cok uzun bir komut
        // veya OS hatasi metadata sinirini asarsa metadata yine tam korunur.
        let overhead =
            self.report_header().chars().count() + "false\nSTDOUT:\n\nSTDERR:\n".chars().count();
        let budget = MAX_COMMAND_REPORT_CHARS.saturating_sub(overhead);
        let out_len = self.stdout.chars().count();
        let err_len = self.stderr.chars().count();
        let err_budget = err_len.min(budget / 2);
        let out_budget = out_len.min(budget - err_budget);
        let err_budget = err_len.min(budget - out_budget);
        (
            clip_command_output(&self.stdout, out_budget),
            clip_command_output(&self.stderr, err_budget),
            out_len > out_budget || err_len > err_budget,
        )
    }

    pub fn tool_response(&self) -> Value {
        let (stdout, stderr, _) = self.output_preview();
        json!({
            "authority": "rust_process",
            "command": self.command,
            "status": self.status.label(),
            "exit_code": self.exit_code,
            "success": self.success,
            "test_counts": null,
            "test_counts_note": "Guvenilir test sayimi mevcut degil; sayilar stdout metninden turetilmedi.",
            "output_truncated": self.output_truncated,
            "stdout": stdout,
            "stderr": stderr,
            "error": self.error,
        })
    }
}

fn clip_command_output(text: &str, budget: usize) -> String {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= budget {
        return text.to_owned();
    }
    let marker_len = OUTPUT_CLIP_MARKER.chars().count();
    if budget < marker_len {
        return chars.into_iter().take(budget).collect();
    }
    let content_budget = budget - marker_len;
    let head = content_budget / 2;
    let tail = content_budget - head;
    format!(
        "{}{}{}",
        chars[..head].iter().collect::<String>(),
        OUTPUT_CLIP_MARKER,
        chars[chars.len() - tail..].iter().collect::<String>(),
    )
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
    if crate::verification::verification_kind(dir)? != crate::verification::VerificationKind::Cargo
    {
        return Err("Cargo dogrulamasi calistirilmadi: desteklenmeyen proje turu.".into());
    }
    let program = cargo_bin();
    let command_name = format!("{program} {mode}");
    let mut command = Command::new(program);
    command
        .arg(mode)
        .current_dir(dir)
        .env("CARGO_TERM_COLOR", "never");
    if let Some(target) = target_dir {
        command.env("CARGO_TARGET_DIR", target);
    }

    Ok(execute_command(command, command_name, limit).await)
}

async fn execute_command(mut command: Command, name: String, limit: Duration) -> TestResult {
    command.kill_on_drop(true);
    match timeout(limit, command.output()).await {
        Ok(Ok(output)) => TestResult::from_output(name, output),
        Ok(Err(error)) => {
            TestResult::execution_error(name, format!("Komut calistirilamadi: {error}"))
        }
        Err(_) => TestResult::timed_out(name),
    }
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
fn line_block_boundary(content: &str, needle: &str, start: usize) -> bool {
    let prefix = content[..start].rsplit('\n').next().unwrap_or("");
    let suffix = content[start + needle.len()..]
        .split('\n')
        .next()
        .unwrap_or("");
    let only_indent = prefix.chars().all(|c| c == ' ' || c == '\t' || c == '\r');
    let only_tail =
        needle.ends_with('\n') || suffix.chars().all(|c| c == ' ' || c == '\t' || c == '\r');
    only_indent && only_tail
}

pub fn exact_line_block_locations(content: &str, needle: &str) -> Vec<usize> {
    content
        .match_indices(needle)
        .filter_map(|(start, _)| line_block_boundary(content, needle, start).then_some(start))
        .collect()
}

/// Opt-in ve recovery, ortusen bloklari da ayri konum sayar. Strict API'nin
/// mevcut match_indices davranisi ve kabul kurali degistirilmez.
fn all_line_block_locations(content: &str, needle: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    if needle.is_empty() {
        return positions;
    }
    let mut offset = 0;
    while let Some(relative) = content[offset..].find(needle) {
        let start = offset + relative;
        if line_block_boundary(content, needle, start) {
            positions.push(start);
        }
        offset = start + content[start..].chars().next().unwrap().len_utf8();
    }
    positions
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PatchMatchMode {
    #[default]
    Exact,
    LineEndings,
}

impl PatchMatchMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::LineEndings => "line_endings",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PatchMatchOptions {
    pub mode: PatchMatchMode,
    pub expected_sha256: Option<String>,
}

fn match_option_error(field: &str, reason: &str) -> String {
    json!({"error":"PATCH_MATCH_OPTIONS","field":field,"reason":reason}).to_string()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl PatchMatchOptions {
    pub fn from_arguments(args: &Value) -> Result<Self, String> {
        let mode = match args.get("match_mode") {
            None => PatchMatchMode::Exact,
            Some(Value::String(value)) if value == "exact" => PatchMatchMode::Exact,
            Some(Value::String(value)) if value == "line_endings" => PatchMatchMode::LineEndings,
            _ => {
                return Err(match_option_error(
                    "match_mode",
                    "exact_or_line_endings_required",
                ));
            }
        };
        let expected_sha256 = match args.get("expected_sha256") {
            None => None,
            Some(Value::String(value)) if valid_sha256(value) => Some(value.to_ascii_lowercase()),
            _ => {
                return Err(match_option_error(
                    "expected_sha256",
                    "sha256_64_hex_required",
                ));
            }
        };
        let options = Self {
            mode,
            expected_sha256,
        };
        options.validate()?;
        Ok(options)
    }

    fn validate(&self) -> Result<(), String> {
        if self
            .expected_sha256
            .as_ref()
            .is_some_and(|value| !valid_sha256(value))
        {
            return Err(match_option_error(
                "expected_sha256",
                "sha256_64_hex_required",
            ));
        }
        if self.mode == PatchMatchMode::LineEndings && self.expected_sha256.is_none() {
            return Err(match_option_error(
                "expected_sha256",
                "required_for_line_endings",
            ));
        }
        Ok(())
    }
}

fn source_sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Byte araligi asil UTF-8 dosyaya aittir; end haric sinirdir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedBlock {
    pub start: usize,
    pub end: usize,
    pub source_sha256: String,
    pub range_sha256: String,
    pub mode: PatchMatchMode,
}

impl MatchedBlock {
    fn new(content: &str, start: usize, end: usize, mode: PatchMatchMode) -> Self {
        Self {
            start,
            end,
            source_sha256: source_sha256(content),
            range_sha256: source_sha256(&content[start..end]),
            mode,
        }
    }

    pub fn json(&self) -> Value {
        json!({"match_mode":self.mode.label(),"expected_sha256":self.source_sha256,
            "byte_range":{"start":self.start,"end":self.end},"range_sha256":self.range_sha256})
    }
}

/// Yalniz CRLF ciftini LF yapar. Her LF-gorunum byte siniri, orijinal byte
/// sinirina eslenir; Unicode byte'lari ve tek basina CR aynen kalir.
struct LfView {
    text: String,
    original_offsets: Vec<usize>,
}

impl LfView {
    fn new(content: &str) -> Self {
        let bytes = content.as_bytes();
        let mut normalized = Vec::with_capacity(bytes.len());
        let mut original_offsets = Vec::with_capacity(bytes.len() + 1);
        original_offsets.push(0);
        let mut position = 0;
        while position < bytes.len() {
            if bytes[position] == b'\r' && bytes.get(position + 1) == Some(&b'\n') {
                normalized.push(b'\n');
                position += 2;
            } else {
                normalized.push(bytes[position]);
                position += 1;
            }
            original_offsets.push(position);
        }
        Self {
            text: String::from_utf8(normalized).expect("CRLF silme UTF-8 sinirlarini degistirmez"),
            original_offsets,
        }
    }

    fn block(&self, content: &str, start: usize, len: usize) -> MatchedBlock {
        MatchedBlock::new(
            content,
            self.original_offsets[start],
            self.original_offsets[start + len],
            PatchMatchMode::LineEndings,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchMatchReason {
    NotFound,
    Ambiguous,
    PartialLine,
    LineEndingsMismatch,
}

impl PatchMatchReason {
    fn label(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::PartialLine => "partial_line",
            Self::LineEndingsMismatch => "line_endings_mismatch",
        }
    }

    fn next_action(self) -> &'static str {
        match self {
            Self::NotFound => "read_file_or_read_lines_then_copy_exact_old_text",
            Self::Ambiguous => "choose_a_larger_unique_exact_block",
            Self::PartialLine => "include_complete_line_or_block_with_punctuation",
            Self::LineEndingsMismatch => {
                "read_file_preserve_line_endings_then_resubmit_exact_old_text"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEndingMetadata {
    pub lf: usize,
    pub crlf: usize,
    pub bare_cr: usize,
    pub final_newline: bool,
}

impl LineEndingMetadata {
    fn of(text: &str) -> Self {
        let crlf = text
            .as_bytes()
            .windows(2)
            .filter(|pair| *pair == b"\r\n")
            .count();
        Self {
            lf: text.bytes().filter(|byte| *byte == b'\n').count() - crlf,
            crlf,
            bare_cr: text.bytes().filter(|byte| *byte == b'\r').count() - crlf,
            final_newline: text.ends_with('\n'),
        }
    }

    fn json(self) -> Value {
        json!({"lf":self.lf,"crlf":self.crlf,"bare_cr":self.bare_cr,"final_newline":self.final_newline})
    }
}

/// Kaynak govdesi, yol veya model girdisi tasimayan, sinirli eslesme tanisi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchMatchDiagnostic {
    pub reason: PatchMatchReason,
    /// Satir/blok sinirlarini da gecen, old_text ile birebir eslesen konum sayisi.
    pub match_count: usize,
    pub literal_match_count: usize,
    pub normalized_match_count: Option<usize>,
    pub total_lines: usize,
    pub file_line_endings: LineEndingMetadata,
    pub old_text_line_endings: LineEndingMetadata,
    pub mode: PatchMatchMode,
    pub recovery: Option<Box<MatchedBlock>>,
}

impl PatchMatchDiagnostic {
    pub fn json(&self) -> Value {
        let normalization_hint = (self.reason == PatchMatchReason::LineEndingsMismatch).then_some(
            "Yalniz CRLF/LF karsilastirmasi eslesti; normalizasyon veya patch uygulanmadi.",
        );
        let mut diagnostic = json!({
                "error":"PATCH_MATCH", "field":"old_text", "reason":self.reason.label(),
                "match_count":self.match_count, "literal_match_count":self.literal_match_count,
                "normalized_match_count":self.normalized_match_count, "total_lines":self.total_lines,
                "line_endings":{"file":self.file_line_endings.json(),"old_text":self.old_text_line_endings.json()},
                "normalization_hint":normalization_hint,"next_action":self.reason.next_action(),
                "message":"Eski metin tam bir satir/blok olarak dosyada tam bir kez bulunmali. Girintiyi atlayabilirsiniz; satir sonundaki ';' gibi karakterleri atlayamazsiniz."
        });
        if self.mode != PatchMatchMode::Exact {
            diagnostic["match_mode"] = json!(self.mode.label());
        }
        if let Some(recovery) = &self.recovery {
            diagnostic["recovery"] = recovery.json();
            diagnostic["next_action"] = json!(
                "Ayni old_text/new_text ile match_mode=line_endings ve recovery.expected_sha256 gonder; yeniden kaynak okumak gerekmez. Rust hash ve tum eslesmeleri yeniden denetler."
            );
        }
        diagnostic
    }

    /// Degisiklik dizisinin Rust tarafindan gozlenen, sifir tabanli indeksi.
    pub fn for_change(&self, change_index: usize) -> Value {
        let mut diagnostic = self.json();
        diagnostic["change_index"] = json!(change_index);
        diagnostic
    }
}

impl std::fmt::Display for PatchMatchDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.json())
    }
}

impl std::error::Error for PatchMatchDiagnostic {}

/// Iki patch yolu da ayni tam ve tek eslesme kuralini kullanir. LF kopyalari
/// yalnizca tani icindir; kabul edilen byte konumu her zaman asil metindendir.
pub fn unique_line_block_match(
    content: &str,
    old_text: &str,
) -> Result<usize, PatchMatchDiagnostic> {
    let matches = if old_text.is_empty() {
        Vec::new()
    } else {
        exact_line_block_locations(content, old_text)
    };
    if let [position] = matches.as_slice() {
        return Ok(*position);
    }
    let literal_match_count = if old_text.is_empty() {
        0
    } else {
        content.matches(old_text).count()
    };
    let mut normalized_match_count = None;
    let mut recovery = None;
    if !old_text.is_empty() && (content.contains("\r\n") || old_text.contains("\r\n")) {
        let view = LfView::new(content);
        let normalized_old = old_text.replace("\r\n", "\n");
        let normalized_matches = all_line_block_locations(&view.text, &normalized_old);
        if !normalized_matches.is_empty() {
            normalized_match_count = Some(normalized_matches.len());
        }
        if let [start] = normalized_matches.as_slice() {
            recovery = Some(Box::new(view.block(content, *start, normalized_old.len())));
        }
    }
    let reason = if matches.len() > 1 {
        PatchMatchReason::Ambiguous
    } else if literal_match_count > 0 {
        PatchMatchReason::PartialLine
    } else if normalized_match_count.is_some() {
        PatchMatchReason::LineEndingsMismatch
    } else {
        PatchMatchReason::NotFound
    };
    Err(PatchMatchDiagnostic {
        reason,
        match_count: matches.len(),
        literal_match_count,
        normalized_match_count,
        total_lines: content.lines().count(),
        file_line_endings: LineEndingMetadata::of(content),
        old_text_line_endings: LineEndingMetadata::of(old_text),
        mode: PatchMatchMode::Exact,
        recovery,
    })
}

/// Opt-in yol LF-gorunumundeki BUTUN tam blok eslesmelerini sayar. Bir exact
/// eslesme bulunmasi diger CRLF/LF eslesmelerinin goz ardi edilmesine yol acmaz.
pub fn match_line_block(
    content: &str,
    old_text: &str,
    options: &PatchMatchOptions,
) -> Result<MatchedBlock, String> {
    options.validate()?;
    if options
        .expected_sha256
        .as_ref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&source_sha256(content)))
    {
        return Err(json!({"error":"PATCH_MATCH","field":"expected_sha256","reason":"source_changed",
            "match_mode":options.mode.label(),"next_action":"read_current_source_and_build_new_candidate"}).to_string());
    }
    if options.mode == PatchMatchMode::Exact {
        let start =
            unique_line_block_match(content, old_text).map_err(|error| error.to_string())?;
        return Ok(MatchedBlock::new(
            content,
            start,
            start + old_text.len(),
            options.mode,
        ));
    }
    let view = LfView::new(content);
    let normalized_old = old_text.replace("\r\n", "\n");
    let matches = if normalized_old.is_empty() {
        Vec::new()
    } else {
        all_line_block_locations(&view.text, &normalized_old)
    };
    if let [start] = matches.as_slice() {
        return Ok(view.block(content, *start, normalized_old.len()));
    }
    let literal_match_count = if normalized_old.is_empty() {
        0
    } else {
        view.text.matches(&normalized_old).count()
    };
    let reason = if matches.len() > 1 {
        PatchMatchReason::Ambiguous
    } else if literal_match_count > 0 {
        PatchMatchReason::PartialLine
    } else {
        PatchMatchReason::NotFound
    };
    Err(PatchMatchDiagnostic {
        reason,
        match_count: matches.len(),
        literal_match_count,
        normalized_match_count: Some(matches.len()),
        total_lines: content.lines().count(),
        file_line_endings: LineEndingMetadata::of(content),
        old_text_line_endings: LineEndingMetadata::of(old_text),
        mode: options.mode,
        recovery: None,
    }
    .to_string())
}

pub fn replace_line_block(
    content: &str,
    old_text: &str,
    new_text: &str,
    options: &PatchMatchOptions,
) -> Result<String, String> {
    if old_text.is_empty() {
        return Err("Eski metin bos olamaz.".into());
    }
    if old_text == new_text {
        return Err("Eski ve yeni kod ayni.".into());
    }
    let matched = match_line_block(content, old_text, options)?;
    let mut updated = content.to_owned();
    updated.replace_range(matched.start..matched.end, new_text);
    if updated == content {
        return Err("Eski ve yeni kod ayni.".into());
    }
    Ok(updated)
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
    prepare_patch_with_options(
        root,
        relative,
        old_text,
        new_text,
        &PatchMatchOptions::default(),
    )
}

pub fn prepare_patch_with_options(
    root: &Path,
    relative: &str,
    old_text: &str,
    new_text: &str,
    options: &PatchMatchOptions,
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
    let full = crate::discovery::resolve_existing(root, relative)?;

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
    let updated = replace_line_block(&original, old_text, new_text, options)?;
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
    // Read-only preflight must not create lock/backup artifacts on stale input.
    crate::workspace_recovery::ensure_clear(root)?;
    let current = fs::read_to_string(&plan.path).map_err(|e| e.to_string())?;
    if current != plan.original {
        return Err("Dosya onay beklenirken degisti; patch iptal edildi.".into());
    }
    // Coordinate all FIKO writers and revalidate under the same lock.
    let _operation_lock = crate::workspace_recovery::try_lock_operation(root)?;
    crate::workspace_recovery::ensure_clear(root)?;
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
    let root = record
        .path
        .parent()
        .and_then(Path::parent)
        .ok_or("Patch hedefi bir src altinda olmali.")?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    // Read-only checks first; then recheck under the same lock used by apply.
    let current = fs::read_to_string(&record.path).map_err(|e| e.to_string())?;
    if current != record.updated {
        return Err("Dosya patch sonrasinda degismis; otomatik geri alma reddedildi.".into());
    }
    let _operation_lock = crate::workspace_recovery::try_lock_operation(&root)?;
    crate::workspace_recovery::ensure_clear(&root)?;
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
    let results = verify_patch_results(root).await;
    let success = results.len() == 2
        && results
            .iter()
            .all(|result| result.status == ExecutionStatus::Succeeded);
    let report = results
        .iter()
        .map(|result| result.report.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (success, report)
}

/// Eski metin API'sinin altinda gercek surec sonuclarini kaybetmez.
pub async fn verify_patch_results(root: &Path) -> Vec<TestResult> {
    let mut results = Vec::new();
    for mode in ["check", "test"] {
        let result = match run_tests(root, mode).await {
            Ok(result) => result,
            Err(error) => TestResult::execution_error(format!("cargo {mode}"), error),
        };
        let success = result.status == ExecutionStatus::Succeeded;
        results.push(result);
        if !success {
            break;
        }
    }
    results
}

// ============================================================
// OLLAMA ARAC TANIMLARI
// ============================================================
pub fn tool_definitions() -> Value {
    let match_mode_schema = json!({"type":"string","enum":["exact","line_endings"],
        "description":"Varsayilan exact. Yalniz Rust PATCH_MATCH recovery kaniti varsa line_endings ile CRLF/LF farkini acikca kabul et; expected_sha256 zorunludur. Bosluk veya noktalama esnetilmez."});
    let expected_sha256_schema = json!({"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-fA-F]{64}$",
        "description":"Rust recovery.expected_sha256: kaynak dosyanin TAM byte'larinin SHA256 ozeti. line_endings modunda zorunlu; hash degisirse aday reddedilir."});
    let mut definitions = json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Izinli UTF-8 dosyasini LF/CRLF ve dosya sonundaki satir sonunu degistirmeden oku. En fazla 64000 byte; buyuk dosyalarda read_lines kullan.",
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
                "description": "Bir dosyanin 1 tabanli en fazla 60 satirini sinirli oku. Satir numaralari ve kisaltma etiketleri old_text'e eklenmez; LF/CRLF bicimi bu gorunumden anlasilmaz.",
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
                        "match_mode": match_mode_schema,
                        "expected_sha256": expected_sha256_schema,
                        "mode": {"type": "string", "enum": ["check", "test"], "description": "Varsayilan check"}
                    }, "required": ["path", "old_text", "new_text"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Rust dosyasinda yalnizca TAM SATIR veya TAM SATIRLARDAN olusan kodu degistir. old_text satir sonundaki noktalama karakterlerini de icermeli; satir basi bosluklari istege bagli. KILIT: yalnizca verify_candidate ile dogrulanmis ve BIREBIR AYNI path/old_text/new_text uygulanabilir. Farkli, degistirilmis veya eskimis aday once gecici kopyada yeniden dogrulanir; izin yoksa reddedilir. Ardindan kullanici onayi, yedekleme, gercek projede cargo check + cargo test; hatada geri alma.",
                "parameters": {
                    "type": "object", "properties": {
                        "path": {"type": "string"},
                        "old_text": {"type": "string", "description": "Satirin/blokun tam eski metni (girinti disinda hicbir karakter atlama)"},
                        "new_text": {"type": "string", "description": "Yerine konacak tam yeni metin"},
                        "match_mode": match_mode_schema,
                        "expected_sha256": expected_sha256_schema
                    }, "required": ["path", "old_text", "new_text"]
                }
            }
        }
    ]);
    let changes = json!({
        "type": "array", "minItems": 1, "maxItems": 16,
        "description": "Istekte belirtilen tum degisiklik hedefleri; her dosya icin tek edit veya create. Tum hedefleri tek changes dizisine koy.",
        "items": {"oneOf": [
            {"type": "object", "properties": {
                "operation": {"type": "string", "const": "edit"},
                "path": {"type": "string"},
                "old_text": {"type": "string", "description": "Okunan tam eski satir/blok, en fazla 8000 byte. JSON string olarak kodla; cozulen deger kaynakla ayni olmali. LF/CRLF ve dosya sonunda satir sonu olup olmadigini koru. Rust tam eslesmeyi denetler."},
                "new_text": {"type": "string", "description": "Eski blok yerine gelecek tam metin, en fazla 8000 byte."},
                "match_mode": match_mode_schema,
                "expected_sha256": expected_sha256_schema
            }, "required": ["operation", "path", "old_text", "new_text"], "additionalProperties": false},
            {"type": "object", "properties": {
                "operation": {"type": "string", "const": "create"},
                "path": {"type": "string"},
                "content": {"type": "string", "description": "Yeni dosyanin tum UTF-8 icerigi, en fazla 8000 byte. Hedef dosya mevcut olmamali."}
            }, "required": ["operation", "path", "content"], "additionalProperties": false}
        ]}
    });
    let tools = definitions
        .as_array_mut()
        .expect("Sabit arac semasi bir dizidir.");
    tools.push(json!({"type": "function", "function": {
        "name": "search_in_file", "description": "Belirtilen izinli dosyada sinirli, satir numarali sabit metin aramasi. Buyuk dosyalar kontrollu taranir; limitler acikca belirtilir.",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "query": {"type": "string"}}, "required": ["path", "query"]}
    }}));
    for (name, description) in [
        (
            "verify_workspace",
            "Okunan tum hedefler icin tek changes dizisi hazirla ve verify_workspace arac cagrisi gonder. old_text satir sonlari dahil kaynakla eslesmeli. Rust adaylari gecici proje kopyasinda birlikte dogrular; Cargo kod calistirma izni gerekir. VERIFY_OK sonrasi ayni changes ile apply_workspace kullan. VERIFY_OK uygulama basarisi degildir.",
        ),
        (
            "apply_workspace",
            "Ayni dogrulanmis changes planinin tum hedeflerini diff ile tek kullanici onayina sun, yedekle, uygula ve cargo check+test ile dogrula. Hatada tum plan geri alinmaya calisilir. Yalniz acik Modify gorevi; silme/tasima/yeni dizin desteklenmez.",
        ),
    ] {
        let mut definition = json!({"type": "function", "function": {
            "name": name, "description": description,
            "parameters": {"type": "object", "properties": {"changes": changes}, "required": ["changes"]}
        }});
        if name == "verify_workspace" {
            definition["function"]["parameters"]["properties"]["mode"] =
                json!({"type": "string", "enum": ["check", "test"], "default": "check"});
        }
        tools.push(definition);
    }
    definitions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_ending_options(content: &str) -> PatchMatchOptions {
        PatchMatchOptions {
            mode: PatchMatchMode::LineEndings,
            expected_sha256: Some(source_sha256(content)),
        }
    }

    #[test]
    fn eol_eslesme_secenekleri_acik_ve_sha256_ile_bagli_olmali() {
        assert_eq!(
            PatchMatchOptions::from_arguments(&json!({})).unwrap(),
            PatchMatchOptions::default()
        );
        assert_eq!(
            PatchMatchOptions::from_arguments(&json!({"match_mode":"exact"})).unwrap(),
            PatchMatchOptions::default()
        );
        let options = PatchMatchOptions::from_arguments(
            &json!({"match_mode":"line_endings","expected_sha256":"A".repeat(64)}),
        )
        .unwrap();
        assert_eq!(options.expected_sha256, Some("a".repeat(64)));
        for args in [
            json!({"match_mode":"line_endings"}),
            json!({"match_mode":"fuzzy"}),
            json!({"match_mode":null}),
            json!({"expected_sha256":"bad"}),
            json!({"expected_sha256":"z".repeat(64)}),
        ] {
            assert!(PatchMatchOptions::from_arguments(&args).is_err());
        }
        let direct = PatchMatchOptions {
            mode: PatchMatchMode::LineEndings,
            expected_sha256: None,
        };
        assert!(match_line_block("fn a() {}\n", "fn a() {}", &direct).is_err());
    }

    #[test]
    fn eol_byte_eslemesi_lf_crlf_mixed_unicode_ve_dis_byte_lari_korumali() {
        for (first, second, outside_first, outside_last) in [
            ("\n", "\n", "\n", "\n"),
            ("\r\n", "\r\n", "\r\n", "\r\n"),
            ("\r\n", "\n", "\r\n", "\n"),
            ("\n", "\r\n", "\r\n", "\n"),
        ] {
            let prefix = format!("// önce🦀{outside_first}");
            let old_original = format!("    fn α() {{}}{first}\tfn β() {{}}{second}");
            let suffix = format!("// sonra🌱{outside_last}");
            let content = format!("{prefix}{old_original}{suffix}");
            let old_text = "fn α() {}\n\tfn β() {}\n";
            let new_text = "fn α() { /* ✓ */ }\n\tfn β() {}\n";
            let options = line_ending_options(&content);
            let matched = match_line_block(&content, old_text, &options).unwrap();
            assert_eq!(matched.start, prefix.len() + 4);
            assert_eq!(matched.end, prefix.len() + old_original.len());
            assert_eq!(matched.range_sha256, source_sha256(&old_original[4..]));
            assert_eq!(matched.source_sha256, source_sha256(&content));
            let updated = replace_line_block(&content, old_text, new_text, &options).unwrap();
            assert_eq!(updated, format!("{prefix}    {new_text}{suffix}"));
            assert_eq!(
                &updated.as_bytes()[..matched.start],
                &content.as_bytes()[..matched.start]
            );
            assert!(
                updated
                    .as_bytes()
                    .ends_with(&content.as_bytes()[matched.end..])
            );
            assert_eq!(
                unique_line_block_match(&content, old_text).is_err(),
                first == "\r\n" || second == "\r\n"
            );
        }
    }

    #[test]
    fn eol_modu_exact_ve_normalized_eslesmelerin_tumunu_saymali() {
        let content = "fn a() {}\nfn a() {}\r\n";
        let old = "fn a() {}\n";
        assert_eq!(
            unique_line_block_match(content, old),
            Ok(0),
            "Strict varsayilan semantik korunur."
        );
        let error = match_line_block(content, old, &line_ending_options(content)).unwrap_err();
        let diagnostic: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic["reason"], "ambiguous");
        assert_eq!(diagnostic["match_count"], 2);
        assert_eq!(diagnostic["normalized_match_count"], 2);
        assert!(diagnostic.get("recovery").is_none());
        let only_crlf = "fn a() {}\r\nfn a() {}\r\n";
        let strict = unique_line_block_match(only_crlf, old).unwrap_err().json();
        assert!(
            strict.get("recovery").is_none(),
            "Normalized iki konum tek kanit sayilamaz."
        );
        let overlap = "fn a() {}\nfn a() {}\r\nfn a() {}\n";
        let two_lines = "fn a() {}\nfn a() {}\n";
        let strict = unique_line_block_match(overlap, two_lines)
            .unwrap_err()
            .json();
        assert_eq!(strict["normalized_match_count"], 2);
        assert!(strict.get("recovery").is_none());
        let error =
            match_line_block(overlap, two_lines, &line_ending_options(overlap)).unwrap_err();
        let diagnostic: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic["reason"], "ambiguous");
        assert_eq!(
            diagnostic["match_count"], 2,
            "Ortusen normalized tam bloklar da sayilir."
        );
    }

    #[test]
    fn eol_optin_noktalama_bosluk_bare_cr_ve_literal_escape_esnetmemeli() {
        for (content, old_text, reason) in [
            ("    let value = 1;\r\n", "let value = 1", "partial_line"),
            ("fn a() {}\r\n", "fn a() {", "partial_line"),
            ("fn a() { 1 }\r\n", "fn a(){ 1 }\n", "not_found"),
            ("fn a() {}\r\n", "fn a() {}\\n", "not_found"),
            (
                "fn a() {}\rfn b() {}\r",
                "fn a() {}\nfn b() {}\n",
                "not_found",
            ),
        ] {
            let error =
                match_line_block(content, old_text, &line_ending_options(content)).unwrap_err();
            let diagnostic: Value = serde_json::from_str(&error).unwrap();
            assert_eq!(diagnostic["reason"], reason);
            assert!(diagnostic.get("recovery").is_none());
        }
        let content = "fn a() {}\r\n";
        assert!(
            replace_line_block(
                content,
                "fn a() {}\n",
                "fn a() {}\r\n",
                &line_ending_options(content)
            )
            .is_err(),
            "Somut byte adayi degismiyorsa no-op reddedilir."
        );
        assert_eq!(
            replace_line_block(content, "fn a() {}\n", "", &line_ending_options(content)).unwrap(),
            "",
            "Workspace bos replacement blok silmesini korur."
        );
    }

    #[test]
    fn eol_kurtarma_kaniti_tek_aralik_ve_guncel_kaynak_hashi_gerektirmeli() {
        let content = "// ü\r\nfn a() {}\r\n// son\n";
        let old = "fn a() {}\n";
        let strict = unique_line_block_match(content, old).unwrap_err().json();
        let recovery = &strict["recovery"];
        assert_eq!(recovery["match_mode"], "line_endings");
        assert_eq!(recovery["expected_sha256"], source_sha256(content));
        assert_eq!(recovery["byte_range"]["start"], "// ü\r\n".len());
        assert_eq!(recovery["range_sha256"], source_sha256("fn a() {}\r\n"));
        let options = PatchMatchOptions::from_arguments(recovery).unwrap();
        assert!(match_line_block(content, old, &options).is_ok());
        let drift = format!("// drift\n{content}");
        let error = match_line_block(&drift, old, &options).unwrap_err();
        let diagnostic: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(diagnostic["field"], "expected_sha256");
        assert_eq!(diagnostic["reason"], "source_changed");
        assert!(diagnostic.get("recovery").is_none());
        assert!(!error.contains("drift\n"));
    }

    #[test]
    fn optin_prepare_exact_varsayilani_ve_yazmadan_aday_uretimini_korumali() {
        let root = std::env::temp_dir().join(format!(
            "fiko_eol_prepare_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        let content = "// ü\r\nfn a() {}\r\n";
        fs::write(root.join("src/a.rs"), content).unwrap();
        let old = "fn a() {}\n";
        let new = "fn a() { }\n";
        assert!(prepare_patch(&root, "src/a.rs", old, new).is_err());
        let plan =
            prepare_patch_with_options(&root, "src/a.rs", old, new, &line_ending_options(content))
                .unwrap();
        assert_eq!(plan.original, content);
        assert_eq!(plan.updated, "// ü\r\nfn a() { }\n");
        assert_eq!(fs::read(root.join("src/a.rs")).unwrap(), content.as_bytes());
        assert!(!root.join(".fiko_backups").exists());
        assert!(root.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fiko_eol_prepare_")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newline_tam_satir_bloklari_eslesmeli() {
        for ending in ["\n", "\r\n"] {
            let first = format!("fn a() {{}}{ending}");
            let middle = format!("fn b() {{}}{ending}");
            let last = format!("fn c() {{}}{ending}");
            let content = format!("{first}{middle}{last}");
            for (needle, expected) in [
                (first.clone(), 0),
                (middle.clone(), first.len()),
                (last.clone(), first.len() + middle.len()),
                (format!("{first}{middle}"), 0),
                (format!("{middle}{last}"), first.len()),
            ] {
                assert_eq!(
                    exact_line_block_locations(&content, &needle),
                    [expected],
                    "ending={ending:?}, needle={needle:?}"
                );
            }
            let no_final_newline = format!("{first}{middle}fn c() {{}}");
            assert_eq!(
                exact_line_block_locations(&no_final_newline, "fn c() {}"),
                [first.len() + middle.len()]
            );
            assert_eq!(exact_line_block_locations(&content, "fn a() {}"), [0]);
            let repeated = format!("{first}{first}");
            assert_eq!(
                exact_line_block_locations(&repeated, &first),
                [0, first.len()]
            );
        }
        for (content, needle) in [
            ("fn a() {}\nfn b() {}\n", "fn a() {}\\n"),
            ("fn a() {}\nfn b() {}\n", "fn a() {}\r\n"),
            ("fn a() {}\r\nfn b() {}\r\n", "fn a() {}\n"),
            ("fn a() {}\nfn b() {}\n", "a() {}\n"),
            ("    println!(\"x\");\nfn b() {}\n", "println!(\"x\")"),
        ] {
            assert!(
                exact_line_block_locations(content, needle).is_empty(),
                "Yanlis veya eksik blok kabul edildi: {needle:?}"
            );
        }
    }

    fn output_fixture(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code as u32)
        };
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code << 8)
        };
        std::process::Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn komut_durumu_metin_degil_exit_kodu_ile_belirlenmeli() {
        for (code, stdout, expected) in [
            (
                0,
                "test result: ok. 3 passed; 0 failed",
                ExecutionStatus::Succeeded,
            ),
            (1, "test result: FAILED. 1 failed", ExecutionStatus::Failed),
            (
                0,
                "FAILED ve INTENT_BLOCKED siradan metindir",
                ExecutionStatus::Succeeded,
            ),
            (7, "All tests passed. Basarili!", ExecutionStatus::Failed),
        ] {
            let result =
                TestResult::from_output("cargo test".into(), output_fixture(code, stdout, ""));
            assert_eq!(result.status, expected);
            assert_eq!(result.exit_code, Some(code));
            assert_eq!(result.success, code == 0);
            assert_eq!(
                result.observation(),
                CommandObservation {
                    command: "cargo test".into(),
                    status: expected,
                    exit_code: Some(code),
                }
            );
            let response = result.tool_response();
            assert_eq!(response["authority"], "rust_process");
            assert_eq!(response["command"], "cargo test");
            assert_eq!(response["status"], expected.label());
            assert_eq!(response["exit_code"], code);
            assert!(response["test_counts"].is_null());
            assert!(
                result
                    .summary_line()
                    .contains("Test sayilari: mevcut degil")
            );
        }
    }

    #[tokio::test]
    async fn spawn_hatasi_tipli_sonuc_olmali() {
        let missing = std::env::temp_dir().join(format!(
            "fiko_missing_command_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        assert!(!missing.exists());
        let result = execute_command(
            Command::new(&missing),
            "cargo test".into(),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.status, ExecutionStatus::ExecutionError);
        assert!(!result.success);
        assert_eq!(result.exit_code, None);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("Komut calistirilamadi")
        );
        assert_eq!(result.tool_response()["status"], "execution_error");
        assert!(result.summary_line().contains("Cikis kodu: mevcut degil"));
    }

    #[tokio::test]
    async fn zaman_asimi_basarisiz_test_olarak_sunulmamali() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 3",
            ]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 3"]);
            command
        };
        command.stdin(std::process::Stdio::null());
        let result = execute_command(
            command,
            "kontrollu zaman asimi komutu".into(),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(result.status, ExecutionStatus::TimedOut);
        assert_eq!(result.exit_code, None);
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("kesinti oncesi cikti mevcut degil")
        );
        assert_eq!(result.tool_response()["status"], "timed_out");
    }

    #[cfg(unix)]
    #[test]
    fn sinyalle_kesilen_komut_tamamlanmis_sayilmamali() {
        use std::os::unix::process::ExitStatusExt;
        let result = TestResult::from_output(
            "cargo test".into(),
            std::process::Output {
                status: std::process::ExitStatus::from_raw(9),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        );
        assert_eq!(result.status, ExecutionStatus::Interrupted);
        assert_eq!(result.exit_code, None);
        assert!(!result.success);
    }

    #[test]
    fn kisaltilmis_cikti_metadata_ve_tam_kaniti_korumali() {
        let stdout = format!("ILK SATIR\n{}\nSON SATIR", "ğ".repeat(15_000));
        let stderr = format!("ILK HATA\n{}\nSON HATA", "ş".repeat(15_000));
        let result =
            TestResult::from_output("cargo test".into(), output_fixture(13, &stdout, &stderr));
        let response = result.tool_response();
        assert!(result.output_truncated);
        assert!(result.report.chars().count() <= MAX_COMMAND_REPORT_CHARS);
        assert_eq!(result.stdout, stdout);
        assert_eq!(result.stderr, stderr);
        assert_eq!(response["status"], "failed");
        assert_eq!(response["exit_code"], 13);
        assert_eq!(response["output_truncated"], true);
        assert!(
            response["stdout"]
                .as_str()
                .unwrap()
                .starts_with("ILK SATIR")
        );
        assert!(response["stdout"].as_str().unwrap().ends_with("SON SATIR"));
        assert!(response["stderr"].as_str().unwrap().starts_with("ILK HATA"));
        assert!(response["stderr"].as_str().unwrap().ends_with("SON HATA"));
        assert!(
            response["stdout"]
                .as_str()
                .unwrap()
                .contains("CIKTI KISALTILDI")
        );
        assert!(result.report.contains("Cikti kisaltildi: true"));
    }

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
    fn patch_match_tanisi_kaynak_verisini_tasimamali_ve_sinirli_olmali() {
        for content in [
            "SECRET_SOURCE_TOKEN\nC:\\private\\secret.rs\n".to_owned(),
            "SECRET_SOURCE_TOKEN\n".repeat(6000),
        ] {
            let error = unique_line_block_match(&content, "SECRET_NEEDLE_TOKEN").unwrap_err();
            assert_eq!(error.reason, PatchMatchReason::NotFound);
            let encoded = error.to_string();
            let diagnostic: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(diagnostic["error"], "PATCH_MATCH");
            assert_eq!(diagnostic["field"], "old_text");
            assert_eq!(diagnostic["match_count"], 0);
            assert_eq!(diagnostic["total_lines"], content.lines().count());
            assert!(encoded.len() < 1600);
            assert!(!encoded.contains("SECRET_SOURCE_TOKEN"));
            assert!(!encoded.contains("SECRET_NEEDLE_TOKEN"));
            assert!(!encoded.contains("private"));
            assert!(!encoded.contains("secret.rs"));
        }
    }

    #[test]
    fn patch_match_noktalama_sinir_cogulluk_ve_yokluk_ayrilmali() {
        for (content, old_text, reason, exact_count, literal_count) in [
            (
                "    let value = 1;\n",
                "let value = 1",
                PatchMatchReason::PartialLine,
                0,
                1,
            ),
            (
                "fn demo() {}\n",
                "fn demo() {",
                PatchMatchReason::PartialLine,
                0,
                1,
            ),
            (
                "fn demo() {}\n",
                "demo() {}",
                PatchMatchReason::PartialLine,
                0,
                1,
            ),
            (
                "fn demo() {}\nfn demo() {}\n",
                "fn demo() {}",
                PatchMatchReason::Ambiguous,
                2,
                2,
            ),
            (
                "fn demo() {}\n",
                "fn absent() {}",
                PatchMatchReason::NotFound,
                0,
                0,
            ),
            (
                "fn demo() {}\n",
                "fn demo() {}\\n",
                PatchMatchReason::NotFound,
                0,
                0,
            ),
            ("", "", PatchMatchReason::NotFound, 0, 0),
        ] {
            let error = unique_line_block_match(content, old_text).unwrap_err();
            assert_eq!(error.reason, reason);
            assert_eq!(error.match_count, exact_count);
            assert_eq!(error.literal_match_count, literal_count);
            assert_eq!(error.normalized_match_count, None);
        }
    }

    #[test]
    fn crlf_lf_farki_yalniz_kanitli_tani_olmali() {
        for (content, old_text) in [
            (
                "    fn a() {}\r\n\tfn b() {}\r\n",
                "fn a() {}\n\tfn b() {}\n",
            ),
            (
                "    fn a() {}\n\tfn b() {}\n",
                "fn a() {}\r\n\tfn b() {}\r\n",
            ),
            ("    fn a() {}\n\tfn b() {}\r\n", "fn a() {}\n\tfn b() {}\n"),
        ] {
            let error = unique_line_block_match(content, old_text).unwrap_err();
            assert_eq!(error.reason, PatchMatchReason::LineEndingsMismatch);
            assert_eq!(error.match_count, 0);
            assert_eq!(error.normalized_match_count, Some(1));
            assert!(!exact_line_block_locations(content, old_text).contains(&4));
            let diagnostic: Value = serde_json::from_str(&error.to_string()).unwrap();
            assert!(diagnostic["normalization_hint"].is_string());
        }
        let crlf = LineEndingMetadata::of("a\r\nb\r\n");
        assert_eq!(
            crlf,
            LineEndingMetadata {
                lf: 0,
                crlf: 2,
                bare_cr: 0,
                final_newline: true
            }
        );
        let mixed = LineEndingMetadata::of("a\r\nb\nc\r");
        assert_eq!(
            mixed,
            LineEndingMetadata {
                lf: 1,
                crlf: 1,
                bare_cr: 1,
                final_newline: false
            }
        );
    }

    #[test]
    fn ortak_match_helper_tam_eslesme_ve_girinti_kuralini_korumali() {
        for ending in ["\n", "\r\n"] {
            let content = format!("fn demo() {{{ending}\tlet value = 1;{ending}}}{ending}");
            for old_text in [
                "let value = 1;".to_owned(),
                format!("\tlet value = 1;{ending}"),
                format!("fn demo() {{{ending}\tlet value = 1;{ending}}}{ending}"),
            ] {
                let exact = exact_line_block_locations(&content, &old_text);
                assert_eq!(exact.len(), 1);
                assert_eq!(unique_line_block_match(&content, &old_text), Ok(exact[0]));
            }
        }
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
        let (header, body) = result_1_1.split_once('\n').unwrap();
        let metadata: Value =
            serde_json::from_str(header.strip_prefix("[READ_LINES] ").unwrap()).unwrap();
        assert_eq!(metadata["total_lines"], 65);
        assert_eq!(
            metadata["returned_range"],
            json!({"start_line":1,"end_line":1})
        );
        assert_eq!(body.lines().count(), 1);

        // 1-60 araligi: 60 satir dondurmeli
        let result_1_60 = read_lines(&root, "src/test.rs", 1, 60).unwrap();
        assert!(result_1_60.contains("60 |     println!(\"satir 60\", 60);"));
        let (header, body) = result_1_60.split_once('\n').unwrap();
        let metadata: Value =
            serde_json::from_str(header.strip_prefix("[READ_LINES] ").unwrap()).unwrap();
        assert_eq!(metadata["total_lines"], 65);
        assert_eq!(
            metadata["returned_range"],
            json!({"start_line":1,"end_line":60})
        );
        assert_eq!(body.lines().count(), 60);

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
