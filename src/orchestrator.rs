//! v0.9 Intelligent Tool Orchestrator.
//!
//! Modelin arac kullanimini Rust tarafinda yonetir. Calisan tool-calling
//! mekanizmasina (`agent::handle_tool_call`) dokunmaz; onun onune bir denetim
//! katmani koyar:
//! - Task Intent Engine'den gelen gorev turune gore yazma yetkisi: verify_candidate
//!   ve apply_patch yalnizca `Modify` gorevlerinde ya da belirsiz istekte
//!   kullanicinin acikca izin vermesiyle calisir,
//! - arac argumanlarinin on dogrulamasi (anlasilir hata, bos cagri yok),
//! - cagri defteri: ayni okuma tekrarlanirsa kisa TEKRAR yaniti, ayni basarisiz
//!   cagri bir kez uyarilir, ucuncu tekrarda gorev durdurulur,
//! - her cagrinin hangi asamada yapildiginin kaydi ve olculebilir istatistikler.
//!
//! Verified Patch Lock, parmak izi kontrolleri, dogrulama izni ve patch onayi
//! `handle_tool_call` icinde oldugu gibi kalir; bu katman onlari asla atlamaz.

use std::collections::{BTreeMap, HashMap};
use std::io;

use serde_json::{Map, Value};

use crate::agent::{Prompter, Session, ToolOutcome, handle_tool_call, task_status_line};
use crate::execution::TaskPhase;
use crate::intent::{self, IntentDecision, TaskIntent, WriteAuthority};
use crate::repair_controller::RepairController;

/// Ayni arac ayni argumanlarla en fazla bu kadar cagrilabilir; sonuncusu gorevi durdurur.
pub const MAX_IDENTICAL_CALLS: usize = 3;
/// Bir gorevde toplam arac cagrisi ust siniri (MAX_AGENT_ROUNDS'a ek guvenlik agi).
pub const MAX_TOOL_CALLS_PER_TASK: usize = 30;

pub const READ_TOOLS: &[&str] = &["read_file", "list_files", "read_lines", "search_code"];
pub const WRITE_PATH_TOOLS: &[&str] = &["verify_candidate", "apply_patch"];
pub const ALL_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "read_lines",
    "search_code",
    "run_tests",
    "verify_candidate",
    "apply_patch",
];

const INTENT_QUESTION: &str = "Istegin turu belirsiz ve model kod degisikligi onermek istiyor. Bu gorevde kod degisikligine izin veriyor musunuz?";

/// Bu on eklerle baslayan sonuclar basarisiz/engellenmis sayilir; ayni cagriyi
/// birebir tekrarlamak anlamsizdir.
const FAILURE_PREFIXES: &[&str] = &[
    "ERROR:",
    "VERIFY_FAILED",
    "VERIFY_REJECTED",
    "APPLY_BLOCKED",
    "INTENT_BLOCKED",
    "TASK_ENDED",
    "LOOP_BLOCKED",
    "TOOL_LIMIT",
    "PATCH_REJECTED",
    "PATCH_FAILED_ROLLED_BACK",
    "CRITICAL",
    "TEST_REJECTED",
];

pub fn is_failure(output: &str) -> bool {
    FAILURE_PREFIXES
        .iter()
        .any(|prefix| output.starts_with(prefix))
}

// ============================================================
// ISTATISTIK VE DEFTER
// ============================================================
/// Gorev basina olculebilir arac kullanim sayaclari.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolStats {
    /// Modelin yaptigi tum cagri denemeleri.
    pub calls: usize,
    /// handle_tool_call'a gercekten ulasan cagrilar.
    pub dispatched: usize,
    /// Ayni okuma tekrarlandi, icerik degismedi: kisa TEKRAR yaniti verildi.
    pub duplicate_reads: usize,
    /// TEKRAR yanitiyla modele gonderilmeyen karakter sayisi.
    pub saved_chars: usize,
    /// Gorev turu yazma yetkisi vermedigi icin reddedilen verify/apply cagrilari.
    pub intent_blocked: usize,
    /// Arac calistirilmadan yakalanan hatali argumanlar.
    pub invalid_args: usize,
    /// Daha once basarisiz olan birebir ayni cagrinin tekrari (calistirilmadi).
    pub repeated_failures: usize,
    /// Ucuncu tekrar ya da cagri siniri: gorev durduruldu.
    pub loop_stops: usize,
}

#[derive(Debug, Clone)]
struct CallRecord {
    key: String,
    name: String,
    phase_before: TaskPhase,
    phase_after: TaskPhase,
    failed: bool,
    /// Yalnizca basarili okuma araclari icin tam cikti (tekrar kiyasi icin).
    output: Option<String>,
}

#[derive(Default)]
struct ToolLedger {
    calls: Vec<CallRecord>,
    by_key: HashMap<String, Vec<usize>>,
}

impl ToolLedger {
    fn record(&mut self, record: CallRecord) -> usize {
        let number = self.calls.len() + 1;
        self.by_key
            .entry(record.key.clone())
            .or_default()
            .push(self.calls.len());
        self.calls.push(record);
        number
    }

    fn repeats(&self, key: &str) -> usize {
        self.by_key.get(key).map_or(0, Vec::len)
    }

    fn last(&self, key: &str) -> Option<&CallRecord> {
        self.by_key
            .get(key)
            .and_then(|indices| indices.last())
            .map(|index| &self.calls[*index])
    }

