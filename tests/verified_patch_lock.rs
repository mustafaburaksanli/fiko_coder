//! FIKO CODER v0.8 - Son guvenlik kabul testi.
//!
//! Verified Patch Lock, Qwen modelinin davranisina bagli olmamalidir. Bu test
//! modeli tamamen devre disi birakir: `verify_candidate` hic cagrilmadan,
//! dogrudan `handle_tool_call` uzerinden `apply_patch` istenir ve kullanici
//! gecici derleme iznini reddeder. Beklenti: gercek dosya degismez, onay
//! sorusu gelmez, yedek klasoru olusmaz. Ayni senaryo izin verilerek tekrar
//! edilir ve guvenli akisin (otomatik dogrulama -> tek onay -> gercek
//! projede cargo check + test) calistigi kontrol edilir.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use fiko_coder::agent::{Prompter, Session, ToolOutcome, handle_tool_call};
use fiko_coder::repair_controller::RepairController;

const MINI_MAIN: &str = "fn main() {\n    let value: i32 = 1;\n    println!(\"{value}\");\n}\n";
const MINI_OLD: &str = "let value: i32 = 1;";
const MINI_GOOD: &str = "let value: i32 = 2;";

/// Modelin yerine gecen sahte kullanici: cevaplar onceden belirlenir, sorular kaydedilir.
struct FakePrompter {
    answers: VecDeque<bool>,
    asked: Vec<String>,
}

impl FakePrompter {
    fn with(answers: &[bool]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            asked: Vec::new(),
        }
    }

    fn asked_approval(&self) -> bool {
        self.asked.iter().any(|q| q.contains("Patch uygulansin"))
    }

    fn asked_consent(&self) -> bool {
        self.asked.iter().any(|q| q.contains("cargo check"))
    }
}

impl Prompter for FakePrompter {
    fn ask_yes(&mut self, question: &str) -> io::Result<bool> {
        self.asked.push(question.to_string());
        Ok(self.answers.pop_front().unwrap_or(false))
    }
}

