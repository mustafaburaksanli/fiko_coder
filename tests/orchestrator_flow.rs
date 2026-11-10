//! FIKO CODER v0.9 - Intelligent Tool Orchestrator entegrasyon testleri.
//!
//! Model devre disi: kullanici istegi metin olarak verilir, niyet Rust tarafinda
//! siniflandirilir ve arac cagrilari dogrudan `Orchestrator::execute` ile yapilir.
//! Amac: dosya degistirme yetkisinin modelin degil Rust tarafinin kararina
//! bagli oldugunu ve v0.8 kilidinin orkestrator katmani altinda da aynen
//! calistigini gostermek.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use fiko_coder::agent::{Prompter, Session};
use fiko_coder::execution::TaskPhase;
use fiko_coder::intent::{TaskIntent, WriteAuthority};
use fiko_coder::orchestrator::Orchestrator;

const MINI_MAIN: &str = "fn main() {\n    let value: i32 = 1;\n    println!(\"{value}\");\n}\n";
const MINI_OLD: &str = "let value: i32 = 1;";
const MINI_GOOD: &str = "let value: i32 = 2;";

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
}

impl Prompter for FakePrompter {
    fn ask_yes(&mut self, question: &str) -> io::Result<bool> {
        self.asked.push(question.to_string());
        Ok(self.answers.pop_front().unwrap_or(false))
    }
}

fn mini_crate(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fiko_orch_it_{tag}_{}_{}",
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

fn patch() -> Value {
    json!({"path": "src/main.rs", "old_text": MINI_OLD, "new_text": MINI_GOOD})
}

fn main_rs(root: &Path) -> String {
    fs::read_to_string(root.join("src/main.rs")).unwrap()
}

#[tokio::test]
async fn analiz_isteginde_model_apply_patch_cagirsa_bile_dosya_degismez() {
    let root = mini_crate("analyze");
    let mut session = Session::new(root.clone());
    let mut task = Orchestrator::for_request("src/main.rs dosyasini incele ve hatalari bul");
    assert_eq!(task.intent(), TaskIntent::Analyze);
    assert_eq!(task.authority(), WriteAuthority::Denied);
    // Kullanici her soruya "evet" dese bile hic soru gelmemeli.
    let mut prompter = FakePrompter::with(&[true, true, true]);

    let outcome = task
        .execute(&mut session, &mut prompter, "apply_patch", &patch())
        .await
        .unwrap();
    assert!(
        outcome.output.starts_with("INTENT_BLOCKED"),
        "{}",
        outcome.output
    );
    assert!(prompter.asked.is_empty(), "{:?}", prompter.asked);
    assert_eq!(main_rs(&root), MINI_MAIN);
    assert!(!root.join(".fiko_backups").exists());
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.stats().intent_blocked, 1);

    task.finish_answer("Inceleme bitti.");
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(!task.applied());
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn acik_degistirme_isteginde_guvenli_akis_tamamlanir() {
    let root = mini_crate("modify");
    let mut session = Session::new(root.clone());
    let mut task =
        Orchestrator::for_request("main.rs icindeki value degerini 2 yap, hatayi duzelt");
    assert_eq!(task.intent(), TaskIntent::Modify);
    // Sira: gecici derleme izni -> patch onayi (niyet sorusu yok).
    let mut prompter = FakePrompter::with(&[true, true]);

    let outcome = task
        .execute(&mut session, &mut prompter, "apply_patch", &patch())
        .await
        .unwrap();
    assert!(
        outcome.output.starts_with("PATCH_VERIFIED"),
        "{}",
        outcome.output
    );
    assert!(outcome.stop.is_some(), "basarili patch sonrasi gorev biter");
    assert_eq!(prompter.asked.len(), 2, "{:?}", prompter.asked);
    assert!(prompter.asked[0].contains("cargo check"));
    assert!(prompter.asked[1].contains("Patch uygulansin"));
    assert!(main_rs(&root).contains(MINI_GOOD));
    assert!(task.applied());
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert_eq!(
        task.repair.execution().trace_line(),
        "Created -> Verifying -> Planning -> AwaitingApproval -> Applying -> Completed"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn belirsiz_istekte_kullanici_izin_vermezse_dosya_degismez() {
    let root = mini_crate("unknown");
    let mut session = Session::new(root.clone());
    let mut task = Orchestrator::for_request("main.rs icindeki value");
    assert_eq!(task.intent(), TaskIntent::Unknown);
    let mut prompter = FakePrompter::with(&[false]);

    let outcome = task
        .execute(&mut session, &mut prompter, "apply_patch", &patch())
        .await
        .unwrap();
    assert!(
        outcome.output.starts_with("INTENT_BLOCKED"),
        "{}",
        outcome.output
    );
    assert_eq!(prompter.asked.len(), 1);
    assert!(prompter.asked[0].contains("kod degisikligine izin"));
    assert_eq!(main_rs(&root), MINI_MAIN);
    assert!(!root.join(".fiko_backups").exists());
    assert_eq!(task.repair.verify_attempts(), 0);
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn v08_kilidi_orkestrator_altinda_aynen_calisir() {
    let root = mini_crate("lock");
    let mut session = Session::new(root.clone());
    let mut task = Orchestrator::for_request("hatayi duzelt");
    assert_eq!(task.intent(), TaskIntent::Modify);
    // Gecici derleme iznine "hayir": Verified Patch Lock devreye girer.
    let mut prompter = FakePrompter::with(&[false]);

    let outcome = task
        .execute(&mut session, &mut prompter, "apply_patch", &patch())
        .await
        .unwrap();
    assert!(
        outcome.output.starts_with("APPLY_BLOCKED"),
        "{}",
        outcome.output
    );
    assert!(outcome.stop.is_some());
    assert_eq!(prompter.asked.len(), 1);
    assert!(prompter.asked[0].contains("cargo check"));
    assert_eq!(main_rs(&root), MINI_MAIN);
    assert!(!root.join(".fiko_backups").exists());
    assert_eq!(task.phase(), TaskPhase::Blocked);
    let _ = fs::remove_dir_all(&root);
}