    /// Ayni anahtarla daha once alinmis basarili okuma ciktisi ve cagri numarasi.
    fn last_success_output(&self, key: &str) -> Option<(usize, &str)> {
        self.by_key.get(key)?.iter().rev().find_map(|index| {
            self.calls[*index]
                .output
                .as_deref()
                .map(|output| (*index + 1, output))
        })
    }
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, canonical(v))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Ayni arac + ayni argumanlar icin ayni anahtar (alan sirasi onemsiz).
pub fn call_key(name: &str, args: &Value) -> String {
    format!("{name}|{}", canonical(args))
}

// ============================================================
// ARGUMAN DOGRULAMA
// ============================================================
fn signature(name: &str) -> &'static str {
    match name {
        "read_file" => "{path: metin}",
        "list_files" => "{path: metin; proje koku icin \".\"}",
        "read_lines" => {
            "{path: metin, start_line: tam sayi >= 1, end_line: tam sayi >= start_line; en fazla 60 satir}"
        }
        "search_code" => "{query: tek satir metin, 1-128 byte}",
        "run_tests" => "{mode: \"check\" | \"test\"}",
        "verify_candidate" => {
            "{path: metin, old_text: metin, new_text: metin, mode?: \"check\" | \"test\"}"
        }
        "apply_patch" => "{path: metin, old_text: metin, new_text: metin}",
        _ => "{}",
    }
}

fn example(name: &str) -> &'static str {
    match name {
        "read_file" => r#"{"path": "src/main.rs"}"#,
        "list_files" => r#"{"path": "src"}"#,
        "read_lines" => r#"{"path": "src/main.rs", "start_line": 10, "end_line": 40}"#,
        "search_code" => r#"{"query": "fn read_lines"}"#,
        "run_tests" => r#"{"mode": "check"}"#,
        "verify_candidate" => {
            r#"{"path": "src/main.rs", "old_text": "let x = 1;", "new_text": "let x = 2;", "mode": "check"}"#
        }
        "apply_patch" => {
            r#"{"path": "src/main.rs", "old_text": "let x = 1;", "new_text": "let x = 2;"}"#
        }
        _ => "{}",
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "sayi",
        Value::String(_) => "metin",
        Value::Array(_) => "dizi",
        Value::Object(_) => "nesne",
    }
}

fn invalid(name: &str, detail: &str) -> String {
    format!(
        "Gecersiz arguman ({name}): {detail}. Beklenen: {}. Ornek: {}",
        signature(name),
        example(name)
    )
}

fn text(name: &str, object: &Map<String, Value>, field: &str) -> Result<Value, String> {
    match object.get(field) {
        None => Err(invalid(name, &format!("'{field}' alani eksik"))),
        Some(Value::String(value)) if value.trim().is_empty() => {
            Err(invalid(name, &format!("'{field}' bos olamaz")))
        }
        Some(Value::String(value)) => Ok(Value::String(value.clone())),
        Some(other) => Err(invalid(
            name,
            &format!("'{field}' metin olmali (gelen: {})", kind(other)),
        )),
    }
}

fn integer(name: &str, object: &Map<String, Value>, field: &str) -> Result<u64, String> {
    match object.get(field) {
        None => Err(invalid(name, &format!("'{field}' alani eksik"))),
        Some(Value::Number(number)) => number
            .as_u64()
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                    .map(|f| f as u64)
            })
            .ok_or_else(|| invalid(name, &format!("'{field}' negatif olmayan tam sayi olmali"))),
        Some(Value::String(value)) => value.trim().parse::<u64>().map_err(|_| {
            invalid(
                name,
                &format!("'{field}' tam sayi olmali (gelen metin: {value:?})"),
            )
        }),
        Some(other) => Err(invalid(
            name,
            &format!("'{field}' tam sayi olmali (gelen: {})", kind(other)),
        )),
    }
}

fn mode(name: &str, object: &Map<String, Value>, optional: bool) -> Result<Value, String> {
    match object.get("mode") {
        None if optional => Ok(Value::from("check")),
        None => Err(invalid(name, "'mode' alani eksik")),
        Some(Value::String(value)) if value == "check" || value == "test" => {
            Ok(Value::from(value.as_str()))
        }
        Some(other) => Err(invalid(
            name,
            &format!("'mode' yalnizca \"check\" veya \"test\" olabilir (gelen: {other})"),
        )),
    }
}

