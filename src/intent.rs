//! v0.9 Task Intent Engine.
//!
//! Kullanicinin istegini bes gorev turunden birine ayirir. Siniflandirma tek
//! bir anahtar kelime listesine degil, birlikte degerlendirilen sinyallere
//! dayanir: Turkce/Ingilizce fiil kokleri (olumsuz ve edilgen cekimler haric),
//! soru bicimi, salt-okunur koruma kaliplari ("degistirme", "sadece incele")
//! ve kod tanimlayicilarinin ayiklanmasi (`add_user`, `fix()` fiil sayilmaz).
//! Sinyaller celisirse sonuc `Unknown`dur.
//!
//! Dosya degistirme yetkisi buradan turetilir ve Rust tarafinda uygulanir
//! (bkz. orchestrator): yalnizca `Modify` yetki verir; `Unknown` icin karar
//! kullaniciya sorulur; diger turlerde apply_patch gercek dosyaya yazamaz.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskIntent {
    /// Kodu incele, degisiklik yapma.
    Analyze,
    /// Kodun davranisini acikla.
    Explain,
    /// Kod degisikligi acikca istendi.
    Modify,
    /// Mevcut kodu derle / test et.
    Test,
    /// Niyet yeterince acik degil.
    Unknown,
}

/// Bu gorevde gercek dosyaya yazma yetkisi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAuthority {
    Granted,
    Denied,
    /// Belirsiz istek: ilk yazma girisiminde kullaniciya sorulur.
    Undecided,
}

impl TaskIntent {
    pub fn label(self) -> &'static str {
        match self {
            TaskIntent::Analyze => "Analyze",
            TaskIntent::Explain => "Explain",
            TaskIntent::Modify => "Modify",
            TaskIntent::Test => "Test",
            TaskIntent::Unknown => "Unknown",
        }
    }

    pub fn write_authority(self) -> WriteAuthority {
        match self {
            TaskIntent::Modify => WriteAuthority::Granted,
            TaskIntent::Unknown => WriteAuthority::Undecided,
            TaskIntent::Analyze | TaskIntent::Explain | TaskIntent::Test => WriteAuthority::Denied,
        }
    }

    /// Model icin kisa yol gosterici; system prompt'u buyutmek yerine goreve eklenir.
    pub fn guidance(self) -> &'static str {
        match self {
            TaskIntent::Analyze => {
                "Bu gorev salt inceleme: dosya DEGISTIRILMEZ; verify_candidate ve apply_patch bu gorevde kullanilamaz. Ilgili kodu oku, bulgulari KANITLANMIS HATA / POTANSIYEL RISK olarak raporla."
            }
            TaskIntent::Explain => {
                "Bu gorev aciklama: dosya DEGISTIRILMEZ; verify_candidate ve apply_patch bu gorevde kullanilamaz. Ilgili kodu oku ve davranisini yalnizca okudugun satirlara dayanarak acikla."
            }
            TaskIntent::Test => {
                "Bu gorev derleme/test: dosya DEGISTIRILMEZ. run_tests ile gercek projede cargo check/test calistir (kullanici onayi istenir) ve ciktiyi oldugu gibi raporla."
            }
            TaskIntent::Modify => {
                "Kod degisikligi istendi. Sira: search_code/read_lines ile kodu bul -> verify_candidate -> VERIFY_OK sonrasi BIREBIR AYNI path/old_text/new_text ile apply_patch. Ayni dosyayi tekrar okuma."
            }
            TaskIntent::Unknown => {
                "Gorev turu belirsiz. Once kodu incele ve bulgulari raporla. Kod degisikligi gerekiyorsa verify_candidate cagir; Rust tarafi kullanicidan izin ister, izin yoksa degisiklik yapilamaz."
            }
        }
    }
}

