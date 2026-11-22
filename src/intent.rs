//! v0.9.1 Intelligent Intent Engine.
//!
//! Kullanicinin istegini alti gorev turunden birine ayirir. Siniflandirma tek
//! bir anahtar kelime listesine degil, birlikte degerlendirilen sinyallere
//! dayanir: Turkce/Ingilizce fiil kokleri (olumsuz ve edilgen cekimler haric),
//! soru bicimi, salt-okunur koruma kaliplari ("degistirme", "sadece incele")
//! ve kod tanimlayicilarinin ayiklanmasi (`add_user`, `fix()` fiil sayilmaz).
//! Oncelik: acik yazmama talimati tum yazma sinyallerini gecersiz kilar;
//! acik dosya degisikligi `Modify` olarak testten once gelir; yanitta kod
//! uretimi `Generate`, gercek calistirma istegi `Test`, ardindan Analyze ve
//! Explain gelir. Sadece bir eylemden bahsetmek o eylemi istemek degildir.
//! Incele/acikla ile baslayan eylem cumlesinde anlatilan fiiller yetki vermez;
//! "nasil/how" degisiklik sorusu, soru isareti olmasa da belirsizdir.
//! Belirsiz dosya olusturma/yazmama celiskisi `Unknown` ve yazma yasagidir.
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
    /// Kodu yalnizca yanitta uret; arac, dosya yazimi ve dogrulama yok.
    Generate,
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
            TaskIntent::Generate => "Generate",
            TaskIntent::Modify => "Modify",
            TaskIntent::Test => "Test",
            TaskIntent::Unknown => "Unknown",
        }
    }

    pub fn write_authority(self) -> WriteAuthority {
        match self {
            TaskIntent::Modify => WriteAuthority::Granted,
            TaskIntent::Unknown => WriteAuthority::Undecided,
            TaskIntent::Analyze | TaskIntent::Explain | TaskIntent::Generate | TaskIntent::Test => {
                WriteAuthority::Denied
            }
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
            TaskIntent::Generate => {
                "Bu gorev yalnizca yanitta kod uretimidir. HICBIR arac cagirma; dosya olusturma/degistirme, derleme/test, verify_candidate ve apply_patch kullanma, patch onayi isteme. Istenen eksiksiz kodu istenen yanit biciminde sun. Kod metni uretmek dosya yazimi veya dogrulama degildir; dosya olusturuldugunu, kodun calistirildigini ya da testlerin gectigini iddia etme."
            }
            TaskIntent::Test => {
                "Bu gorev derleme/test: dosya DEGISTIRILMEZ. run_tests ile gercek projede cargo check/test calistir (kullanici onayi istenir) ve ciktiyi oldugu gibi raporla."
            }
            TaskIntent::Modify => {
                "Kod degisikligi istendi. search_code/read_lines ile kodu bul. Tek dosyada verify_candidate -> ayni path/old_text/new_text ile apply_patch. Birden fazla dosya veya yeni dosya icin verify_workspace -> BIREBIR AYNI changes ile apply_workspace; tum hedefler tek kullanici onayina sunulur. Ayni dosyayi tekrar okuma."
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
    pub generate: u32,
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
    /// Acik yazmama talimati: Unknown olsa bile model yeniden izin isteyemez.
    pub write_forbidden: bool,
}

impl IntentDecision {
    /// Testler ve dogrudan kurulum icin: kanitsiz, sabit karar.
    pub fn fixed(intent: TaskIntent) -> Self {
        Self {
            intent,
            confidence: Confidence::High,
            evidence: vec!["sabit".into()],
            scores: Scores::default(),
            write_forbidden: false,
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
const W_GENERATE: u32 = 3;
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
    "kaydet",
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
    "yazmani",
    "yazmanizi",
    "yazmami",
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
    "save",
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
const GENERATE_TR: &[&str] = &["uret", "tasarla", "olustur", "hazirla", "goster", "sun"];
const GENERATE_EN: &[&str] = &[
    "generate", "create", "write", "design", "show", "provide", "produce",
];
const CODE_TARGETS: &[&str] = &[
    "html",
    "html5",
    "css",
    "css3",
    "javascript",
    "typescript",
    "rust",
    "python",
    "sql",
    "code",
    "kod",
    "snippet",
    "function",
    "fonksiyon",
    "component",
    "bilesen",
    "layout",
    "landing",
    "page",
    "sayfa",
    "script",
    "query",
    "sorgu",
    "configuration",
    "config",
    "yapilandirma",
    "example",
    "ornek",
];
/// Soru kaliplari (iki kelime): acik aciklama istegi.
const EXPLAIN_PHRASES: &[&str] = &[
    "ne demek",
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
const GUARD_TOKENS: &[&str] = &[
    "degistirme",
    "degistirmeden",
    "dokunma",
    "dokunmadan",
    "olusturma",
    "olusturmadan",
    "yazma",
    "yazmadan",
    "kaydetme",
    "kaydetmeden",
    "silme",
    "guncelleme",
];
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
    "do not create",
    "do not write",
    "do not save",
    "do not edit",
    "do not update",
    "do not delete",
    "without writing",
    "without saving",
    "without creating",
    "no filesystem modifications",
    "no file changes",
    "no writes",
    "write nothing to disk",
    "avoid changing",
    "avoid modifying",
    "avoid writing",
    "avoid creating",
    "do not touch",
    "don't touch",
    "never touch",
    "without touching",
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
            let word = word.trim_end_matches(['.', ':']);
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
    let passive = ["il", "in", "en", "n"]
        .iter()
        .any(|suffix| rest.starts_with(suffix));
    !(turkish_negative_suffix(rest) || passive)
}

fn turkish_negative_suffix(rest: &str) -> bool {
    rest == "me"
        || rest == "ma"
        || [
            "mey", "may", "mez", "maz", "med", "mad", "mes", "mas", "meme", "mama",
        ]
        .iter()
        .any(|suffix| rest.starts_with(suffix))
}

fn english_match(token: &str, word: &str) -> bool {
    token == word || token.strip_suffix('s') == Some(word)
}

fn english_in(token: &str, group: &[&str]) -> bool {
    group.iter().any(|word| english_match(token, word))
}

fn modification_word(word: &str) -> bool {
    english_in(word, MODIFY_EN)
        || MODIFY_TR.iter().any(|stem| turkish_stem_match(word, stem))
        || WRITE_TR_EXACT.contains(&word)
}

/// "Guncelleme yap"ta -me isim-fiildir; tek basina "guncelleme" yasaktir.
fn noun_action(words: &[String], index: usize) -> bool {
    words
        .get(index + 1)
        .is_some_and(|word| turkish_stem_match(word, "yap"))
}

fn score_group(
    tokens: &[String],
    turkish: &[&str],
    english: &[&str],
    weight: u32,
    evidence: &mut Vec<String>,
) -> u32 {
    let mut score = 0;
    for (index, token) in tokens.iter().enumerate() {
        let hit = turkish
            .iter()
            .find(|stem| {
                turkish_stem_match(token, stem)
                    || noun_action(tokens, index)
                        && token
                            .strip_prefix(**stem)
                            .is_some_and(|rest| rest == "ma" || rest == "me")
            })
            .or_else(|| english.iter().find(|word| english_match(token, word)));
        if let Some(stem) = hit {
            score += weight;
            evidence.push((*stem).to_string());
        }
    }
    score
}

fn guard(folded: &str, tokens: &[String]) -> Option<String> {
    if let Some((_, token)) = tokens.iter().enumerate().find(|(index, token)| {
        GUARD_TOKENS.contains(&token.as_str()) && !noun_action(tokens, *index)
    }) {
        return Some(token.clone());
    }
    GUARD_PHRASES
        .iter()
        .find(|phrase| phrase_match(folded, phrase))
        .map(|phrase| (*phrase).to_string())
        .or_else(|| {
            clauses(folded).into_iter().find_map(|clause| {
                let words = self::tokens(&clause);
                let refused = words
                    .iter()
                    .any(|word| ["istemiyorum", "istemem", "istemiyoruz"].contains(&word.as_str()));
                let denied_english = english_negation(&words)
                    && words
                        .iter()
                        .any(|word| english_in(word, MODIFY_EN) || word == "touch");
                let denied_turkish = words.iter().enumerate().any(|(index, word)| {
                    if noun_action(&words, index) {
                        return false;
                    }
                    MODIFY_TR.iter().chain(["yaz", "dokun"].iter()).any(|stem| {
                        word.strip_prefix(stem).is_some_and(turkish_negative_suffix)
                            || (refused && turkish_stem_match(word, stem))
                    })
                });
                (denied_english || denied_turkish).then_some(clause)
            })
        })
}

/// Yalniz mevcut proje kokunun disini hedefleyen yasak, kok icindeki yazma
/// istegini iptal etmez. Ek hedef/eylem varsa muafiyet uygulanmaz; asil yol
/// guvenligi yine discovery/patch katmaninda zorunludur.
fn outside_workspace_only(group: &str) -> bool {
    let Some((before, _)) = group.split_once(" outside ") else {
        return false;
    };
    if before
        .split_whitespace()
        .any(|word| is_identifier(word.trim_end_matches(['.', ':'])))
    {
        return false;
    }
    let words = tokens(group);
    let Some(outside) = words.iter().position(|word| word == "outside") else {
        return false;
    };
    let scope = &words[outside + 1..];
    let Some((noun, qualifiers)) = scope.split_last() else {
        return false;
    };
    if !["project", "workspace", "repository"].contains(&noun.as_str())
        || qualifiers.is_empty()
        || qualifiers.len() > 2
        || !qualifiers
            .iter()
            .all(|word| ["this", "the", "current", "my", "our"].contains(&word.as_str()))
    {
        return false;
    }
    words[..outside].iter().all(|word| {
        english_in(word, MODIFY_EN)
            || word.strip_suffix("ing").is_some_and(|stem| {
                MODIFY_EN
                    .iter()
                    .any(|action| stem == *action || action.strip_suffix('e') == Some(stem))
            })
            || [
                "do",
                "not",
                "never",
                "without",
                "please",
                "you",
                "must",
                "should",
                "shall",
                "cannot",
                "any",
                "the",
                "file",
                "files",
                "anything",
                "code",
                "changes",
                "modifications",
            ]
            .contains(&word.as_str())
    })
}

/// Kaliplar tam sozcuk sinirinda eslesir: "kod yazma" bir yasaktir,
/// "kod yazmani istiyorum" ise olumlu istektir.
fn phrase_match(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + phrase.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Cumle/eylem sinirlari olumsuzlugun komsu olumlu istege yayilmasini onler.
/// Dosya adlarinin icindeki noktalara dokunulmaz.
fn command_groups(folded: &str) -> Vec<String> {
    let text = folded.replace(". ", "; ");
    let mut groups = Vec::new();
    for sentence in text.split([';', '!', '?']) {
        let mut group = Vec::new();
        for word in sentence.split_whitespace() {
            let boundary = fold(word.trim_matches(','));
            let discard = ["but", "ama", "ancak"].contains(&boundary.as_str());
            let retain =
                ["without", "only", "just", "sadece", "yalnizca"].contains(&boundary.as_str());
            if (discard || retain) && !group.is_empty() {
                groups.push(group.join(" "));
                group.clear();
            }
            if !discard {
                group.push(word);
            }
        }
        if !group.is_empty() {
            groups.push(group.join(" "));
        }
    }
    groups
}

fn clauses(folded: &str) -> Vec<String> {
    let mut result = Vec::new();
    for sentence in command_groups(folded) {
        // Inceleme nesnesindeki "create and update functions" isimleri emir
        // degildir. Ayiklama baglaclardan once yapilir; Turkce fiil sonda olabilir.
        let sentence = mask_function_names(&sentence);
        for segment in sentence.split(',').flat_map(|part| part.split(" ve ")) {
            let mut negative = false;
            for clause in segment
                .split(" and ")
                .map(str::trim)
                .filter(|c| !c.is_empty())
            {
                // "Do not test and modify files" belirsizliginde ikinci eylemi
                // olumlu yetkiye cevirmeyiz. Yeni cumle veya "but" kapsami bitirir.
                let denied = english_negation(&tokens(clause));
                result.push(if negative {
                    format!("do not {clause}")
                } else {
                    clause.to_owned()
                });
                negative |= denied;
            }
        }
    }
    result
}

fn english_negation(words: &[String]) -> bool {
    let action = words.iter().position(|word| {
        english_in(word, MODIFY_EN)
            || english_in(word, GENERATE_EN)
            || english_in(word, ANALYZE_EN)
            || english_in(
                word,
                &[
                    "explain", "describe", "clarify", "run", "execute", "test", "compile", "build",
                    "touch",
                ],
            )
    });
    action.is_some_and(|action| english_negation_at(words, action))
}

fn english_negation_at(words: &[String], action: usize) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        ["not", "never", "without", "no", "cannot"].contains(&word.as_str()) && index <= action + 1
    })
}

/// Acik inceleme/aciklama cumlesinde fonksiyon adlarini kod tanimlayicisi
/// gibi isaretler. Ayri bir dosya emri korunur; yalnizca fonksiyon adinin
/// hemen onundeki ad/listesi ayiklanir.
fn mask_function_names(sentence: &str) -> String {
    let has_read = tokens(sentence).iter().any(|word| {
        english_in(word, ANALYZE_EN)
            || english_in(word, EXPLAIN_EN)
            || ANALYZE_TR
                .iter()
                .chain(EXPLAIN_TR)
                .any(|stem| turkish_stem_match(word, stem))
    });
    if !has_read {
        return sentence.to_owned();
    }
    let mut words: Vec<_> = sentence.split_whitespace().map(str::to_owned).collect();
    for index in 1..words.len() {
        let noun = words[index].trim_matches(|c: char| !c.is_alphanumeric());
        if !["function", "functions", "method", "methods"].contains(&noun)
            && !["fonksiyon", "metot", "islev"]
                .iter()
                .any(|stem| noun.starts_with(stem))
        {
            continue;
        }
        let mut name = index - 1;
        loop {
            let word = words[name].trim_matches(|c: char| !c.is_alphanumeric());
            if !modification_word(word) {
                break;
            }
            words[name] = format!("`{}`", words[name]);
            if name > 0 && words[name - 1].ends_with(',') {
                name -= 1;
            } else if name >= 2 && ["and", "ve"].contains(&words[name - 1].as_str()) {
                name -= 2;
            } else {
                break;
            }
        }
    }
    words.join(" ")
}

fn code_target(words: &[String], clause: &str) -> bool {
    words.iter().any(|word| {
        CODE_TARGETS.iter().any(|target| {
            word == target
                || (["kod", "fonksiyon", "sayfa", "ornek", "sorgu"].contains(target)
                    && word.starts_with(target))
        })
    }) || [".html", ".css", ".js", ".sql"]
        .iter()
        .any(|ext| clause.contains(ext))
}

fn response_only(clause: &str) -> bool {
    [
        "code block",
        "kod blog",
        "show me the code",
        "show the code",
        "as a response",
        "in your response",
        "yanitinda",
        "yanitta",
        "kodu goster",
        "ornek kod",
    ]
    .iter()
    .any(|phrase| clause.contains(phrase))
}

fn filesystem_target(words: &[String], clause: &str) -> bool {
    words.iter().any(|word| {
        ["file", "files", "workspace", "project", "disk"].contains(&word.as_str())
            || ["dosya", "proje", "klasor", "dizin"]
                .iter()
                .any(|stem| word.starts_with(stem))
    }) || clause.split_whitespace().any(|word| {
        word.contains(['/', '\\'])
            || [".rs", ".html", ".css", ".js", ".toml", ".json"]
                .iter()
                .any(|ext| word.contains(ext))
    })
}

/// Inceleme/aciklama emrinden sonra gelen "create/update" gibi sozcukler
/// anlatilan kodun parcasi olabilir. Yalnizca once gelen gercek eylem sayilir.
fn read_only_lead(words: &[String], existing_code: bool) -> bool {
    let read = words.iter().position(|word| {
        ["explain", "describe", "clarify"].contains(&word.as_str())
            || ["acikla", "anlat", "yorumla"]
                .iter()
                .any(|stem| turkish_stem_match(word, stem))
            || ((word != "show" && !turkish_stem_match(word, "goster")) || existing_code)
                && ((english_in(word, ANALYZE_EN)
                    && !(word == "check" && words.iter().any(|word| word == "cargo")))
                    || ANALYZE_TR.iter().any(|stem| turkish_stem_match(word, stem)))
    });
    let action = words.iter().position(|word| {
        modification_word(word)
            || ([
                "generate", "produce", "design", "run", "execute", "compile", "build",
            ]
            .contains(&word.as_str()))
            || ["uret", "tasarla", "calistir", "derle"]
                .iter()
                .any(|stem| turkish_stem_match(word, stem))
    });
    read.is_some_and(|read| action.is_none_or(|action| read < action))
}

/// "test" isim olarak puan getirmez. Calistirma/derleme fiili veya acik
/// Ingilizce test emri gerekir. "calistirma" olumsuz; "calistirmani istiyorum"
/// olumlu isim-fiil istegidir ve ayni kok kontrolunde ayrilir.
fn execution_requested(words: &[String], clause: &str) -> bool {
    if words
        .iter()
        .any(|word| ["istemiyorum", "istemem", "istemiyoruz"].contains(&word.as_str()))
    {
        return false;
    }
    // Inceleme/aciklama cumlesindeki eylem bir konu olabilir ("build script",
    // "how we run tests"). Gercek run + aciklama bilesimi ayri eylem cumleleridir.
    if read_only_lead(words, true) || phrase_match(clause, "ne demek") {
        return false;
    }
    if response_only(clause) && code_target(words, clause) {
        return false;
    }
    let test_subject = words.iter().any(|word| {
        word.starts_with("test") || ["cargo", "clippy", "benchmark"].contains(&word.as_str())
    });
    let run = words
        .iter()
        .any(|word| turkish_stem_match(word, "calistir") || word == "run" || word == "execute");
    let compile = words.iter().any(|word| {
        turkish_stem_match(word, "derle") || ["compile", "build"].contains(&word.as_str())
    });
    let imperative_test = words
        .iter()
        .position(|word| word == "test")
        .is_some_and(|index| {
            words[..index].iter().all(|word| {
                ["please", "can", "could", "you", "would", "kindly"].contains(&word.as_str())
            }) && words.get(index + 1).is_some_and(|word| {
                [
                    "the",
                    "this",
                    "my",
                    "our",
                    "project",
                    "it",
                    "all",
                    "application",
                ]
                .contains(&word.as_str())
            }) && !clause.contains("test code")
                && !clause.contains("test results")
        });
    (run && test_subject) || compile || imperative_test
}

/// Acik dosya komutlari ve `- src/file.rs: ...` degisiklik listelerinin
/// hedefleri. Referans/okuma yollarini kapsama katmaz; serbest dildeki tum
/// isteklerin dosya kapsamlarini cikardigini iddia etmez.
fn file_path(word: &str) -> Option<String> {
    let path = word
        .trim_start_matches(['`', '\'', '"', '(', '['])
        .trim_end_matches(['`', '\'', '"', ')', ']', ':', '.']);
    let mut path = path.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(':')
        || path.split('/').any(|part| part.is_empty() || part == "..")
        || path.contains(['=', '{', '}', '<', '>', '|'])
    {
        return None;
    }
    path = path
        .split('/')
        .filter(|part| *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    let extension = path.rsplit('.').next().unwrap_or("");
    (path.contains('/') || path.contains('.') && extension.chars().any(char::is_alphabetic))
        .then_some(path)
}

fn raw_workspace_write_targets(request: &str) -> Vec<String> {
    fn collect(part: &[&str], inherited: &mut bool, targets: &mut Vec<String>) {
        let text = fold(&part.join(" "));
        let words = tokens(&text);
        let action = words
            .iter()
            .position(|word| modification_word(word) || english_in(word, &["register"]));
        if read_only_lead(&words, true)
            || action.is_some_and(|index| english_negation_at(&words, index))
        {
            *inherited = false;
            return;
        }
        let listed = part.first().is_some_and(|word| ["-", "*"].contains(word))
            && part
                .get(1)
                .is_some_and(|word| word.ends_with(':') && file_path(word).is_some());
        let mut writing = action.is_some() || *inherited || listed;
        for word in part {
            let normalized = fold(word);
            if [
                "using",
                "use",
                "from",
                "based",
                "reference",
                "referencing",
                "with",
                "read",
                "review",
                "explain",
                "analyze",
                "inspect",
                "preserve",
                "keep",
                "leave",
                "not",
                "never",
                "without",
                "no",
                "cannot",
            ]
            .contains(&normalized.as_str())
            {
                writing = false;
            }
            if writing
                && let Some(path) = file_path(word)
                && !targets.contains(&path)
            {
                targets.push(path);
            }
        }
        *inherited = writing;
    }

    let text = request
        .replace(". ", " ; ")
        .replace(['\n', '\r', ';', '!', '?'], " ; ")
        .replace(',', " , ");
    let mut part = Vec::new();
    let mut inherited = false;
    let mut targets = Vec::new();
    for word in text.split_whitespace() {
        let boundary = fold(word);
        if [
            ",", ";", "and", "then", "but", "ve", "sonra", "ama", "ancak", "without",
        ]
        .contains(&boundary.as_str())
        {
            collect(&part, &mut inherited, &mut targets);
            part.clear();
            if [";", "then", "but", "sonra", "ama", "ancak", "without"].contains(&boundary.as_str())
            {
                inherited = false;
            }
            if boundary == "without" {
                part.push(word);
            }
        } else {
            part.push(word);
        }
    }
    collect(&part, &mut inherited, &mut targets);
    targets
}

/// Acik dosya kapsamidir; belirsiz/global koruma otomatik yazmaya yetki vermez.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileScope {
    pub write_targets: Vec<String>,
    pub protected_targets: Vec<String>,
    pub ambiguous: bool,
}

fn safe_scope_path(word: &str) -> Option<String> {
    let path = file_path(word)?;
    if path.contains(['\'', '"', '`']) || crate::discovery::check_relative_path(&path).is_err() {
        return None;
    }
    Some(path)
}

fn scope_path_list(words: &[&str]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for word in words {
        if ["and", "or", "ve", ","].contains(&fold(word).as_str()) {
            continue;
        }
        let path = safe_scope_path(word)?;
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    (!paths.is_empty()).then_some(paths)
}

/// Kucuk ve acik English dosya koruma dilbilgisi. Diger olumsuzluklar legacy
/// guard'a kalir; serbest bir isim/it belirli bir dosyaya tahminle baglanmaz.
fn local_protection(text: &str) -> Option<Result<(Vec<String>, bool), ()>> {
    let mut raw: Vec<_> = text.split_whitespace().collect();
    if raw
        .first()
        .is_some_and(|word| ["please", "kindly"].contains(&fold(word).as_str()))
    {
        raw.remove(0);
    }
    let words: Vec<_> = raw
        .iter()
        .map(|word| fold(word.trim_end_matches('.')))
        .collect();
    let patterns: &[&[&str]] = &[
        &["do", "not", "modify"],
        &["do", "not", "change"],
        &["do", "not", "edit"],
        &["do", "not", "write"],
        &["do", "not", "save"],
        &["do", "not", "delete"],
        &["do", "not", "touch"],
        &["never", "modify"],
        &["never", "change"],
        &["never", "edit"],
        &["never", "touch"],
        &["without", "modifying"],
        &["without", "changing"],
        &["without", "touching"],
        &["avoid", "changing"],
        &["avoid", "modifying"],
        &["preserve"],
        &["keep"],
        &["leave"],
    ];
    let prefix = patterns.iter().find(|prefix| {
        words.len() >= prefix.len()
            && words
                .iter()
                .zip(prefix.iter())
                .all(|(word, expected)| word == expected)
    });
    let Some(prefix) = prefix else {
        let negative = words
            .first()
            .is_some_and(|word| ["never", "without", "avoid"].contains(&word.as_str()))
            || words
                .windows(2)
                .any(|words| words[0] == "do" && words[1] == "not");
        let file_subject = raw.iter().any(|word| file_path(word).is_some())
            || words.iter().any(|word| {
                ["it", "them", "file", "files", "project", "workspace"].contains(&word.as_str())
            });
        let unsupported_preserve = words
            .iter()
            .position(|word| ["preserve", "keep", "leave", "protect"].contains(&word.as_str()))
            .is_some_and(|index| {
                raw[index + 1..].iter().any(|word| {
                    file_path(word).is_some()
                        || [
                            "it",
                            "them",
                            "file",
                            "files",
                            "anything",
                            "everything",
                            "project",
                            "workspace",
                        ]
                        .contains(&fold(word.trim_end_matches('.')).as_str())
                })
            });
        return (negative && file_subject || unsupported_preserve).then_some(Err(()));
    };
    let preserving = ["preserve", "keep", "leave"].contains(&words[0].as_str());
    let mut tail = &raw[prefix.len()..];
    if preserving {
        let file_subject = tail.iter().any(|word| {
            file_path(word).is_some()
                || [
                    "it",
                    "them",
                    "file",
                    "files",
                    "anything",
                    "everything",
                    "project",
                    "workspace",
                    "this",
                    "that",
                ]
                .contains(&fold(word.trim_end_matches('.')).as_str())
        });
        if !file_subject {
            return None;
        }
        let unchanged = tail
            .last()
            .is_some_and(|word| fold(word.trim_end_matches('.')) == "unchanged");
        if unchanged {
            tail = &tail[..tail.len() - 1];
        }
        if words[0] != "preserve" && !unchanged {
            return Some(Err(()));
        }
    }
    Some(
        scope_path_list(tail)
            .map(|paths| (paths, preserving))
            .ok_or(()),
    )
}

fn scopes_overlap(first: &str, second: &str) -> bool {
    let lexical = |path: &str| {
        path.replace('\\', "/")
            .split('/')
            .filter(|part| *part != ".")
            .collect::<Vec<_>>()
            .join("/")
    };
    let (first, second) = (lexical(first), lexical(second));
    let (first, second) = if cfg!(windows) {
        (first.to_lowercase(), second.to_lowercase())
    } else {
        (first, second)
    };
    first == second
        || (!first.contains('/') || !second.contains('/'))
            && first.rsplit('/').next() == second.rsplit('/').next()
}

fn scoped_files(request: &str) -> (FileScope, Option<String>) {
    let normalized = request.replace(['’', '‘'], "'").replace("n't", " not");
    let mut scope = FileScope {
        write_targets: raw_workspace_write_targets(&normalized),
        ..FileScope::default()
    };
    let mut remaining = Vec::new();
    let mut previous_preserve: Option<Vec<String>> = None;
    for group in command_groups(&normalized) {
        let folded = fold(&group);
        if outside_workspace_only(&folded) {
            previous_preserve = None;
            continue;
        }
        let mut parts = Vec::new();
        let mut part = Vec::new();
        let separated = group.replace(',', " , ");
        for word in separated.split_whitespace() {
            if [",", "and", "ve"].contains(&fold(word).as_str()) {
                if !part.is_empty() {
                    parts.push(part.join(" "));
                    part.clear();
                }
            } else {
                part.push(word);
            }
        }
        if !part.is_empty() {
            parts.push(part.join(" "));
        }
        let mut local_paths: Option<Vec<String>> = None;
        let mut negative_scope = false;
        let mut leftover = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let folded = fold(part).trim_end_matches('.').to_owned();
            let bound_pronoun = index == 0
                && [
                    "without modifying it",
                    "without changing it",
                    "without touching it",
                ]
                .contains(&folded.as_str())
                && previous_preserve
                    .as_ref()
                    .is_some_and(|paths| paths.len() == 1);
            let protection = if bound_pronoun {
                Some(Ok((previous_preserve.clone().unwrap(), false)))
            } else {
                local_protection(part)
            };
            match protection {
                Some(Ok((paths, preserving))) => {
                    for path in &paths {
                        if !scope.protected_targets.contains(path) {
                            scope.protected_targets.push(path.clone());
                        }
                    }
                    previous_preserve = preserving.then(|| paths.clone());
                    local_paths = Some(paths);
                    negative_scope = !preserving;
                }
                Some(Err(())) => {
                    scope.ambiguous = true;
                    previous_preserve = None;
                    local_paths = None;
                    leftover.push(part.clone());
                }
                None => {
                    let continuation = local_paths.as_ref().and_then(|_| {
                        scope_path_list(&part.split_whitespace().collect::<Vec<_>>())
                    });
                    if let Some(paths) = continuation {
                        for path in paths {
                            if let Some(previous) = &mut previous_preserve
                                && !previous.contains(&path)
                            {
                                previous.push(path.clone());
                            }
                            if !scope.protected_targets.contains(&path) {
                                scope.protected_targets.push(path);
                            }
                        }
                    } else {
                        // "Do not modify B and modify A" olumsuzlugunu A'ya
                        // yazma iznine cevirmeyiz. Yeni but/cumle grubu ayirir.
                        scope.ambiguous |= negative_scope
                            && tokens(&folded).iter().any(|word| modification_word(word));
                        previous_preserve = None;
                        local_paths = None;
                        leftover.push(part.clone());
                    }
                }
            }
        }
        if !leftover.is_empty() {
            remaining.push(leftover.join(" and "));
        }
    }
    // Tek basina bir koruma istegini Unknown uzerinden yeni yazma onayina
    // donusturme. Yerel muafiyet yalniz ayri, acik yazma hedefi varken anlamli.
    scope.ambiguous |= scope.write_targets.is_empty() && !scope.protected_targets.is_empty();
    scope.ambiguous |= scope.write_targets.iter().any(|written| {
        scope
            .protected_targets
            .iter()
            .any(|protected| scopes_overlap(written, protected))
    });
    let global_guard = remaining
        .iter()
        .find_map(|group| guard(&fold(group), &tokens(&fold(group))));
    scope.ambiguous |= global_guard.is_some();
    let write_guard = global_guard.or_else(|| {
        scope
            .ambiguous
            .then(|| "belirsiz veya celisen dosya korumasi".into())
    });
    (scope, write_guard)
}

pub(crate) fn file_scope(request: &str) -> FileScope {
    scoped_files(request).0
}

pub(crate) fn workspace_write_targets(request: &str) -> Vec<String> {
    file_scope(request).write_targets
}

// ============================================================
// SINIFLANDIRMA
// ============================================================
/// Model arac cagirmadiginda yalniz kullanicinin acik ve desteklenen komutunu
/// yeniden onaya sunmak icin kullanilir; kendisi komut calistirmaz.
pub(crate) fn test_execution_plan(request: &str) -> Vec<crate::repair_controller::VerifyMode> {
    use crate::repair_controller::VerifyMode;

    fn generic_execution_forbidden(words: &[String]) -> bool {
        if !words.iter().any(|word| {
            english_in(word, &["tool", "command"])
                || word.starts_with("arac")
                || word.starts_with("komut")
        }) {
            return false;
        }
        let operation = words
            .iter()
            .position(|word| english_in(word, &["use", "call", "run", "execute"]));
        let refused = words
            .iter()
            .any(|word| ["istemiyorum", "istemem", "istemiyoruz"].contains(&word.as_str()));
        operation.is_some_and(|action| english_negation_at(words, action))
            || words
                .first()
                .is_some_and(|word| ["no", "without"].contains(&word.as_str()))
            || words.iter().any(|word| {
                ["cagir", "kullan", "calistir"].iter().any(|stem| {
                    word.strip_prefix(stem).is_some_and(turkish_negative_suffix)
                        || (refused && turkish_stem_match(word, stem))
                })
            })
    }

    fn mode_for(words: &[String]) -> Option<VerifyMode> {
        if let Some(index) = words.iter().position(|word| word == "cargo") {
            match words.get(index + 1).map(String::as_str) {
                Some("test") => Some(VerifyMode::Test),
                Some("check") => Some(VerifyMode::Check),
                _ => None,
            }
        } else if words.iter().any(|word| {
            turkish_stem_match(word, "derle") || ["compile", "build"].contains(&word.as_str())
        }) {
            Some(VerifyMode::Check)
        } else if words.iter().any(|word| word.starts_with("test")) {
            Some(VerifyMode::Test)
        } else {
            None
        }
    }

    if classify(request).intent != TaskIntent::Test {
        return Vec::new();
    }
    let folded = fold(request)
        .replace(['’', '‘'], "'")
        .replace("n't", " not");
    // Bu yol yalniz iki sabit argumani temsil edebilir. Shell sozdizimi,
    // secenekler ve alintilar kirpilip farkli bir komuta donusturulmez.
    if folded.contains(['`', '"', '\'', '|', '&', '>', '<', ';', '$'])
        || folded.split_whitespace().any(|word| word.starts_with('-'))
    {
        return Vec::new();
    }

    let execution_clauses = clauses(&folded);
    if execution_clauses.iter().any(|clause| {
        !phrase_match(clause, "iddia etme")
            && !phrase_match(clause, "do not claim")
            && generic_execution_forbidden(&tokens(clause))
    }) {
        return Vec::new();
    }

    let mut plan = Vec::new();
    let mut denied = Vec::new();
    for clause in execution_clauses {
        let words = tokens(&clause);
        if phrase_match(&clause, "iddia etme") || phrase_match(&clause, "do not claim") {
            continue;
        }
        let execution_stem = |word: &str| {
            ["calistir", "derle"]
                .into_iter()
                .find(|stem| word.strip_prefix(stem).is_some_and(turkish_negative_suffix))
        };
        let refused = words
            .iter()
            .any(|word| ["istemiyorum", "istemem", "istemiyoruz"].contains(&word.as_str()));
        let negative = english_negation(&words)
            || refused
            || words.iter().any(|word| execution_stem(word).is_some());
        if negative {
            // Yalniz olumsuz calistirma eyleminin modunu yasakla. Ornegin
            // "do not modify files" test calistirmayi yasaklamaz. Bu olumlu
            // bicim yalniz negasyon analizidir, hicbir zaman yurutulmez.
            let affirmed: Vec<String> = words
                .iter()
                .filter_map(|word| {
                    if [
                        "not",
                        "never",
                        "without",
                        "no",
                        "cannot",
                        "istemiyorum",
                        "istemem",
                        "istemiyoruz",
                    ]
                    .contains(&word.as_str())
                    {
                        None
                    } else {
                        Some(execution_stem(word).unwrap_or(word).to_string())
                    }
                })
                .collect();
            if execution_requested(&affirmed, &clause)
                && let Some(mode) = mode_for(&affirmed)
                && !denied.contains(&mode)
            {
                denied.push(mode);
            }
            continue;
        }
        if clause
            .split_whitespace()
            .any(|word| is_identifier(word.trim_end_matches(['.', ':'])))
        {
            continue;
        }
        // "Run cargo check and cargo test"te ikinci acik komut, ilk
        // calistirma emrinin devamidir. Tek basina konu/sonuc ifadesi degildir.
        let bare_cargo = words.first().is_some_and(|word| word == "cargo") && !plan.is_empty();
        if !bare_cargo && !execution_requested(&words, &clause) {
            continue;
        }
        if let Some(index) = words.iter().position(|word| word == "cargo") {
            // Ek hedef/filter argumani ya da ayrilmamis ikinci komut, bu
            // iki-modlu aracla ayni komut degildir. Yalniz dogal amac/istek
            // devamini kabul et; argumanlari sessizce atlama.
            if words[index + 1..].iter().any(|word| word == "cargo")
                || words
                    .get(index + 2)
                    .is_some_and(|word| word != "to" && !turkish_stem_match(word, "calistir"))
            {
                continue;
            }
        }
        if let Some(mode) = mode_for(&words)
            && !plan.contains(&mode)
        {
            plan.push(mode);
        }
    }
    plan.retain(|mode| !denied.contains(mode));
    plan
}

pub fn classify(request: &str) -> IntentDecision {
    let folded = fold(request)
        .replace(['’', '‘'], "'")
        .replace("n't", " not");
    let mut evidence = Vec::new();
    let mut scores = Scores::default();
    let mut explicit_file_write = false;
    let mut located_file_write = false;
    let mut explicit_response = false;
    let mut question_change = false;
    let mut direct_change = false;
    for clause in clauses(&folded) {
        let words = tokens(&clause);
        // Kanitsiz sonuc iddiasini yasaklamak bir calistirma istegi degildir.
        // Negasyon komsu cumle/eyleme tasinmaz.
        if english_negation(&words)
            || phrase_match(&clause, "iddia etme")
            || phrase_match(&clause, "do not claim")
            || words
                .iter()
                .any(|word| ["istemiyorum", "istemem", "istemiyoruz"].contains(&word.as_str()))
        {
            continue;
        }
        explicit_response |= response_only(&clause);
        let existing_code = filesystem_target(&words, &clause)
            || words.iter().any(|word| {
                ["existing", "mevcut", "this", "that", "bu", "su"].contains(&word.as_str())
            });
        let read_only = read_only_lead(&words, existing_code);
        let mut modify = if read_only {
            0
        } else {
            score_group(&words, MODIFY_TR, MODIFY_EN, W_MODIFY, &mut evidence)
        };
        let write = !read_only
            && words
                .iter()
                .any(|word| WRITE_TR_EXACT.contains(&word.as_str()));
        if write {
            modify += W_MODIFY;
            evidence.push("yaz".into());
        }
        if modify > 0 {
            let question = words.iter().position(|word| {
                ["how", "why", "nasil", "neden", "niye", "nicin"].contains(&word.as_str())
            });
            let change = words.iter().position(|word| modification_word(word));
            if question.is_some_and(|question| change.is_some_and(|change| question < change)) {
                question_change = true;
            } else {
                direct_change = true;
            }
        }
        let generation_action = !read_only
            && (write
                || (words.iter().any(|word| word == "build")
                    && (response_only(&clause)
                        || words.iter().any(|word| {
                            ["html", "css", "layout", "page", "component"].contains(&word.as_str())
                        })))
                || words.iter().any(|word| {
                    if existing_code && (word == "show" || turkish_stem_match(word, "goster")) {
                        return false;
                    }
                    GENERATE_TR
                        .iter()
                        .any(|stem| turkish_stem_match(word, stem))
                        || english_in(word, GENERATE_EN)
                        || (word == "ver" && code_target(&words, &clause))
                }));
        if generation_action && code_target(&words, &clause) {
            scores.generate += W_GENERATE;
            evidence.push("yanitta kod/artifact uretimi".into());
        }
        if modify > 0 && filesystem_target(&words, &clause) && !response_only(&clause) {
            let located = words.iter().any(|word| {
                ["workspace", "project", "disk"].contains(&word.as_str())
                    || ["proje", "klasor", "dizin"]
                        .iter()
                        .any(|stem| word.starts_with(stem))
                    || modification_word(word)
                        && !english_in(word, GENERATE_EN)
                        && !GENERATE_TR
                            .iter()
                            .any(|stem| turkish_stem_match(word, stem))
                        && !WRITE_TR_EXACT.contains(&word.as_str())
            }) || clause.contains(['/', '\\'])
                && [" into ", " to ", " in "]
                    .iter()
                    .any(|relation| clause.contains(relation));
            located_file_write |= located;
            let code_reference = generation_action
                && !located
                && (clause.contains(" for ") || clause.contains(" icin "));
            explicit_file_write |= !code_reference;
        }
        scores.modify += modify;
        scores.analyze += score_group(&words, ANALYZE_TR, ANALYZE_EN, W_ANALYZE, &mut evidence);
        scores.explain += score_group(&words, EXPLAIN_TR, EXPLAIN_EN, W_EXPLAIN, &mut evidence);
        if execution_requested(&words, &clause) {
            scores.test += W_TEST;
            evidence.push("acik derleme/test calistirma istegi".into());
        }
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
    if scores.generate > 0 && explicit_response && !located_file_write {
        explicit_file_write = false;
    }

    // Virgulle veya and/or ile uzayan hedefleri ayirmadan once kapsam secilir;
    // aksi halde "outside this project and src/lib.rs" ic yasagi gizleyebilir.
    let (_, write_guard) = scoped_files(request);
    if let Some(found) = &write_guard {
        // Kullanici acikca degisiklik istemiyor: yazma sinyalleri gecersiz.
        scores.modify = 0;
        explicit_file_write = false;
        evidence.push(format!("salt-okunur koruma: '{found}'"));
    }

    let read_only_top = scores.test.max(scores.analyze).max(scores.explain);
    let (intent, top, runner_up) = if scores.modify > 0 && question_change && !direct_change
        || explicit_file_write && scores.explain >= scores.modify
    {
        evidence.push("celisen sinyal: dosya degisikligi hakkinda soru".into());
        (TaskIntent::Unknown, scores.modify, scores.explain)
    } else if explicit_file_write {
        (TaskIntent::Modify, scores.modify, scores.explain)
    } else if scores.generate > 0 {
        // Uretilen kod icindeki "ekle" gibi fiiller dosya yetkisi vermez.
        (TaskIntent::Generate, scores.generate, scores.explain)
    } else if scores.modify > 0 {
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
    } else if top >= 3 && top.saturating_sub(runner_up) >= 2 {
        Confidence::High
    } else {
        Confidence::Medium
    };

    IntentDecision {
        intent,
        confidence,
        evidence,
        scores,
        write_forbidden: write_guard.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn belirli_dosya_korumasi_yalniz_diger_acik_hedefe_yazma_izni_birakmali() {
        for (request, written, protected) in [
            (
                "Modify src/math.rs, preserve src/lib.rs without modifying it.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs and preserve src/lib.rs.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs, but please preserve src/lib.rs.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs, but please keep src/lib.rs unchanged.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs, but leave src/lib.rs unchanged.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify ./src/math.rs and keep src\\lib.rs unchanged.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs, but do not modify src/lib.rs.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs, but never touch src/lib.rs or src/other.rs.",
                vec!["src/math.rs"],
                vec!["src/lib.rs", "src/other.rs"],
            ),
            (
                "Modify src/math.rs without modifying src/lib.rs.",
                vec!["src/math.rs"],
                vec!["src/lib.rs"],
            ),
            (
                "Modify src/math.rs and src/lib.rs.",
                vec!["src/math.rs", "src/lib.rs"],
                vec![],
            ),
        ] {
            let scope = file_scope(request);
            assert_eq!(scope.write_targets, written, "{request}: {scope:?}");
            assert_eq!(scope.protected_targets, protected, "{request}: {scope:?}");
            assert!(!scope.ambiguous, "{request}: {scope:?}");
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Modify,
                "{request}: {decision:?}"
            );
            assert!(!decision.write_forbidden, "{request}: {decision:?}");
        }
    }

    #[test]
    fn genel_belirsiz_guvensiz_ve_ayni_dosya_yasagi_muafiyet_almamali() {
        for request in [
            "Avoid changing src/main.rs.",
            "Never modify src/main.rs.",
            "Preserve src/main.rs.",
            "Never touch src/main.rs.",
            "Modify src/math.rs, but do not modify any files.",
            "Modify src/math.rs, do not touch any files.",
            "Modify src/math.rs, never touch files.",
            "Modify src/math.rs, but you must not touch src/lib.rs.",
            "Modify src/math.rs, preserve src/math.rs without modifying it.",
            "Modify src/math.rs, preserve src/./math.rs without modifying it.",
            "Modify src/./math.rs, preserve src/math.rs without modifying it.",
            "Modify src/math.rs and preserve math.rs.",
            "Modify src/math.rs, but do not modify it.",
            "Modify src/math.rs and src/lib.rs, but preserve it without modifying it.",
            "Modify src/math.rs, preserve src/lib.rs and src/other.rs without modifying it.",
            "Modify src/math.rs, but do not modify ../outside.rs.",
            "Modify src/math.rs, but do not modify .env.",
            "Modify src/math.rs, preserve behavior without modifying it.",
            "Modify src/math.rs, but do not modify src/lib.rs or anything else.",
            "Modify src/math.rs, but do not alter src/lib.rs.",
            "Modify src/math.rs, but you must preserve src/lib.rs.",
            "Never touch src/lib.rs and modify src/math.rs.",
            "Do not modify src/lib.rs and modify src/math.rs.",
        ] {
            let scope = file_scope(request);
            assert!(scope.ambiguous, "{request}: {scope:?}");
            let decision = classify(request);
            assert!(decision.write_forbidden, "{request}: {decision:?}");
            assert_ne!(
                decision.intent,
                TaskIntent::Modify,
                "{request}: {decision:?}"
            );
        }
    }

    #[test]
    fn salt_inceleme_ve_aciklama_yerel_korumayla_yazma_yetkisi_almamali() {
        for (request, expected) in [
            (
                "Analyze src/lib.rs, do not modify any files.",
                TaskIntent::Analyze,
            ),
            (
                "Explain src/lib.rs without modifying it.",
                TaskIntent::Explain,
            ),
        ] {
            assert!(file_scope(request).write_targets.is_empty());
            let decision = classify(request);
            assert_eq!(decision.intent, expected, "{request}: {decision:?}");
            assert!(decision.write_forbidden, "{request}: {decision:?}");
        }
    }

    #[test]
    fn fonksiyon_davranisini_korumak_tum_dosyayi_yazmaya_kapatmamali() {
        for request in [
            "Modify src/math.rs by preserving double and adding triple.",
            "Modify src/math.rs and preserve existing functions.",
            "Modify src/math.rs, preserve double and add triple.",
        ] {
            let scope = file_scope(request);
            assert_eq!(scope.write_targets, vec!["src/math.rs"]);
            assert!(scope.protected_targets.is_empty(), "{request}: {scope:?}");
            assert!(!scope.ambiguous, "{request}: {scope:?}");
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Modify,
                "{request}: {decision:?}"
            );
            assert!(!decision.write_forbidden, "{request}: {decision:?}");
        }
    }

    #[test]
    fn proje_disi_yazma_yasagi_proje_ici_modify_istegini_iptal_etmemeli() {
        for request in [
            "Modify src/lib.rs, but do not modify files outside this project.",
            "Update src/lib.rs. Never edit files outside the workspace.",
            "Modify src/lib.rs, but do not write anything outside the current repository.",
            "Modify src/lib.rs without modifying files outside this project.",
            "Do not modify files outside this project, but update src/lib.rs.",
        ] {
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Modify,
                "{request}: {decision:?}"
            );
            assert!(!decision.write_forbidden, "{request}: {decision:?}");
        }
    }

    #[test]
    fn proje_siniri_kurali_gercek_yazma_yasaklarini_gizlememeli() {
        for request in [
            "Modify src/lib.rs, but do not modify files.",
            "Modify src/lib.rs, but do not modify src/lib.rs.",
            "Modify src/lib.rs, but do not modify anything in this project.",
            "Modify src/lib.rs, but do not modify files inside or outside this project.",
            "Modify src/lib.rs, but do not modify src/lib.rs or files outside this project.",
            "Modify src/lib.rs, but do not modify files outside this project and src/lib.rs.",
            "Modify src/lib.rs, but do not modify files outside this project, or src/lib.rs.",
            "Modify src/lib.rs. Do not modify files outside this project. Do not modify any files.",
            "Modify src/lib.rs, but do not modify files outside this project or inside it.",
            "Modify src/lib.rs, but do not modify files outside another project.",
            "src/lib.rs dosyasını güncelle ama dosyaları değiştirme.",
        ] {
            let decision = classify(request);
            assert!(decision.write_forbidden, "{request}: {decision:?}");
            assert_ne!(
                decision.intent,
                TaskIntent::Modify,
                "{request}: {decision:?}"
            );
        }
    }

    #[test]
    fn workspace_acik_yazma_hedefleri_tum_dosyalari_kapsamali() {
        for request in [
            "Modify src/math.rs by preserving double and adding triple, create src/format.rs, and modify src/lib.rs to register the new module.",
            "Modify these files:\n- src/math.rs: preserve double and add triple\n- src/format.rs: create a formatting module\n- src/lib.rs: register the new module",
            "Modify these files:\n- `src/math.rs`: preserve double and add triple\n- `src/format.rs`: create a formatting module\n- `src/lib.rs`: register the new module",
        ] {
            assert_eq!(
                workspace_write_targets(request),
                ["src/math.rs", "src/format.rs", "src/lib.rs"],
                "{request}"
            );
        }
        assert_eq!(
            workspace_write_targets("Modify src/Math.rs and src/Lib.rs."),
            ["src/Math.rs", "src/Lib.rs"]
        );
        assert_eq!(
            workspace_write_targets("Modify ./src/Math.rs and src\\Lib.rs."),
            ["src/Math.rs", "src/Lib.rs"]
        );
        for request in ["Modify `src/lib.rs`.", "Modify \"src/lib.rs\"."] {
            assert_eq!(
                workspace_write_targets(request),
                ["src/lib.rs"],
                "{request}"
            );
        }
    }

    #[test]
    fn workspace_okuma_ve_referans_yollari_yazma_hedefi_sayilmamali() {
        for request in [
            "Read src/math.rs then modify src/lib.rs.",
            "Modify src/lib.rs using src/math.rs.",
            "Modify src/lib.rs to use src/math.rs.",
            "Modify src/lib.rs based on src/math.rs and src/format.rs.",
            "Modify src/lib.rs; explain how to modify src/math.rs.",
        ] {
            assert_eq!(
                workspace_write_targets(request),
                ["src/lib.rs"],
                "{request}"
            );
        }
        assert!(workspace_write_targets("Read src/math.rs and explain src/lib.rs.").is_empty());
        assert!(workspace_write_targets("Do not modify src/math.rs.").is_empty());
    }

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
        for intent in [
            TaskIntent::Analyze,
            TaskIntent::Explain,
            TaskIntent::Generate,
            TaskIntent::Test,
        ] {
            assert_eq!(intent.write_authority(), WriteAuthority::Denied);
        }
    }

    #[test]
    fn turkce_karakterler_katlanmali() {
        assert_eq!(fold("Düzelt  ŞİMDİ, Çabuk"), "duzelt simdi, cabuk");
        assert_eq!(fold("Işık"), "isik");
    }

    #[test]
    fn zorunlu_turkce_ingilizce_regresyon_tablosu() {
        for (request, expected) in [
            (
                "Create an HTML landing page and show me the code.",
                TaskIntent::Generate,
            ),
            (
                "Write a Rust function but do not modify my project.",
                TaskIntent::Generate,
            ),
            (
                "Generate a responsive CSS layout in one code block.",
                TaskIntent::Generate,
            ),
            (
                "Create an index.html file inside the workspace.",
                TaskIntent::Modify,
            ),
            ("Update src/main.rs and run cargo test.", TaskIntent::Modify),
            ("Run cargo test and report the results.", TaskIntent::Test),
            (
                "Review this JavaScript and explain the bugs.",
                TaskIntent::Analyze,
            ),
            ("Explain how Rust ownership works.", TaskIntent::Explain),
            (
                "Do not run any tests. Only generate HTML.",
                TaskIntent::Generate,
            ),
            (
                "Bana bir HTML sayfası kodu üret, dosyalara dokunma.",
                TaskIntent::Generate,
            ),
            (
                "Sadece Rust fonksiyonu yaz, projeyi değiştirme.",
                TaskIntent::Generate,
            ),
            (
                "src/main.rs dosyasını incele ve hataları raporla.",
                TaskIntent::Analyze,
            ),
            ("cargo test çalıştır ve sonuçları bildir.", TaskIntent::Test),
            (
                "src/main.rs dosyasındaki başlığı değiştir.",
                TaskIntent::Modify,
            ),
            (
                "index.html dosyasını proje klasöründe oluştur.",
                TaskIntent::Modify,
            ),
            ("Kodu yaz ama dosyayı kaydetme.", TaskIntent::Generate),
            (
                "Test çalıştırma, yalnızca örnek kod göster.",
                TaskIntent::Generate,
            ),
        ] {
            assert_eq!(intent_of(request), expected, "{request}");
        }
    }

    #[test]
    fn orijinal_landing_page_istegi_generate_olmali() {
        let decision = classify(include_str!("../tests/fixtures/landing_page_request.txt"));
        assert_eq!(decision.intent, TaskIntent::Generate);
        assert_eq!(decision.scores.test, 0);
        assert_eq!(decision.scores.modify, 0);
        assert!(decision.write_forbidden);
    }

    #[test]
    fn olumsuz_eylem_olumlu_talimat_olmamali() {
        for request in [
            "Do not test.",
            "Do not modify.",
            "Do not create any files.",
            "No tool calls.",
            "Without changing the project.",
            "Test ettiğini iddia etme.",
            "Test edildiğini iddia etme.",
            "Dosyaları değiştirme.",
            "Dosya oluşturma.",
            "Test çalıştırma.",
            "Araç çağırma.",
            "Projeye dokunma.",
            "Herhangi bir dosyaya yazma.",
            "Never modify src/main.rs.",
            "Dosyayı silmemeni istiyorum.",
            "Dosyayı değiştirmemeni istiyorum.",
            "Dosyayı değiştirmeni istemiyorum.",
            "Test çalıştırmamanı istiyorum.",
            "Test çalıştırmanı istemiyorum.",
        ] {
            let decision = classify(request);
            assert_ne!(
                decision.intent,
                TaskIntent::Modify,
                "{request}: {decision:?}"
            );
            assert_ne!(decision.intent, TaskIntent::Test, "{request}: {decision:?}");
            assert_eq!(decision.scores.test, 0, "{request}");
            assert_eq!(decision.scores.modify, 0, "{request}");
        }
        for request in ["Yalnızca kod üret.", "Sadece kodu göster."] {
            assert_eq!(intent_of(request), TaskIntent::Generate);
        }
    }

    #[test]
    fn olumsuzluk_komsu_olumlu_teste_yayilmamali() {
        for request in [
            "Test çalıştırmanı istiyorum.",
            "Testleri çalıştır.",
            "Please test the project.",
            "Do not modify files. Run cargo test.",
            "Test çalıştırma. Derle.",
        ] {
            assert_eq!(intent_of(request), TaskIntent::Test, "{request}");
        }
        for request in [
            "Test çalıştırmak istemiyorum, yalnızca HTML üret.",
            "Do not test. Generate a SQL query.",
            "Generate HTML with no external libraries.",
            "Write an HTML file and show me the code.",
        ] {
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Generate,
                "{request}: {decision:?}"
            );
            assert_eq!(decision.scores.test, 0);
        }
    }

    #[test]
    fn karisik_istekler_yazma_sinirini_zayiflatmamali() {
        assert_eq!(
            intent_of("Explain this code and generate an improved example without changing files."),
            TaskIntent::Generate
        );
        for request in [
            "Update src/main.rs and run cargo test.",
            "Generate an HTML example and save it to src/index.html.",
            "src/main.rs dosyasını güncelle ve testleri çalıştır.",
        ] {
            assert_eq!(intent_of(request), TaskIntent::Modify, "{request}");
        }
        for request in [
            "Create a file, but don't write anything to disk.",
            "Never modify src/main.rs.",
            "Dosyayı değiştirmemeni istiyorum.",
        ] {
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Unknown,
                "{request}: {decision:?}"
            );
            assert!(decision.write_forbidden, "{request}");
        }
        for request in [
            "How do I modify src/main.rs?",
            "How do I modify src/main.rs.",
            "src/main.rs dosyasını nasıl değiştiririm?",
            "How do I create an index.html file?",
            "How do I create an index.html file.",
            "nasıl düzeltirim",
        ] {
            assert_eq!(intent_of(request), TaskIntent::Unknown, "{request}");
        }
    }

    #[test]
    fn mevcut_kodu_gostermek_ve_testlerden_bahsetmek_uretim_veya_calistirma_degildir() {
        for request in [
            "Review the test results.",
            "Show the code in src/main.rs.",
            "Show me the Rust function in src/main.rs.",
            "src/main.rs dosyasındaki kodu göster.",
            "Show me the Rust code already present in src/main.rs.",
        ] {
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Analyze,
                "{request}: {decision:?}"
            );
            assert_eq!(decision.scores.test, 0);
        }
        for request in [
            "Write a Python script.",
            "Generate a configuration example.",
            "Create a JavaScript component in one code block.",
            "Generate a SQL query.",
        ] {
            assert_eq!(intent_of(request), TaskIntent::Generate, "{request}");
        }
    }

    #[test]
    fn eylemi_aciklamak_calistirma_yetkisi_vermemeli() {
        for (request, expected) in [
            ("Explain the build configuration.", TaskIntent::Explain),
            ("Review the build script.", TaskIntent::Analyze),
            ("Review how we run the tests.", TaskIntent::Analyze),
            ("Test çalıştırmak ne demek?", TaskIntent::Explain),
            (
                "Build a responsive CSS layout in one code block.",
                TaskIntent::Generate,
            ),
            ("Sadece örnek kod yazmanı istiyorum.", TaskIntent::Generate),
            (
                "Kod yazmanı istiyorum, dosyaları değiştirmeden.",
                TaskIntent::Generate,
            ),
        ] {
            let decision = classify(request);
            assert_eq!(decision.intent, expected, "{request}: {decision:?}");
            assert_eq!(decision.scores.test, 0, "{request}");
        }
        assert!(!classify("Sadece örnek kod yazmanı istiyorum.").write_forbidden);
        assert_eq!(intent_of("Run cargo check."), TaskIntent::Test);
        assert_eq!(intent_of("cargo check çalıştır."), TaskIntent::Test);
    }

    #[test]
    fn acik_inceleme_talimatindaki_fiiller_yetki_vermemeli() {
        for (request, expected) in [
            (
                "Only show me how to update src/main.rs.",
                TaskIntent::Analyze,
            ),
            (
                "Review the create function in src/main.rs.",
                TaskIntent::Analyze,
            ),
            (
                "Explain this Rust code: create a file.",
                TaskIntent::Explain,
            ),
            ("Explain how to modify src/main.rs.", TaskIntent::Explain),
            (
                "Explain how to create a Rust function.",
                TaskIntent::Explain,
            ),
            (
                "Show this code and explain what it does.",
                TaskIntent::Explain,
            ),
        ] {
            let decision = classify(request);
            assert_eq!(decision.intent, expected, "{request}: {decision:?}");
            assert_eq!(decision.intent.write_authority(), WriteAuthority::Denied);
            assert_eq!(decision.scores.modify, 0);
            assert_eq!(decision.scores.generate, 0);
        }
        assert_eq!(
            intent_of("Review this code and update src/main.rs."),
            TaskIntent::Modify
        );
        assert_eq!(
            intent_of("Write a Rust example showing how ownership works."),
            TaskIntent::Generate
        );
    }

    #[test]
    fn bagli_olumsuz_eylemler_ve_kacinma_talimatlari_yazma_yetkisi_vermemeli() {
        for request in [
            "Do not test and modify src/main.rs.",
            "Do not compile and run tests.",
            "Avoid changing src/main.rs.",
        ] {
            let decision = classify(request);
            assert_eq!(
                decision.intent,
                TaskIntent::Unknown,
                "{request}: {decision:?}"
            );
            assert_eq!(decision.scores.test, 0);
            assert_eq!(decision.scores.modify, 0);
        }
        assert!(classify("Do not test and modify src/main.rs.").write_forbidden);
        assert!(classify("Avoid changing src/main.rs.").write_forbidden);
        assert_eq!(
            intent_of("Do not run tests but update src/main.rs."),
            TaskIntent::Modify
        );
    }

    fn assert_stabilized(request: &str, expected: TaskIntent, authority: WriteAuthority) {
        let decision = classify(request);
        let actual_authority = if decision.write_forbidden {
            WriteAuthority::Denied
        } else {
            decision.intent.write_authority()
        };
        assert_eq!(decision.intent, expected, "{request}: {decision:?}");
        assert_eq!(actual_authority, authority, "{request}: {decision:?}");
    }

    #[test]
    fn stabilizasyon_fonksiyon_adlari_yazma_emri_degil() {
        assert_stabilized(
            "Review the create and update functions in src/main.rs.",
            TaskIntent::Analyze,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "src/main.rs içindeki update fonksiyonunu açıkla.",
            TaskIntent::Explain,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "Review the create, update, and delete functions in src/main.rs.",
            TaskIntent::Analyze,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "src/main.rs dosyasındaki delete ve update fonksiyonlarını incele.",
            TaskIntent::Analyze,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "src/main.rs içindeki degistir fonksiyonunu açıkla.",
            TaskIntent::Explain,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "src/main.rs içindeki fonksiyonu değiştir.",
            TaskIntent::Modify,
            WriteAuthority::Granted,
        );
    }

    #[test]
    fn stabilizasyon_negasyon_eylem_eslesmesi_ortak_olmali() {
        let failures: Vec<_> = [
            "Do not make changes to src/main.rs.",
            "You mustn't modify src/main.rs.",
            "You mustn’t modify src/main.rs.",
            "You shouldn't change src/main.rs.",
            "You shouldn’t change src/main.rs.",
            "You can't modify src/main.rs.",
            "You couldn’t modify src/main.rs.",
            "You won't change src/main.rs.",
        ]
        .into_iter()
        .filter_map(|request| {
            let decision = classify(request);
            (decision.intent == TaskIntent::Modify || !decision.write_forbidden)
                .then_some((request, decision))
        })
        .collect();
        assert!(failures.is_empty(), "{failures:#?}");
        assert_stabilized(
            "Can you modify src/main.rs?",
            TaskIntent::Modify,
            WriteAuthority::Granted,
        );
    }

    #[test]
    fn stabilizasyon_tam_istekte_kod_yaniti_dosya_yazimi_degil() {
        assert_stabilized(
            "Write a Rust function for src/main.rs. Return it in one code block. Do not call any tools.",
            TaskIntent::Generate,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "Write a Rust function for src/main.rs.",
            TaskIntent::Generate,
            WriteAuthority::Denied,
        );
        assert_stabilized(
            "Update src/main.rs and write a Rust example for src/lib.rs in one code block.",
            TaskIntent::Modify,
            WriteAuthority::Granted,
        );
    }

    #[test]
    fn stabilizasyon_reclaim_gecerli_kod_uretimidir() {
        assert_stabilized(
            "Generate a Rust function named reclaim.",
            TaskIntent::Generate,
            WriteAuthority::Denied,
        );
    }

    #[test]
    fn stabilizasyon_guncelleme_isim_tamlamasi_olumlu_olabilir() {
        assert_stabilized(
            "Güncelleme yap: src/main.rs başlığını değiştir.",
            TaskIntent::Modify,
            WriteAuthority::Granted,
        );
    }

    #[test]
    fn stabilizasyon_test_amaci_analiz_sozcuguyle_bastirilmamali() {
        assert_stabilized(
            "Run cargo test to find failures.",
            TaskIntent::Test,
            WriteAuthority::Denied,
        );
    }

    #[test]
    fn test_execution_plan_acik_desteklenen_komutlari_secmeli() {
        use crate::repair_controller::VerifyMode::{Check, Test};
        for (request, expected) in [
            ("Run cargo test to find failures.", vec![Test]),
            ("cargo test çalıştır ve sonuçları bildir.", vec![Test]),
            ("Run cargo check.", vec![Check]),
            ("Derle.", vec![Check]),
            ("Build the project.", vec![Check]),
            ("Testleri çalıştır.", vec![Test]),
            ("Run the tests.", vec![Test]),
            ("Run cargo check and cargo test.", vec![Check, Test]),
            (
                "Run cargo check and run cargo test to find failures.",
                vec![Check, Test],
            ),
            ("Run cargo test and run cargo test.", vec![Test]),
        ] {
            assert_eq!(test_execution_plan(request), expected, "{request}");
        }
    }

    #[test]
    fn test_execution_plan_olumsuzlugu_ve_diger_niyetleri_korumali() {
        use crate::repair_controller::VerifyMode::{Check, Test};
        for request in [
            "Do not run cargo test.",
            "Test çalıştırma.",
            "Test çalıştırmanı istemiyorum.",
            "Run cargo test. Do not run tests.",
            "Run cargo test. Test çalıştırma.",
            "Run cargo test. Test çalıştırmanı istemiyorum.",
            "Explain how to run cargo test.",
            "Generate HTML and do not run tests.",
            "Update src/main.rs and run cargo test.",
            "src/main.rs dosyasını incele.",
            "Run cargo test. Do not call any tools.",
            "Run cargo test, but do not execute commands.",
            "cargo test çalıştır ama araç çağırma.",
            "cargo test çalıştır ama komut çalıştırma.",
        ] {
            assert!(test_execution_plan(request).is_empty(), "{request}");
        }
        assert_eq!(
            test_execution_plan("Do not run cargo test. Run cargo check."),
            vec![Check]
        );
        assert_eq!(test_execution_plan("Test çalıştırma. Derle."), vec![Check]);
        assert_eq!(
            test_execution_plan("Do not modify files. Run cargo test."),
            vec![Test]
        );
        assert_eq!(
            test_execution_plan("Run cargo check. Do not run tests."),
            vec![Check]
        );
        assert_eq!(
            test_execution_plan("Run cargo test. Test edildiğini iddia etme."),
            vec![Test]
        );
        assert_eq!(
            test_execution_plan("Run cargo test. Do not claim that tests passed."),
            vec![Test]
        );
        assert_eq!(
            test_execution_plan("Run cargo test. Do not claim to execute commands."),
            vec![Test]
        );
    }

    #[test]
    fn test_execution_plan_desteklenmeyen_sozdizimini_kesmemeli() {
        for request in [
            "Run cargo clippy.",
            "Run cargo build.",
            "Run cargo test --release.",
            "Run cargo test --test smoke.",
            "Run cargo test smoke.",
            "Run cargo check then explain cargo test.",
            "Run cargo test && cargo check.",
            "Run cargo test | more.",
            "Run cargo test > result.txt.",
            "Run `cargo test`.",
            "Run \"cargo test\".",
            "Run cargo test in ../other.",
        ] {
            assert!(test_execution_plan(request).is_empty(), "{request}");
        }
    }

    #[test]
    fn test_execution_plan_genel_komut_yasagini_korumali() {
        let failures: Vec<_> = [
            "Run cargo test. Never execute any commands.",
            "Run cargo test. You must not use any tools.",
            "Run cargo test. No tool calls.",
            "Run cargo test without tools.",
            "Run cargo test. Araç çağırma.",
            "Run cargo test. Komutları çalıştırma.",
            "Run cargo test. Araç kullanmanı istemiyorum.",
        ]
        .into_iter()
        .filter_map(|request| {
            let plan = test_execution_plan(request);
            (!plan.is_empty()).then_some((request, plan))
        })
        .collect();
        assert!(failures.is_empty(), "{failures:?}");
    }
}