/// Argumanlari arac calistirilmadan once dogrular ve normalize eder
/// (metin olarak gelen satir numaralari sayiya cevrilir, mode varsayilani eklenir).
/// Hata metni modele neyin yanlis oldugunu ve beklenen bicimi soyler.
pub fn validate_args(name: &str, args: &Value) -> Result<Value, String> {
    if !ALL_TOOLS.contains(&name) {
        return Err(format!(
            "Bilinmeyen arac '{name}'. Kullanilabilir araclar: {}.",
            ALL_TOOLS.join(", ")
        ));
    }
    let Some(object) = args.as_object() else {
        return Err(invalid(
            name,
            &format!(
                "argumanlar JSON nesnesi olarak ayristirilamadi (gelen: {})",
                kind(args)
            ),
        ));
    };
    let mut out = Map::new();
    match name {
        "read_file" | "list_files" => {
            out.insert("path".into(), text(name, object, "path")?);
        }
        "read_lines" => {
            out.insert("path".into(), text(name, object, "path")?);
            let start = integer(name, object, "start_line")?;
            let end = integer(name, object, "end_line")?;
            if start == 0 {
                return Err(invalid(
                    name,
                    "start_line 1 veya daha buyuk olmali (satirlar 1 tabanli)",
                ));
            }
            if end < start {
                return Err(invalid(
                    name,
                    &format!("end_line ({end}) start_line'dan ({start}) kucuk olamaz"),
                ));
            }
            if end - start >= 60 {
                return Err(invalid(
                    name,
                    &format!(
                        "aralik en fazla 60 satir olabilir (istenen {} satir)",
                        end - start + 1
                    ),
                ));
            }
            out.insert("start_line".into(), Value::from(start));
            out.insert("end_line".into(), Value::from(end));
        }
        "search_code" => {
            let query = text(name, object, "query")?;
            let raw = query.as_str().unwrap_or("");
            if raw.len() > 128 || raw.contains('\n') {
                return Err(invalid(name, "query tek satir ve en fazla 128 byte olmali"));
            }
            out.insert("query".into(), query);
        }
        "run_tests" => {
            out.insert("mode".into(), mode(name, object, false)?);
        }
        "verify_candidate" | "apply_patch" => {
            out.insert("path".into(), text(name, object, "path")?);
            let old_text = text(name, object, "old_text")?;
            let new_text = text(name, object, "new_text")?;
            if old_text == new_text {
                return Err(invalid(name, "old_text ve new_text ayni; degisiklik yok"));
            }
            out.insert("old_text".into(), old_text);
            out.insert("new_text".into(), new_text);
            if name == "verify_candidate" {
                out.insert("mode".into(), mode(name, object, true)?);
            }
        }
        other => {
            return Err(format!(
                "Bilinmeyen arac '{other}'. Kullanilabilir araclar: {}.",
                ALL_TOOLS.join(", ")
            ));
        }
    }
    Ok(Value::Object(out))
}

// ============================================================
// ORKESTRATOR
// ============================================================
/// Tek bir kullanici gorevi icin arac denetim katmani. `RepairController`i
/// (kilit + sayaclar + asama) sahiplenir; `run_task` her gorev icin bir tane kurar.
pub struct Orchestrator {
    decision: IntentDecision,
    authority: WriteAuthority,
    pub repair: RepairController,
    ledger: ToolLedger,
    stats: ToolStats,
    applied: bool,
    unproven_claim: bool,
}

impl Orchestrator {
    pub fn new(decision: IntentDecision) -> Self {
        let authority = decision.intent.write_authority();
        Self {
            decision,
            authority,
            repair: RepairController::default(),
            ledger: ToolLedger::default(),
            stats: ToolStats::default(),
            applied: false,
            unproven_claim: false,
        }
    }

    /// Kullanici istegini siniflandirip gorev baglamini kurar.
    pub fn for_request(request: &str) -> Self {
        Self::new(intent::classify(request))
    }

    pub fn intent(&self) -> TaskIntent {
        self.decision.intent
    }

    pub fn decision(&self) -> &IntentDecision {
        &self.decision
    }

    pub fn authority(&self) -> WriteAuthority {
        self.authority
    }

    pub fn phase(&self) -> TaskPhase {
        self.repair.phase()
    }

    pub fn stats(&self) -> &ToolStats {
        &self.stats
    }

    /// Gercek projede dogrulanmis bir patch uygulandi mi? (yalnizca PATCH_VERIFIED kaniti)
    pub fn applied(&self) -> bool {
        self.applied
    }

    /// Konsol icin niyet satiri.
    pub fn intent_line(&self) -> String {
        format!(
            "[NIYET] {} (guven: {}; kanit: {})",
            self.decision.intent,
            self.decision.confidence.label(),
            self.decision.evidence_line()
        )
    }

    /// Kullanici mesajina eklenen kisa yol gosterici (system prompt buyutulmez).
    pub fn request_note(&self) -> String {
        format!(
            "[FIKO NOT] Gorev turu: {} (kanit: {}). {}",
            self.decision.intent,
            self.decision.evidence_line(),
            self.decision.intent.guidance()
        )
    }

    /// Her cagrinin asamasi: "#1 read_file [Created->Inspecting] ok".
    pub fn call_log(&self) -> Vec<String> {
        self.ledger
            .calls
            .iter()
            .enumerate()
            .map(|(index, record)| {
                format!(
                    "#{} {} [{}->{}] {}",
                    index + 1,
                    record.name,
                    record.phase_before,
                    record.phase_after,
                    if record.failed { "hata" } else { "ok" }
                )
            })
            .collect()
    }

    fn decide_authority<P: Prompter>(&mut self, prompter: &mut P) -> io::Result<WriteAuthority> {
        if self.authority == WriteAuthority::Undecided {
            println!("[NIYET] Istek belirsiz; kod degisikligi icin kullaniciya soruluyor.");
            let allowed = prompter.ask_yes(INTENT_QUESTION)?;
            self.authority = if allowed {
                WriteAuthority::Granted
            } else {
                WriteAuthority::Denied
            };
            println!(
                "[NIYET] Kullanici karari: {}",
                if allowed {
                    "bu gorevde kod degisikligine izin verildi"
                } else {
                    "bu gorevde kod degisikligi yok"
                }
            );
        }
        Ok(self.authority)
    }