impl fmt::Display for TaskIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Low => "dusuk",
            Confidence::Medium => "orta",
            Confidence::High => "yuksek",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scores {
    pub analyze: u32,
    pub explain: u32,
    pub modify: u32,
    pub test: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentDecision {
    pub intent: TaskIntent,
    pub confidence: Confidence,
    /// Karari dogruran sinyaller (insan okunur, console ve testler icin).
    pub evidence: Vec<String>,
    pub scores: Scores,
}

impl IntentDecision {
    /// Testler ve dogrudan kurulum icin: kanitsiz, sabit karar.
    pub fn fixed(intent: TaskIntent) -> Self {
        Self {
            intent,
            confidence: Confidence::High,
            evidence: vec!["sabit".into()],
            scores: Scores::default(),
        }
    }

    pub fn evidence_line(&self) -> String {
        if self.evidence.is_empty() {
            "sinyal yok".to_string()
        } else {
            self.evidence.join(", ")
        }
    }
}

// ============================================================
// SINYAL TABLOLARI
// ============================================================
const W_MODIFY: u32 = 3;
const W_TEST: u32 = 3;
const W_ANALYZE: u32 = 2;
const W_EXPLAIN: u32 = 2;
const W_QUESTION_MARK: u32 = 1;

/// Turkce kokler: on ek eslesmesi (cekim ekleri serbest), olumsuz/edilgen haric.
const MODIFY_TR: &[&str] = &[
    "duzelt",
    "degistir",
    "ekle",
    "sil",
    "kaldir",
    "guncelle",
    "duzenle",
    "onar",
    "tamir",
    "iyilestir",
    "optimize",
    "refaktor",
    "uygula",
    "olustur",
    "adlandir",
    "donustur",
    "tasi",
];
/// "yaz" koku cok anlamli (yaziyor, yazilim); yalnizca emir/istek bicimleri sayilir.
const WRITE_TR_EXACT: &[&str] = &[
    "yaz",
    "yazalim",
    "yazabilir",
    "yazsana",
    "yazin",
    "yaziniz",
    "yazar",
];
const MODIFY_EN: &[&str] = &[
    "fix",
    "change",
    "modify",
    "add",
    "remove",
    "delete",
    "rename",
    "replace",
    "implement",
    "update",
    "patch",
    "refactor",
    "create",
    "write",
    "move",
    "insert",
    "convert",
    "rewrite",
    "edit",
];
const ANALYZE_TR: &[&str] = &[
    "incele",
    "analiz",
    "kontrol",
    "bak",
    "bul",
    "ara",
    "listele",
    "goster",
    "oku",
    "tara",
    "denetle",
    "ozetle",
    "arastir",
    "degerlendir",
    "tespit",
    "gozden",
];
const ANALYZE_EN: &[&str] = &[
    "review",
    "analyze",
    "analyse",
    "inspect",
    "find",
    "search",
    "list",
    "show",
    "read",
    "scan",
    "audit",
    "summarize",
    "check",
    "look",
    "examine",
    "evaluate",
    "detect",
    "identify",
    "locate",
    "investigate",
];
const EXPLAIN_TR: &[&str] = &[
    "acikla", "anlat", "nedir", "neden", "nasil", "anlam", "yorumla", "niye", "nicin",
];
const EXPLAIN_EN: &[&str] = &[
    "explain", "why", "how", "what", "describe", "clarify", "meaning",
];
const TEST_TR: &[&str] = &["test", "derle", "calistir"];
const TEST_EN: &[&str] = &[
    "test",
    "tests",
    "compile",
    "build",
    "run",
    "cargo",
    "clippy",
    "benchmark",
];
/// Soru kaliplari (iki kelime): acik aciklama istegi.
const EXPLAIN_PHRASES: &[&str] = &[
    "ne yapar",
    "ne yapiyor",
    "ne ise yarar",
    "ne is yapar",
    "what does",
    "how does",
];
/// Analiz kaliplari: "hata var mi" gibi inceleme sorulari.
const ANALYZE_PHRASES: &[&str] = &[
    "hata var mi",
    "sorun var mi",
    "bug var mi",
    "any bugs",
    "any issues",
];
/// Salt-okunur koruma: tek kelimeler tam eslesir, kaliplar metin icinde aranir.
const GUARD_TOKENS: &[&str] = &["degistirme", "degistirmeden", "dokunma", "dokunmadan"];
const GUARD_PHRASES: &[&str] = &[
    "degisiklik yapma",
    "degisiklik yapmadan",
    "kod yazma",
    "sadece incele",
    "sadece acikla",
    "sadece analiz",
    "sadece oku",
    "sadece bak",
    "sadece kontrol",
    "sadece rapor",
    "yalnizca incele",
    "yalnizca acikla",
    "yalnizca analiz",
    "yalnizca oku",
    "yalnizca bak",
    "yalnizca kontrol",
    "yalnizca rapor",
    "don't change",
    "do not change",
    "don't modify",
    "do not modify",
    "without changing",
    "without modifying",
    "read-only",
    "read only",
    "only explain",
    "only analyze",
    "only review",
    "just explain",
    "just analyze",
    "no changes",
    "no modifications",
];

// ============================================================
// METIN NORMALIZASYONU
// ============================================================
fn fold_char(c: char) -> char {
    match c {
        'ç' => 'c',
        'ğ' => 'g',
        'ı' => 'i',
        'ö' => 'o',
        'ş' => 's',
        'ü' => 'u',
        'â' => 'a',
        'î' => 'i',
        'û' => 'u',
        other => other,
    }
}

/// Kucuk harf + Turkce karakterleri ASCII'ye indirger; bosluklari tekler.
pub(crate) fn fold(text: &str) -> String {
    let folded: String = text
        .chars()
        .map(|c| if c == 'İ' { 'i' } else { c })
        .flat_map(char::to_lowercase)
        .filter(|c| *c != '\u{307}')
        .map(fold_char)
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Kod adi mi? (`add_user`, `fix()`, `src/main.rs`, `` `fix` ``) Fiil olarak sayilmaz.
fn is_identifier(word: &str) -> bool {
    word.contains(['_', '(', ')', ':', '/', '\\', '`', '.', '"', '\''])
        || word.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn tokens(folded: &str) -> Vec<String> {
    folded
        .split(' ')
        .filter_map(|word| {
            let word = word.trim_matches(|c: char| matches!(c, ',' | ';' | '!' | '?'));
            // Cumle sonu noktasi tanimlayici sayilmasin; "main.rs" gibi ic nokta sayilsin.
            let word = word.strip_suffix('.').unwrap_or(word);
            if word.is_empty() || is_identifier(word) {
                None
            } else {
                Some(word.to_string())
            }
        })
        .collect()
}

/// Turkce kok eslesmesi: kokten sonra gelen ek olumsuz ("-me", "-mez") ya da
/// edilgen/ortac ("-il", "-in", "-en") ise emir sayilmaz. "-meli" (gereklilik) sayilir.
fn turkish_stem_match(token: &str, stem: &str) -> bool {
    let Some(rest) = token.strip_prefix(stem) else {
        return false;
    };
    if rest.len() > 8 {
        return false;
    }
    if rest.starts_with("meli") || rest.starts_with("mali") {
        return true;
    }
    let negative = rest == "me"
        || rest == "ma"
        || ["mey", "may", "mez", "maz", "med", "mad", "mes", "mas"]
            .iter()
            .any(|suffix| rest.starts_with(suffix));
    let passive = ["il", "in", "en", "n"]
        .iter()
        .any(|suffix| rest.starts_with(suffix));
    !(negative || passive)
}

fn english_match(token: &str, word: &str) -> bool {
    token == word || token.strip_suffix('s') == Some(word)
}

fn score_group(
    tokens: &[String],
    turkish: &[&str],
    english: &[&str],
    weight: u32,
    evidence: &mut Vec<String>,
) -> u32 {
    let mut score = 0;
    for token in tokens {
        let hit = turkish
            .iter()
            .find(|stem| turkish_stem_match(token, stem))
            .or_else(|| english.iter().find(|word| english_match(token, word)));
        if let Some(stem) = hit {
            score += weight;
            evidence.push((*stem).to_string());
        }
    }
    score
}

fn guard(folded: &str, tokens: &[String]) -> Option<String> {
    if let Some(token) = tokens
        .iter()
        .find(|token| GUARD_TOKENS.contains(&token.as_str()))
    {
        return Some(token.clone());
    }
    GUARD_PHRASES
        .iter()
        .find(|phrase| folded.contains(*phrase))
        .map(|phrase| (*phrase).to_string())
}

// ============================================================
// SINIFLANDIRMA
// ============================================================
pub fn classify(request: &str) -> IntentDecision {
    let folded = fold(request);
    let tokens = tokens(&folded);
    let mut evidence = Vec::new();
    let mut scores = Scores {
        modify: score_group(&tokens, MODIFY_TR, MODIFY_EN, W_MODIFY, &mut evidence),
        analyze: score_group(&tokens, ANALYZE_TR, ANALYZE_EN, W_ANALYZE, &mut evidence),
        explain: score_group(&tokens, EXPLAIN_TR, EXPLAIN_EN, W_EXPLAIN, &mut evidence),
        test: score_group(&tokens, TEST_TR, TEST_EN, W_TEST, &mut evidence),
    };
    if let Some(word) = tokens
        .iter()
        .find(|token| WRITE_TR_EXACT.contains(&token.as_str()))
    {
        scores.modify += W_MODIFY;
        evidence.push(word.clone());
    }

    for phrase in EXPLAIN_PHRASES {
        if folded.contains(phrase) {
            scores.explain += W_EXPLAIN;
            evidence.push((*phrase).to_string());
        }
    }
    for phrase in ANALYZE_PHRASES {
        if folded.contains(phrase) {
            scores.analyze += W_ANALYZE;
            evidence.push((*phrase).to_string());
        }
    }
    if folded.ends_with('?') {
        scores.explain += W_QUESTION_MARK;
        evidence.push("soru isareti".into());
    }

    if let Some(found) = guard(&folded, &tokens) {
        // Kullanici acikca degisiklik istemiyor: yazma sinyalleri gecersiz.
        scores.modify = 0;
        evidence.push(format!("salt-okunur koruma: '{found}'"));
    }

    let read_only_top = scores.test.max(scores.analyze).max(scores.explain);
    let (intent, top, runner_up) = if scores.modify > 0 {
        if scores.modify > scores.explain {
            // Inceleme/test fiilleri degisiklik icin hazirlik sayilir; yalnizca
            // soru bicimi (aciklama istegi) yazma niyetiyle celisir.
            (TaskIntent::Modify, scores.modify, scores.explain)
        } else {
            evidence.push("celisen sinyal: degisiklik ve aciklama".into());
            (TaskIntent::Unknown, scores.modify, scores.explain)
        }
    } else if read_only_top == 0 {
        (TaskIntent::Unknown, 0, 0)
    } else if scores.test == read_only_top {
        (
            TaskIntent::Test,
            scores.test,
            scores.analyze.max(scores.explain),
        )
    } else if scores.analyze == read_only_top {
        (
            TaskIntent::Analyze,
            scores.analyze,
            scores.test.max(scores.explain),
        )
    } else {
        (
            TaskIntent::Explain,
            scores.explain,
            scores.test.max(scores.analyze),
        )
    };

    let confidence = if intent == TaskIntent::Unknown {
        Confidence::Low
    } else if top >= 3 && top - runner_up >= 2 {
        Confidence::High
    } else {
        Confidence::Medium
    };

    IntentDecision {
        intent,
        confidence,
        evidence,
        scores,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent_of(text: &str) -> TaskIntent {
        let decision = classify(text);
        eprintln!("{text:?} -> {decision:?}");
        decision.intent
    }

    #[test]
    fn inceleme_istegi_analyze_olmali() {
        assert_eq!(
            intent_of("src/main.rs dosyasini incele ve hatalari bul"),
            TaskIntent::Analyze
        );
        assert_eq!(intent_of("Kodu gözden geçir"), TaskIntent::Analyze);
        assert_eq!(intent_of("bu kodda hata var mi?"), TaskIntent::Analyze);
        assert_eq!(intent_of("review src/tools.rs"), TaskIntent::Analyze);
    }

    #[test]
    fn aciklama_istegi_explain_olmali() {
        assert_eq!(intent_of("Bu fonksiyon ne yapar?"), TaskIntent::Explain);
        assert_eq!(intent_of("neden main.rs derlenmiyor?"), TaskIntent::Explain);
        assert_eq!(
            intent_of("read_lines fonksiyonunu açıkla"),
            TaskIntent::Explain
        );
        assert_eq!(
            intent_of("explain what the prepare_patch function does"),
            TaskIntent::Explain
        );
    }

    #[test]
    fn acik_degistirme_istegi_modify_olmali() {
        let decision = classify("main.rs'deki hatayı düzelt");
        assert_eq!(decision.intent, TaskIntent::Modify);
        assert_eq!(decision.confidence, Confidence::High);
        assert!(decision.evidence.iter().any(|e| e == "duzelt"));
        assert_eq!(
            intent_of("value degerini 2 olarak degistir"),
            TaskIntent::Modify
        );
        assert_eq!(intent_of("fix the bug in main.rs"), TaskIntent::Modify);
        assert_eq!(intent_of("Düzeltebilir misin?"), TaskIntent::Modify);
        assert_eq!(intent_of("bu kodu duzeltmelisin"), TaskIntent::Modify);
        assert_eq!(
            intent_of("kodu incele, hatalari bul ve duzelt"),
            TaskIntent::Modify
        );
        assert_eq!(intent_of("read_lines icin test yaz"), TaskIntent::Modify);
    }

    #[test]
    fn test_istegi_test_olmali() {
        assert_eq!(intent_of("testleri çalıştır"), TaskIntent::Test);
        assert_eq!(
            intent_of("cargo test calistir ve sonucu soyle"),
            TaskIntent::Test
        );
        assert_eq!(intent_of("run the tests"), TaskIntent::Test);
    }

    #[test]
    fn sinyalsiz_istek_unknown_olmali() {
        for text in [
            "",
            "src/main.rs",
            "selam",
            "value = 2",
            "main.rs icindeki value",
        ] {
            let decision = classify(text);
            assert_eq!(decision.intent, TaskIntent::Unknown, "{text:?}");
            assert_eq!(decision.confidence, Confidence::Low);
        }
    }

    #[test]
    fn celisen_sinyal_unknown_olmali() {
        // Soru bicimi + degisiklik fiili: kullanici tarif mi istiyor, degisiklik mi?
        assert_eq!(intent_of("nasıl düzeltirim?"), TaskIntent::Unknown);
    }

    #[test]
    fn salt_okunur_koruma_yazma_sinyalini_silmeli() {
        assert_eq!(intent_of("kodu incele ama değiştirme"), TaskIntent::Analyze);
        assert_eq!(
            intent_of("sadece açıkla, değişiklik yapma"),
            TaskIntent::Explain
        );
        assert_eq!(
            intent_of("review main.rs, do not modify anything"),
            TaskIntent::Analyze
        );
        let decision = classify("hatayi duzelt ama testlere dokunma");
        // "dokunma" korumasi tum yazma sinyallerini kaldirir: belirsizlik kullaniciya sorulur.
        assert_ne!(decision.intent, TaskIntent::Modify);
    }

    #[test]
    fn kod_tanimlayicilari_fiil_sayilmamali() {
        assert_eq!(
            intent_of("add_user fonksiyonu neden panic ediyor?"),
            TaskIntent::Explain
        );
        assert_eq!(intent_of("`fix` fonksiyonunu açıkla"), TaskIntent::Explain);
        assert_eq!(
            intent_of("src/fix.rs dosyasini incele"),
            TaskIntent::Analyze
        );
    }

    #[test]
    fn edilgen_ve_olumsuz_cekimler_emir_sayilmamali() {
        assert_eq!(intent_of("eklenen fonksiyonu açıkla"), TaskIntent::Explain);
        assert_eq!(intent_of("silinen satirlari incele"), TaskIntent::Analyze);
        assert!(turkish_stem_match("duzeltebilir", "duzelt"));
        assert!(turkish_stem_match("degistirmeli", "degistir"));
        assert!(!turkish_stem_match("degistirmedi", "degistir"));
        assert!(!turkish_stem_match("eklenmis", "ekle"));
    }

    #[test]
    fn yazma_yetkisi_yalnizca_modify_ile_verilmeli() {
        assert_eq!(
            TaskIntent::Modify.write_authority(),
            WriteAuthority::Granted
        );
        assert_eq!(
            TaskIntent::Unknown.write_authority(),
            WriteAuthority::Undecided
        );
        for intent in [TaskIntent::Analyze, TaskIntent::Explain, TaskIntent::Test] {
            assert_eq!(intent.write_authority(), WriteAuthority::Denied);
        }
    }

    #[test]
    fn turkce_karakterler_katlanmali() {
        assert_eq!(fold("Düzelt  ŞİMDİ, Çabuk"), "duzelt simdi, cabuk");
        assert_eq!(fold("Işık"), "isik");
    }
}
