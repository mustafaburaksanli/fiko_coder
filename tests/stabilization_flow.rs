//! Intent kararinin gercek orkestrator yetkisine ve arac kapisina yansimasi.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fiko_coder::agent::{Prompter, Session};
use fiko_coder::execution::TaskPhase;
use fiko_coder::intent::{TaskIntent, WriteAuthority};
use fiko_coder::orchestrator::{ALL_TOOLS, Orchestrator};
use serde_json::{Value, json};

const MAIN: &str = "fn main() { println!(\"korunacak\"); }\n";
const CODE_ONLY: &str =
    "Write a Rust function for src/main.rs. Return it in one code block. Do not call any tools.";
const READ_ONLY: &[(&str, TaskIntent)] = &[
    (
        "Review the create and update functions in src/main.rs.",
        TaskIntent::Analyze,
    ),
    (
        "src/main.rs içindeki update fonksiyonunu açıkla.",
        TaskIntent::Explain,
    ),
];
const NEGATED: &[&str] = &[
    "Do not make changes to src/main.rs.",
    "You mustn't modify src/main.rs.",
    "You mustn’t modify src/main.rs.",
    "You shouldn't change src/main.rs.",
    "You shouldn’t change src/main.rs.",
];

#[derive(Default)]
struct NoPrompts(Vec<String>);

impl Prompter for NoPrompts {
    fn ask_yes(&mut self, question: &str) -> io::Result<bool> {
        self.0.push(question.to_owned());
        // Yanlis bir izin sorusu, varsayilan ret ile gizlenmemeli.
        Ok(true)
    }
}

// Nanosecond timestamps alone can alias across parallel tests on Windows.
static NEXT_PROJECT_ID: AtomicU64 = AtomicU64::new(0);

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fiko_stabilization_it_{}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_PROJECT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"stabilization_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), MAIN).unwrap();
        fs::write(root.join("keep.txt"), b"kullanici verisi\n").unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(&self.0, &self.0, &mut files);
        files
    }

    fn assert_unchanged(&self, before: &BTreeMap<PathBuf, Vec<u8>>) {
        assert_eq!(&self.snapshot(), before);
        assert!(!self.0.join("target").exists());
        assert!(!self.0.join(".fiko_backups").exists());
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Never panic from Drop while a test is already unwinding: on Windows,
        // a second panic can abort the whole test process (0xc0000409).
        let Ok(temporary_root) = std::env::temp_dir().canonicalize() else {
            eprintln!("stabilization fixture: cannot resolve temporary root for cleanup");
            return;
        };
        let trusted_name = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("fiko_stabilization_it_"));
        if self.0.parent() != Some(temporary_root.as_path()) || !trusted_name {
            eprintln!("stabilization fixture: refusing cleanup outside expected temp root");
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("stabilization fixture: cleanup failed: {error}");
        }
    }
}

#[test]
fn parallel_fixture_paths_are_distinct() {
    let projects = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16).map(|_| scope.spawn(Project::new)).collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let distinct: BTreeSet<_> = projects.iter().map(|project| project.0.clone()).collect();
    assert_eq!(distinct.len(), projects.len());
    for project in &projects {
        assert!(project.0.join("src/main.rs").is_file());
    }
}

fn patch() -> Value {
    json!({"path": "src/main.rs", "old_text": MAIN, "new_text": "fn main() {}\n", "mode": "check"})
}