    fn intent_block_text(&self, name: &str) -> String {
        let reason = match self.decision.intent {
            TaskIntent::Analyze => "kullanici yalnizca inceleme istedi",
            TaskIntent::Explain => "kullanici aciklama istedi",
            TaskIntent::Test => "kullanici derleme/test istedi",
            TaskIntent::Modify | TaskIntent::Unknown => {
                "istek belirsizdi ve kullanici kod degisikligine izin vermedi"
            }
        };
        format!(
            "INTENT_BLOCKED: {name} bu gorevde kullanilamaz. Gorev turu: {} ({reason}). Ana proje DEGISMEDI.\nsonraki_adim: Bulgularini KANITLANMIS HATA / POTANSIYEL RISK olarak raporla; kod degisikligi icin kullanici ayri bir gorevle acikca istemeli.",
            self.decision.intent
        )
    }

    fn refuse(
        &mut self,
        name: &str,
        key: String,
        output: String,
        stop: Option<&str>,
        announce: bool,
    ) -> ToolOutcome {
        let phase = self.phase();
        if announce {
            println!("[ORCH] {}", output.lines().next().unwrap_or(""));
        }
        self.ledger.record(CallRecord {
            key,
            name: name.to_string(),
            phase_before: phase,
            phase_after: phase,
            failed: true,
            output: None,
        });
        ToolOutcome {
            output,
            stop: stop.map(str::to_string),
        }
    }

    /// Bir arac cagrisini denetimden gecirip calistirir. Tum guvenlik kurallari
    /// (kilit, izin, onay) handle_tool_call icinde kalir; burasi yalnizca gereksiz,
    /// yetkisiz ve hatali cagrilari ona ulasmadan yakalar.
    pub async fn execute<P: Prompter>(
        &mut self,
        session: &mut Session,
        prompter: &mut P,
        name: &str,
        raw_args: &Value,
    ) -> io::Result<ToolOutcome> {
        self.stats.calls += 1;
        let key = call_key(name, raw_args);
        let repeats = self.ledger.repeats(&key);

        // 1) Dongu ve toplam cagri korumasi.
        if self.stats.calls > MAX_TOOL_CALLS_PER_TASK {
            self.stats.loop_stops += 1;
            let output = format!(
                "TOOL_LIMIT: Bu gorevde {MAX_TOOL_CALLS_PER_TASK} arac cagrisi sinirina ulasildi; gorev durduruldu. Ana proje {}.",
                self.project_state()
            );
            return Ok(self.refuse(
                name,
                key,
                output,
                Some("Arac cagrisi siniri asildi; gorev durduruldu."),
                true,
            ));
        }
        if repeats + 1 >= MAX_IDENTICAL_CALLS {
            self.stats.loop_stops += 1;
            let output = format!(
                "LOOP_BLOCKED: {name} ayni argumanlarla {MAX_IDENTICAL_CALLS}. kez cagrildi; gorev durduruldu. Ana proje {}.",
                self.project_state()
            );
            return Ok(self.refuse(
                name,
                key,
                output,
                Some("Ayni arac cagrisi tekrarlandi (dongu); gorev durduruldu."),
                true,
            ));
        }

        // 2) Daha once basarisiz olan birebir ayni cagri: calistirmadan geri bildir.
        if let Some(previous) = self.ledger.last(&key)
            && previous.failed
        {
            self.stats.repeated_failures += 1;
            let output = format!(
                "ERROR: {name} ayni argumanlarla daha once basarisiz oldu. Ayni cagriyi tekrarlama; argumanlari duzelt ya da farkli bir yaklasim sec. (tekrar {}/{MAX_IDENTICAL_CALLS}; bir sonraki tekrar gorevi durdurur)",
                repeats + 1
            );
            return Ok(self.refuse(name, key, output, None, false));
        }

        // 3) Arguman dogrulama (arac calistirilmadan).
        let args = match validate_args(name, raw_args) {
            Ok(args) => args,
            Err(detail) => {
                self.stats.invalid_args += 1;
                return Ok(self.refuse(name, key, format!("ERROR: {detail}"), None, false));
            }
        };

        // 4) Yazma yoluna giden araclar: asama ve niyet kapisi.
        if WRITE_PATH_TOOLS.contains(&name) {
            let phase = self.phase();
            if phase.is_terminal() {
                let output = format!(
                    "TASK_ENDED: Gorev '{phase}' durumunda sonlandi; {name} artik kullanilamaz. Ana proje {}. Kullaniciya sonucu ozetle.",
                    self.project_state()
                );
                return Ok(self.refuse(name, key, output, None, true));
            }
            if self.decide_authority(prompter)? == WriteAuthority::Denied {
                self.stats.intent_blocked += 1;
                let output = self.intent_block_text(name);
                return Ok(self.refuse(name, key, output, None, true));
            }
        }

        // 5) Gercek calistirma.
        let phase_before = self.phase();
        let mut outcome =
            handle_tool_call(session, &mut self.repair, prompter, name, &args).await?;
        self.stats.dispatched += 1;
        let failed = is_failure(&outcome.output);
        if !failed && (READ_TOOLS.contains(&name) || name == "run_tests") {
            self.repair.enter(TaskPhase::Inspecting);
        }

        // 6) Tekrar okuma: icerik degismediyse tam ciktiyi yeniden gondermek yerine kisa not.
        let mut stored = None;
        if !failed && READ_TOOLS.contains(&name) {
            match self.ledger.last_success_output(&key) {
                Some((number, previous)) if previous == outcome.output => {
                    self.stats.duplicate_reads += 1;
                    self.stats.saved_chars += outcome.output.chars().count();
                    stored = Some(previous.to_string());
                    outcome.output = format!(
                        "TEKRAR: {name} ayni argumanlarla daha once cagrildi (cagri #{number}) ve icerik degismedi. Onceki ciktiyi kullan; ayni dosyayi/araligi yeniden okuma."
                    );
                    println!("[ORCH] Tekrar okuma engellendi: {name} (cagri #{number} ile ayni)");
                }
                _ => stored = Some(outcome.output.clone()),
            }
        }
        if outcome.output.starts_with("PATCH_VERIFIED") {
            self.applied = true;
        }
        self.ledger.record(CallRecord {
            key,
            name: name.to_string(),
            phase_before,
            phase_after: self.phase(),
            failed,
            output: stored,
        });
        Ok(outcome)
    }

