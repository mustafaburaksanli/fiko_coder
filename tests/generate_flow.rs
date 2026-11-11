//! v0.9.1 Generate akisinin modelden bagimsiz guvenlik testleri.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fiko_coder::agent::{Prompter, Session};
use fiko_coder::execution::TaskPhase;
use fiko_coder::intent::{TaskIntent, WriteAuthority};
use fiko_coder::orchestrator::{ALL_TOOLS, Orchestrator, ToolStats};
use serde_json::{Value, json};

const LANDING_REQUEST: &str = include_str!("fixtures/landing_page_request.txt");
const MAIN: &str = "fn main() { println!(\"korunacak\"); }\n";
const CARGO: &str =
    "[package]\nname = \"generate_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

#[derive(Default)]
struct NoPrompts {
    asked: Vec<String>,
}

impl Prompter for NoPrompts {
    fn ask_yes(&mut self, question: &str) -> io::Result<bool> {
        self.asked.push(question.to_owned());
        // Genel bir "evet" Generate kisitlamalarini asamamali.
        Ok(true)
    }
}

struct Project(PathBuf);

impl Project {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fiko_generate_it_{tag}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), CARGO).unwrap();
        fs::write(root.join("src/main.rs"), MAIN).unwrap();
        fs::write(root.join("keep.txt"), b"kullanici dosyasi\n").unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, entries);
                } else {
                    entries.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut entries = BTreeMap::new();
        visit(&self.0, &self.0, &mut entries);
        entries
    }

    fn assert_unchanged(&self, before: &BTreeMap<PathBuf, Vec<u8>>) {
        assert_eq!(&self.snapshot(), before);
        assert!(!self.0.join("target").exists(), "cargo calismamali");
        assert!(
            !self.0.join(".fiko_backups").exists(),
            "yedek/patch akisi baslamamali"
        );
        assert!(
            !self.0.join("index.html").exists(),
            "kod yaniti dosya olusturmamali"
        );
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Yalnizca bu testin olusturdugu, temp altindaki benzersiz kok temizlenir.
        let temporary_root = std::env::temp_dir().canonicalize().unwrap();
        assert!(self.0.starts_with(&temporary_root));
        assert!(
            self.0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("fiko_generate_it_")
        );
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn arguments_for(name: &str) -> Value {
    match name {
        "read_file" => json!({"path": "src/main.rs"}),
        "list_files" => json!({"path": "."}),
        "read_lines" => json!({"path": "src/main.rs", "start_line": 1, "end_line": 1}),
        "search_code" => json!({"query": "fn main"}),
        "run_tests" => json!({"mode": "test"}),
        _ => json!({
            "path": "src/main.rs",
            "old_text": MAIN,
            "new_text": "fn main() {}\n",
            "mode": "test"
        }),
    }
}

#[test]
fn ozgun_landing_page_istegi_aracsiz_uretim_olarak_tamamlanir() {
    let project = Project::new("landing");
    let before = project.snapshot();
    let mut task = Orchestrator::for_request(LANDING_REQUEST);

    assert_eq!(task.intent(), TaskIntent::Generate);
    assert_eq!(task.authority(), WriteAuthority::Denied);
    assert_eq!(task.phase(), TaskPhase::Created);
    assert!(task.intent_line().starts_with("[NIYET] Generate"));
    assert!(task.request_note().contains("Generate"));
    assert!(!task.request_note().contains("run_tests ile gercek projede"));

    task.finish_answer(
        "```html\n<!doctype html><html lang=\"tr\"><title>FIKO CODER</title></html>\n```",
    );

    assert_eq!(task.phase(), TaskPhase::Completed);
    assert_eq!(task.repair.execution().trace_line(), "Created -> Completed");
    assert_eq!(task.stats(), &ToolStats::default());
    assert_eq!(task.repair.verify_attempts(), 0);
    assert!(task.call_log().is_empty());
    assert!(!task.applied());
    let status = task.status_report();
    assert!(status.contains("DEGISMEDI"));
    assert!(status.contains("derleme ve test CALISTIRILMADI"));
    assert!(status.contains("dosya olusturulmadi veya kaydedilmedi"));
    assert!(status.contains("0 cagri (0 calistirildi)"));
    assert!(!status.contains("cargo check + cargo test gecti"));
    project.assert_unchanged(&before);
}

#[tokio::test]
async fn generate_tum_araclari_ve_gizlenmis_adlari_host_tarafinda_reddeder() {
    let project = Project::new("tools");
    let before = project.snapshot();
    let mut session = Session::new(project.0.clone());
    let mut prompter = NoPrompts::default();

    for name in ALL_TOOLS.iter().copied().chain([
        "Apply_Patch",
        " apply_patch",
        "apply_patch ",
        "functions.apply_patch",
        "write_file",
        "run_shell",
        "read_file; cargo test",
        "",
    ]) {
        let mut task = Orchestrator::for_request(LANDING_REQUEST);
        let outcome = task
            .execute(&mut session, &mut prompter, name, &arguments_for(name))
            .await
            .unwrap();

        assert!(
            outcome.output.starts_with("INTENT_BLOCKED"),
            "{name:?}: {}",
            outcome.output
        );
        assert_eq!(task.stats().calls, 1, "{name:?}");
        assert_eq!(task.stats().dispatched, 0, "{name:?}");
        assert_eq!(task.stats().intent_blocked, 1, "{name:?}");
        assert_eq!(task.stats().invalid_args, 0, "{name:?}");
        assert_eq!(task.repair.verify_attempts(), 0, "{name:?}");
        assert!(!task.applied());
        assert!(
            !task
                .repair
                .execution()
                .trace()
                .contains(&TaskPhase::AwaitingApproval)
        );
        assert!(
            !task
                .repair
                .execution()
                .trace()
                .contains(&TaskPhase::Verifying)
        );
        assert!(
            !task
                .repair
                .execution()
                .trace()
                .contains(&TaskPhase::Applying)
        );
        assert_eq!(task.call_log().len(), 1, "reddedilen girisim kaydedilmeli");
        assert!(session.last_patch.is_none());
        assert!(prompter.asked.is_empty(), "{name:?}: {:?}", prompter.asked);
        project.assert_unchanged(&before);
    }
}

#[tokio::test]
async fn generate_gecersiz_argumanla_bile_arac_yoluna_giremez() {
    let project = Project::new("invalid");
    let before = project.snapshot();
    let mut session = Session::new(project.0.clone());
    let mut prompter = NoPrompts::default();
    let mut task = Orchestrator::for_request("Only generate HTML; do not run any tools.");

    assert_eq!(task.intent(), TaskIntent::Generate);
    let outcome = task
        .execute(
            &mut session,
            &mut prompter,
            "verify_candidate",
            &Value::Null,
        )
        .await
        .unwrap();
    assert!(
        outcome.output.starts_with("INTENT_BLOCKED"),
        "{}",
        outcome.output
    );
    assert_eq!(task.stats().dispatched, 0);
    assert_eq!(task.stats().invalid_args, 0);
    assert_eq!(task.stats().intent_blocked, 1);
    assert!(prompter.asked.is_empty());
    assert_eq!(task.repair.verify_attempts(), 0);
    project.assert_unchanged(&before);
}

#[test]
fn karma_gorevlerde_yazma_sinirlari_korunur() {
    let modify = Orchestrator::for_request("Update src/main.rs and run cargo test.");
    assert_eq!(modify.intent(), TaskIntent::Modify);
    assert_eq!(modify.authority(), WriteAuthority::Granted);
    assert_eq!(modify.phase(), TaskPhase::Created);

    let generate = Orchestrator::for_request(
        "Explain this code and generate an improved example without changing files.",
    );
    assert_eq!(generate.intent(), TaskIntent::Generate);
    assert_eq!(generate.authority(), WriteAuthority::Denied);

    let conflict = Orchestrator::for_request("Create a file, but don't write anything to disk.");
    assert_eq!(conflict.authority(), WriteAuthority::Denied);
    assert_eq!(conflict.stats(), &ToolStats::default());
}