#[test]
fn inceleme_bulgulari_siniflandirma_ve_yetkiyle_dogrulanmali() {
    let mut failures = Vec::new();
    for &(request, expected) in READ_ONLY.iter().chain(
        [
            (CODE_ONLY, TaskIntent::Generate),
            (
                "Generate a Rust function named reclaim.",
                TaskIntent::Generate,
            ),
            (
                "Güncelleme yap: src/main.rs başlığını değiştir.",
                TaskIntent::Modify,
            ),
            ("Run cargo test to find failures.", TaskIntent::Test),
        ]
        .iter(),
    ) {
        let task = Orchestrator::for_request(request);
        let authority = expected.write_authority();
        if task.intent() != expected || task.authority() != authority {
            failures.push(format!(
                "{request:?}: {:?}/{:?}; beklenen {expected:?}/{authority:?}",
                task.intent(),
                task.authority()
            ));
        }
    }
    for &request in NEGATED {
        let task = Orchestrator::for_request(request);
        if task.intent() == TaskIntent::Modify
            || task.authority() != WriteAuthority::Denied
            || !task.decision().write_forbidden
        {
            failures.push(format!(
                "{request:?}: {:?}/{:?}; write_forbidden={}; beklenen Denied/true",
                task.intent(),
                task.authority(),
                task.decision().write_forbidden
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn fonksiyon_adlarini_incelemek_patch_akisina_girmemeli() {
    let project = Project::new();
    let before = project.snapshot();
    let mut session = Session::new(project.0.clone());
    let mut prompter = NoPrompts::default();
    for &(request, expected) in READ_ONLY {
        let mut task = Orchestrator::for_request(request);
        assert_eq!(task.intent(), expected, "{request}");
        assert_eq!(task.authority(), WriteAuthority::Denied);
        for name in ["verify_candidate", "apply_patch"] {
            let result = task
                .execute(&mut session, &mut prompter, name, &patch())
                .await
                .unwrap();
            assert!(
                result.output.starts_with("INTENT_BLOCKED"),
                "{request}: {}",
                result.output
            );
        }
        assert_eq!(task.stats().calls, 2);
        assert_eq!(task.stats().intent_blocked, 2);
        assert_eq!(task.stats().dispatched, 0);
        assert_eq!(task.repair.verify_attempts(), 0);
        assert_eq!(task.repair.execution().trace(), &[TaskPhase::Created]);
        task.finish_answer("Okunan kod aciklandi.");
        assert_eq!(task.phase(), TaskPhase::Completed);
        assert!(!task.applied());
    }
    assert!(prompter.0.is_empty());
    assert!(session.last_patch.is_none());
    project.assert_unchanged(&before);
}

#[tokio::test]
async fn acik_yazma_yasagi_yeni_onay_sorusuna_donusmemeli() {
    let project = Project::new();
    let before = project.snapshot();
    let mut session = Session::new(project.0.clone());
    let mut prompter = NoPrompts::default();
    for &request in NEGATED {
        let mut task = Orchestrator::for_request(request);
        assert_ne!(task.intent(), TaskIntent::Modify, "{request}");
        assert!(task.decision().write_forbidden, "{request}");
        assert_eq!(task.authority(), WriteAuthority::Denied, "{request}");
        for name in ["verify_candidate", "apply_patch"] {
            let result = task
                .execute(&mut session, &mut prompter, name, &patch())
                .await
                .unwrap();
            assert!(
                result.output.starts_with("INTENT_BLOCKED"),
                "{request}: {}",
                result.output
            );
        }
        assert_eq!(task.stats().dispatched, 0);
        assert_eq!(task.repair.verify_attempts(), 0);
        assert_eq!(task.repair.execution().trace(), &[TaskPhase::Created]);
    }
    assert!(prompter.0.is_empty());
    assert!(session.last_patch.is_none());
    project.assert_unchanged(&before);
}

#[tokio::test]
async fn dosya_referansli_kod_yaniti_tum_araclari_engellemeli() {
    let project = Project::new();
    let before = project.snapshot();
    let mut session = Session::new(project.0.clone());
    let mut prompter = NoPrompts::default();
    let mut task = Orchestrator::for_request(CODE_ONLY);
    assert_eq!(task.intent(), TaskIntent::Generate);
    assert_eq!(task.authority(), WriteAuthority::Denied);
    let names: Vec<&str> = ALL_TOOLS
        .iter()
        .copied()
        .chain(["functions.apply_patch", "write_file", "run_shell"])
        .collect();
    for (index, name) in names.iter().enumerate() {
        // Normal ve bozuk argumanlar ayni Generate kapisinda durmali.
        let args = if index % 2 == 0 { patch() } else { Value::Null };
        let result = task
            .execute(&mut session, &mut prompter, name, &args)
            .await
            .unwrap();
        assert!(
            result.output.starts_with("INTENT_BLOCKED"),
            "{name}: {}",
            result.output
        );
        assert_eq!(task.stats().dispatched, 0);
        assert_eq!(task.repair.execution().trace(), &[TaskPhase::Created]);
    }
    assert_eq!(task.stats().calls, names.len());
    assert_eq!(task.stats().intent_blocked, names.len());
    assert_eq!(task.repair.verify_attempts(), 0);
    task.finish_answer("```rust\nfn example() {}\n```");
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(!task.applied());
    assert!(prompter.0.is_empty());
    assert!(session.last_patch.is_none());
    project.assert_unchanged(&before);
}