    fn project_state(&self) -> &'static str {
        if self.applied {
            "dogrulanmis patch ile guncellendi"
        } else {
            "DEGISMEDI"
        }
    }

    /// Model arac cagirmadan yanit verdi: gorev kapanir. Yanit metni durumu
    /// belirlemez; yalnizca kanitsiz basari iddiasi varsa uyari isaretlenir.
    pub fn finish_answer(&mut self, answer: &str) {
        if !self.applied && self.authority == WriteAuthority::Granted && claims_success(answer) {
            self.unproven_claim = true;
        }
        if self.phase().is_terminal() {
            return;
        }
        let next = if self.applied {
            TaskPhase::Completed
        } else if self.authority == WriteAuthority::Granted {
            // Kod degisikligi istendi ama dogrulanmis patch uygulanmadi.
            TaskPhase::Failed
        } else {
            TaskPhase::Completed
        };
        self.repair.enter(next);
    }

    /// Gorev sonunda kullaniciya gosterilen Rust tarafi gercegi + olculebilir sayaclar.
    pub fn status_report(&self) -> String {
        let stats = &self.stats;
        let mut lines = vec![task_status_line(self.applied).to_string()];
        lines.push(format!(
            "[DURUM] Gorev turu: {} | Asama: {} | Akis: {}",
            self.decision.intent,
            self.phase(),
            self.repair.execution().trace_line()
        ));
        if self.unproven_claim {
            lines.push(
                "[UYARI] Modelin yaniti bir uygulama/basari iddiasi iceriyor; Rust tarafinda bunun kaniti yok."
                    .to_string(),
            );
        }
        lines.push(format!(
            "[ARAC] {} cagri ({} calistirildi) | tekrar okuma engellendi: {} ({} karakter tasarruf) | niyet disi reddedilen: {} | hatali arguman: {} | tekrar eden hata: {} | dongu durdurma: {}",
            stats.calls,
            stats.dispatched,
            stats.duplicate_reads,
            stats.saved_chars,
            stats.intent_blocked,
            stats.invalid_args,
            stats.repeated_failures,
            stats.loop_stops
        ));
        if !self.ledger.calls.is_empty() {
            lines.push(format!("[ARAC IZI] {}", self.call_log().join(" | ")));
        }
        lines.join("\n")
    }
}

/// Modelin "uygulandi / duzeltildi / PATCH_VERIFIED" gibi kanit gerektiren iddialari.
fn claims_success(answer: &str) -> bool {
    let folded = intent::fold(answer);
    [
        "patch_verified",
        "uygulandi",
        "duzeltildi",
        "degistirildi",
        "applied",
    ]
    .iter()
    .any(|claim| folded.contains(claim))
}