/// Bagimliligi olmayan, derlenebilir gecici bir Rust projesi.
fn mini_crate(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fiko_lock_it_{tag}_{}_{}",
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

/// Dogrulanmamis fakat gecerli (derlenebilir) patch; mode alani yok, dogrudan apply.
fn unverified_valid_patch() -> Value {
    json!({
        "path": "src/main.rs",
        "old_text": MINI_OLD,
        "new_text": MINI_GOOD
    })
}

async fn apply_directly(
    session: &mut Session,
    repair: &mut RepairController,
    prompter: &mut FakePrompter,
) -> ToolOutcome {
    handle_tool_call(
        session,
        repair,
        prompter,
        "apply_patch",
        &unverified_valid_patch(),
    )
    .await
    .expect("handle_tool_call IO hatasi vermemeli")
}

fn main_rs(root: &Path) -> String {
    fs::read_to_string(root.join("src/main.rs")).unwrap()
}

#[tokio::test]
async fn kilit_izin_yokken_dogrulanmamis_patch_gercek_dosyaya_yazilamaz() {
    let root = mini_crate("deny");
    let mut session = Session::new(root.clone());
    let mut repair = RepairController::default();
    // Tek cevap: gecici derleme iznine "hayir". Baska soru gelmemeli.
    let mut prompter = FakePrompter::with(&[false]);

    let outcome = apply_directly(&mut session, &mut repair, &mut prompter).await;

    // 6) Ret sonucu
    assert!(
        outcome.output.starts_with("APPLY_BLOCKED"),
        "beklenen APPLY_BLOCKED, gelen: {}",
        outcome.output
    );
    assert!(outcome.stop.is_some(), "kilit gorevi durdurmali");
    // 7) Gercek dosya degismedi
    assert_eq!(main_rs(&root), MINI_MAIN, "gercek dosya degismis!");
    // 8) Patch uygulama onayi hic istenmedi; yalnizca izin sorusu soruldu
    assert!(
        !prompter.asked_approval(),
        "onay istendi: {:?}",
        prompter.asked
    );
    assert!(
        prompter.asked_consent(),
        "izin sorusu bekleniyordu: {:?}",
        prompter.asked
    );
    assert_eq!(prompter.asked.len(), 1, "{:?}", prompter.asked);
    // 9) Yedek klasoru olusmadi
    assert!(
        !root.join(".fiko_backups").exists(),
        "yedek klasoru olusturulmamali"
    );
    // Hic cargo calismadi, oturumda uygulanmis patch yok
    assert_eq!(repair.verify_attempts(), 0);
    assert!(session.last_patch.is_none());

    // Ayni oturumda farkli bir aday da kilidi asamaz ve izin tekrar sorulmaz.
    let second = handle_tool_call(
        &mut session,
        &mut repair,
        &mut prompter,
        "apply_patch",
        &json!({"path": "src/main.rs", "old_text": MINI_OLD, "new_text": "let value: i32 = 3;"}),
    )
    .await
    .unwrap();
    assert!(
        second.output.starts_with("APPLY_BLOCKED"),
        "{}",
        second.output
    );
    assert_eq!(prompter.asked.len(), 1);
    assert_eq!(main_rs(&root), MINI_MAIN);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn kilit_izin_verilince_guvenli_akis_calisir() {
    let root = mini_crate("allow");
    let mut session = Session::new(root.clone());
    let mut repair = RepairController::default();
    // Iki cevap: gecici derleme iznine "evet", patch onayina "evet".
    let mut prompter = FakePrompter::with(&[true, true]);

    let outcome = apply_directly(&mut session, &mut repair, &mut prompter).await;

    assert!(
        outcome.output.starts_with("PATCH_VERIFIED"),
        "beklenen PATCH_VERIFIED, gelen: {}",
        outcome.output
    );
    assert!(outcome.stop.is_some(), "basarili patch sonrasi gorev biter");
    // Dogrulanmamis aday once gecici kopyada derlendi (otomatik dogrulama), sonra tek onay.
    assert_eq!(
        repair.verify_attempts(),
        1,
        "otomatik dogrulama bir kez calismali"
    );
    assert_eq!(prompter.asked.len(), 2, "{:?}", prompter.asked);
    assert!(
        prompter.asked[0].contains("cargo check"),
        "{:?}",
        prompter.asked
    );
    assert!(
        prompter.asked[1].contains("Patch uygulansin"),
        "{:?}",
        prompter.asked
    );
    // Gercek dosya degisti, yedek alindi, geri alma kaydi var.
    let content = main_rs(&root);
    assert!(content.contains(MINI_GOOD), "{content}");
    let record = session.last_patch.as_ref().expect("geri alma kaydi");
    let backup_dir = root.join(".fiko_backups");
    let backups: Vec<_> = fs::read_dir(&backup_dir)
        .expect("yedek klasoru olusmali")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path != &backup_dir.join("workspace.operation.lock"))
        .collect();
    assert_eq!(backups, vec![record.backup_path.clone()], "tek gercek yedek beklenir");
    assert_eq!(fs::read_to_string(&record.backup_path).unwrap(), MINI_MAIN);

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn kilit_izin_var_onay_yoksa_dosya_degismez() {
    let root = mini_crate("allow_reject");
    let mut session = Session::new(root.clone());
    let mut repair = RepairController::default();
    // Izin "evet", onay "hayir": sandbox calisir ama gercek dosyaya yazilmaz.
    let mut prompter = FakePrompter::with(&[true, false]);

    let outcome = apply_directly(&mut session, &mut repair, &mut prompter).await;

    assert!(
        outcome.output.starts_with("PATCH_REJECTED"),
        "{}",
        outcome.output
    );
    assert_eq!(repair.verify_attempts(), 1);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(main_rs(&root), MINI_MAIN);
    assert!(!root.join(".fiko_backups").exists());
    assert!(session.last_patch.is_none());

    let _ = fs::remove_dir_all(&root);
}