// ============================================================
// TESTLER
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::VerifyConsent;
    use crate::verification::test_support::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::Path;

    struct ScriptedPrompter {
        answers: VecDeque<bool>,
        asked: Vec<String>,
    }

    impl ScriptedPrompter {
        fn with(answers: &[bool]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                asked: Vec::new(),
            }
        }
    }

    impl Prompter for ScriptedPrompter {
        fn ask_yes(&mut self, question: &str) -> io::Result<bool> {
            self.asked.push(question.to_string());
            Ok(self.answers.pop_front().unwrap_or(false))
        }
    }

    fn trusted_session(root: &Path) -> Session {
        let mut session = Session::new(root.to_path_buf());
        session.consent = VerifyConsent::granted();
        session
    }

    fn patch(new_text: &str) -> Value {
        json!({"path": "src/main.rs", "old_text": MINI_OLD, "new_text": new_text})
    }

    fn main_rs(root: &Path) -> String {
        fs::read_to_string(root.join("src/main.rs")).unwrap()
    }

    async fn run(
        task: &mut Orchestrator,
        session: &mut Session,
        prompter: &mut ScriptedPrompter,
        name: &str,
        args: &Value,
    ) -> ToolOutcome {
        task.execute(session, prompter, name, args).await.unwrap()
    }

    // 1) Yalnizca analiz istendiyse kod degistirilemez.
    #[tokio::test]
    async fn analiz_gorevinde_dosya_degistirilememeli() {
        let root = mini_crate("orch_analyze");
        let mut session = trusted_session(&root);
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Analyze));
        // Kullanici her soruya "evet" dese bile soru hic gelmemeli.
        let mut prompter = ScriptedPrompter::with(&[true, true, true]);

        let apply = run(
            &mut task,
            &mut session,
            &mut prompter,
            "apply_patch",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(
            apply.output.starts_with("INTENT_BLOCKED"),
            "{}",
            apply.output
        );
        assert!(apply.stop.is_none(), "model bulgularini raporlayabilmeli");
        let verify = run(
            &mut task,
            &mut session,
            &mut prompter,
            "verify_candidate",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(verify.output.starts_with("INTENT_BLOCKED"));

        assert!(prompter.asked.is_empty(), "{:?}", prompter.asked);
        assert_eq!(main_rs(&root), MINI_MAIN);
        assert!(!root.join(".fiko_backups").exists());
        assert_eq!(task.repair.verify_attempts(), 0, "cargo hic calismamali");
        assert_eq!(task.stats().intent_blocked, 2);
        assert_eq!(task.stats().dispatched, 0);

        // Okuma araclari serbest; gorev raporla tamamlanir.
        let read = run(
            &mut task,
            &mut session,
            &mut prompter,
            "read_file",
            &json!({"path": "src/main.rs"}),
        )
        .await;
        assert_eq!(read.output, MINI_MAIN);
        assert_eq!(task.phase(), TaskPhase::Inspecting);
        task.finish_answer("Bulgular: POTANSIYEL RISK yok.");
        assert_eq!(task.phase(), TaskPhase::Completed);
        assert!(!task.applied());
        assert!(task.status_report().contains("DEGISMEDI"));
        remove(&root);
    }

    // 2) Acik degistirme istegi Modify'a alinir ve guvenli akis sonuna kadar calisir.
    #[tokio::test]
    async fn acik_degistirme_istegi_modify_akisini_calistirmali() {
        let root = mini_crate("orch_modify");
        let mut session = trusted_session(&root);
        let mut task =
            Orchestrator::for_request("src/main.rs icindeki value degerini 2 yap ve duzelt");
        assert_eq!(task.intent(), TaskIntent::Modify);
        assert_eq!(task.authority(), WriteAuthority::Granted);
        let mut prompter = ScriptedPrompter::with(&[true]);

        let verify = run(
            &mut task,
            &mut session,
            &mut prompter,
            "verify_candidate",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(verify.output.starts_with("VERIFY_OK"), "{}", verify.output);
        assert_eq!(task.phase(), TaskPhase::Planning);

        let apply = run(
            &mut task,
            &mut session,
            &mut prompter,
            "apply_patch",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(
            apply.output.starts_with("PATCH_VERIFIED"),
            "{}",
            apply.output
        );
        // 6) Basarili patch sonrasi ajan durur.
        assert!(apply.stop.is_some());
        assert!(task.applied());
        assert_eq!(task.phase(), TaskPhase::Completed);
        let trace = task.repair.execution().trace();
        assert!(trace.contains(&TaskPhase::AwaitingApproval), "{trace:?}");
        assert!(trace.contains(&TaskPhase::Applying), "{trace:?}");
        assert_eq!(prompter.asked.len(), 1);

        // Gorev bittikten sonra yazma yoluna giden cagrilar calistirilmaz.
        let late = run(
            &mut task,
            &mut session,
            &mut prompter,
            "verify_candidate",
            &patch("let value: i32 = 3;"),
        )
        .await;
        assert!(late.output.starts_with("TASK_ENDED"), "{}", late.output);
        assert_eq!(task.stats().dispatched, 2);
        task.finish_answer("PATCH_VERIFIED ile tamamlandi.");
        assert!(task.status_report().contains("uygulandi"));
        assert!(!task.status_report().contains("[UYARI]"));
        remove(&root);
    }

    // 3) Belirsiz istekte yetkisiz dosya degisikligi yapilmaz; karar kullanicinin.
    #[tokio::test]
    async fn belirsiz_istekte_izin_yoksa_dosya_degismemeli() {
        let root = mini_crate("orch_unknown_deny");
        let mut session = trusted_session(&root);
        let mut task = Orchestrator::for_request("main.rs icindeki value");
        assert_eq!(task.intent(), TaskIntent::Unknown);
        assert_eq!(task.authority(), WriteAuthority::Undecided);
        let mut prompter = ScriptedPrompter::with(&[false]);

        let apply = run(
            &mut task,
            &mut session,
            &mut prompter,
            "apply_patch",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(
            apply.output.starts_with("INTENT_BLOCKED"),
            "{}",
            apply.output
        );
        assert_eq!(prompter.asked.len(), 1, "{:?}", prompter.asked);
        assert!(prompter.asked[0].contains("kod degisikligine izin"));
        assert_eq!(task.authority(), WriteAuthority::Denied);
        assert_eq!(main_rs(&root), MINI_MAIN);
        assert!(!root.join(".fiko_backups").exists());
        assert_eq!(task.repair.verify_attempts(), 0);

        // Farkli bir aday da sorulmadan reddedilir.
        let again = run(
            &mut task,
            &mut session,
            &mut prompter,
            "verify_candidate",
            &patch("let value: i32 = 3;"),
        )
        .await;
        assert!(again.output.starts_with("INTENT_BLOCKED"));
        assert_eq!(prompter.asked.len(), 1);
        remove(&root);
    }

    #[tokio::test]
    async fn belirsiz_istekte_izin_verilince_akis_calismali() {
        let root = mini_crate("orch_unknown_allow");
        let mut session = Session::new(root.clone());
        let mut task = Orchestrator::for_request("main.rs icindeki value");
        // Sira: niyet izni -> gecici derleme izni -> patch onayi.
        let mut prompter = ScriptedPrompter::with(&[true, true, true]);

        let apply = run(
            &mut task,
            &mut session,
            &mut prompter,
            "apply_patch",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(
            apply.output.starts_with("PATCH_VERIFIED"),
            "{}",
            apply.output
        );
        assert_eq!(prompter.asked.len(), 3, "{:?}", prompter.asked);
        assert!(prompter.asked[0].contains("kod degisikligine izin"));
        assert!(prompter.asked[1].contains("cargo check"));
        assert!(prompter.asked[2].contains("Patch uygulansin"));
        assert!(main_rs(&root).contains(MINI_GOOD));
        remove(&root);
    }

    // 4) Yinelenen basarisiz cagrilar sinirlandirilir.
    #[tokio::test]
    async fn tekrar_eden_basarisiz_cagri_sinirlandirilmali() {
        let root = mini_crate("orch_repeat_fail");
        let mut session = trusted_session(&root);
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Modify));
        let mut prompter = ScriptedPrompter::with(&[]);
        let args = json!({"path": "src/yok.rs"});

        let first = run(&mut task, &mut session, &mut prompter, "read_file", &args).await;
        assert!(
            first.output.starts_with("ERROR: Dosya bulunamadi"),
            "{}",
            first.output
        );
        assert_eq!(task.stats().dispatched, 1);

        let second = run(&mut task, &mut session, &mut prompter, "read_file", &args).await;
        assert!(
            second.output.contains("daha once basarisiz oldu"),
            "{}",
            second.output
        );
        assert!(second.stop.is_none());
        assert_eq!(task.stats().dispatched, 1, "tekrar calistirilmamali");
        assert_eq!(task.stats().repeated_failures, 1);

        let third = run(&mut task, &mut session, &mut prompter, "read_file", &args).await;
        assert!(third.output.starts_with("LOOP_BLOCKED"), "{}", third.output);
        assert!(third.stop.is_some(), "ucuncu tekrar gorevi durdurmali");
        assert_eq!(task.stats().loop_stops, 1);
        assert_eq!(task.stats().dispatched, 1);
        remove(&root);
    }

    // 5) Gecersiz arac argumanlari kontrollu hata uretir; arac calistirilmaz.
    #[tokio::test]
    async fn gecersiz_argumanlar_kontrollu_hata_uretmeli() {
        let root = mini_crate("orch_invalid_args");
        let mut session = trusted_session(&root);
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Modify));
        let mut prompter = ScriptedPrompter::with(&[true, true]);

        let cases: Vec<(&str, Value, &str)> = vec![
            ("read_file", json!({}), "'path' alani eksik"),
            ("read_file", json!({"path": 42}), "metin olmali"),
            (
                "read_lines",
                json!({"path": "src/main.rs", "start_line": 5, "end_line": 2}),
                "kucuk olamaz",
            ),
            (
                "read_lines",
                json!({"path": "src/main.rs", "start_line": 1, "end_line": 100}),
                "60 satir",
            ),
            (
                "read_lines",
                json!({"path": "src/main.rs", "start_line": "abc", "end_line": 2}),
                "tam sayi olmali",
            ),
            (
                "run_tests",
                json!({"mode": "delete"}),
                "\"check\" veya \"test\"",
            ),
            (
                "apply_patch",
                json!({"path": "src/main.rs", "old_text": "a", "new_text": "a"}),
                "degisiklik yok",
            ),
            ("apply_patch", Value::Null, "JSON nesnesi"),
            ("bilinmeyen_arac", json!({}), "Bilinmeyen arac"),
        ];
        let count = cases.len();
        for (name, args, expected) in cases {
            let outcome = run(&mut task, &mut session, &mut prompter, name, &args).await;
            assert!(
                outcome.output.starts_with("ERROR:") && outcome.output.contains(expected),
                "{name}: {}",
                outcome.output
            );
            assert!(outcome.stop.is_none());
        }
        assert_eq!(task.stats().invalid_args, count);
        assert_eq!(
            task.stats().dispatched,
            0,
            "hatali arguman araca ulasmamali"
        );
        assert!(prompter.asked.is_empty());
        assert_eq!(main_rs(&root), MINI_MAIN);

        // Metin olarak gelen satir numaralari duzeltilir ve arac calisir.
        let coerced = run(
            &mut task,
            &mut session,
            &mut prompter,
            "read_lines",
            &json!({"path": "src/main.rs", "start_line": "1", "end_line": "2"}),
        )
        .await;
        assert!(
            coerced.output.contains("1 | fn main()"),
            "{}",
            coerced.output
        );
        assert_eq!(task.stats().dispatched, 1);
        remove(&root);
    }

    // 9) Gereksiz (tekrar) okumalar olculebilir bicimde azaltilir.
    #[tokio::test]
    async fn ayni_dosya_tekrar_okunursa_kisa_yanit_donmeli() {
        let root = mini_crate("orch_dup_read");
        let mut session = trusted_session(&root);
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Analyze));
        let mut prompter = ScriptedPrompter::with(&[]);
        let read_main = json!({"path": "src/main.rs"});
        let lines = json!({"path": "src/main.rs", "start_line": 1, "end_line": 2});

        let first = run(
            &mut task,
            &mut session,
            &mut prompter,
            "read_file",
            &read_main,
        )
        .await;
        assert_eq!(first.output, MINI_MAIN);
        let second = run(
            &mut task,
            &mut session,
            &mut prompter,
            "read_file",
            &read_main,
        )
        .await;
        assert!(second.output.starts_with("TEKRAR"), "{}", second.output);
        assert!(second.output.contains("cagri #1"));
        assert_eq!(task.stats().duplicate_reads, 1);
        assert_eq!(task.stats().saved_chars, MINI_MAIN.chars().count());

        run(&mut task, &mut session, &mut prompter, "read_lines", &lines).await;
        let again = run(&mut task, &mut session, &mut prompter, "read_lines", &lines).await;
        assert!(again.output.starts_with("TEKRAR"), "{}", again.output);
        assert_eq!(task.stats().duplicate_reads, 2);

        // Dosya degistiyse tam icerik doner (tekrar sayilmaz).
        let other = root.join("src/other.rs");
        fs::write(&other, "pub fn a() {}\n").unwrap();
        let read_other = json!({"path": "src/other.rs"});
        run(
            &mut task,
            &mut session,
            &mut prompter,
            "read_file",
            &read_other,
        )
        .await;
        fs::write(&other, "pub fn b() {}\n").unwrap();
        let changed = run(
            &mut task,
            &mut session,
            &mut prompter,
            "read_file",
            &read_other,
        )
        .await;
        assert_eq!(changed.output, "pub fn b() {}\n");
        assert_eq!(task.stats().duplicate_reads, 2);

        let report = task.status_report();
        assert!(report.contains("tekrar okuma engellendi: 2"), "{report}");
        assert_eq!(task.call_log().len(), 6);
        assert!(task.call_log()[0].contains("read_file [Created->Inspecting] ok"));
        remove(&root);
    }

    // 7) Reddedilen patch sonrasinda basari ilan edilmez.
    #[tokio::test]
    async fn reddedilen_patch_sonrasi_basari_ilan_edilmemeli() {
        let root = mini_crate("orch_rejected");
        let mut session = trusted_session(&root);
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Modify));
        let mut prompter = ScriptedPrompter::with(&[false]);

        run(
            &mut task,
            &mut session,
            &mut prompter,
            "verify_candidate",
            &patch(MINI_GOOD),
        )
        .await;
        let apply = run(
            &mut task,
            &mut session,
            &mut prompter,
            "apply_patch",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(
            apply.output.starts_with("PATCH_REJECTED"),
            "{}",
            apply.output
        );
        assert_eq!(task.phase(), TaskPhase::Blocked);

        // Model yine de basari ilan ediyor: durum degismez, uyari uretilir.
        task.finish_answer("Patch basariyla uygulandi (PATCH_VERIFIED).");
        assert!(!task.applied());
        assert_eq!(task.phase(), TaskPhase::Blocked);
        let report = task.status_report();
        assert!(report.contains("DEGISMEDI"), "{report}");
        assert!(report.contains("[UYARI]"), "{report}");
        assert_eq!(main_rs(&root), MINI_MAIN);
        remove(&root);
    }

    #[test]
    fn model_tamamlandi_ilan_ederek_durumu_degistiremez() {
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Modify));
        task.finish_answer("PATCH_VERIFIED: degisiklik uygulandi, gorev tamamlandi.");
        assert_eq!(task.phase(), TaskPhase::Failed);
        assert!(!task.applied());
        assert!(task.status_report().contains("[UYARI]"));

        // Salt analiz gorevi raporla tamamlanir; yine de proje DEGISMEDI yazilir.
        let mut analysis = Orchestrator::new(IntentDecision::fixed(TaskIntent::Analyze));
        analysis.finish_answer("Tamamlandi.");
        assert_eq!(analysis.phase(), TaskPhase::Completed);
        assert!(analysis.status_report().contains("DEGISMEDI"));
    }

    // 8) Kullanici dogrulama iznini reddedince v0.8 kilidi orkestrator uzerinden de calisir.
    #[tokio::test]
    async fn izin_reddi_kilidi_orkestrator_uzerinden_de_calistirmali() {
        let root = mini_crate("orch_lock");
        let mut session = Session::new(root.clone());
        let mut task = Orchestrator::new(IntentDecision::fixed(TaskIntent::Modify));
        let mut prompter = ScriptedPrompter::with(&[false]);

        let apply = run(
            &mut task,
            &mut session,
            &mut prompter,
            "apply_patch",
            &patch(MINI_GOOD),
        )
        .await;
        assert!(
            apply.output.starts_with("APPLY_BLOCKED"),
            "{}",
            apply.output
        );
        assert!(apply.stop.is_some());
        assert_eq!(task.phase(), TaskPhase::Blocked);
        assert_eq!(prompter.asked.len(), 1);
        assert!(prompter.asked[0].contains("cargo check"));
        assert_eq!(main_rs(&root), MINI_MAIN);
        assert!(!root.join(".fiko_backups").exists());
        remove(&root);
    }

    #[test]
    fn cagri_anahtari_alan_sirasindan_bagimsiz_olmali() {
        let a = call_key(
            "read_lines",
            &json!({"path": "a", "start_line": 1, "end_line": 2}),
        );
        let b = call_key(
            "read_lines",
            &json!({"end_line": 2, "start_line": 1, "path": "a"}),
        );
        assert_eq!(a, b);
        assert_ne!(a, call_key("read_file", &json!({"path": "a"})));
    }
}
