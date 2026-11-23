//! Real dependency-free Cargo project acceptance for workspace edit/create/rollback.
//! This tests the Rust host API, not model autonomy or a security sandbox.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fiko_coder::workspace_patch::{self, FileChange};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

const ORIGINAL: &str = "pub fn calculate(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn arithmetic() {\n        assert_eq!(super::calculate(2, 3), 5);\n    }\n}\n";
const UPDATED: &str = "mod math;\npub fn calculate(a: i32, b: i32) -> i32 { math::calculate(a, b) }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn arithmetic() {\n        assert_eq!(super::calculate(2, 3), 6);\n    }\n}\n";
const MATH: &str = "pub fn calculate(a: i32, b: i32) -> i32 { a + b + 1 }\n";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fiko_cargo_workspace_acceptance_{}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            "[package]\nname = \"fiko_workspace_acceptance_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(path.join("src/lib.rs"), ORIGINAL).unwrap();
        fs::write(path.join("protected.txt"), "must remain unchanged\n").unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let Ok(temp) = std::env::temp_dir().canonicalize() else {
            return;
        };
        if self.0.parent() != Some(temp.as_path())
            || !self
                .0
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("fiko_cargo_workspace_acceptance_"))
        {
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("cargo workspace acceptance cleanup: {error}");
        }
    }
}

fn assert_cargo_tests_pass(root: &Path) {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["test", "--offline", "--locked"])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target-acceptance"))
        .output()
        .expect("Cargo executable must be available in Rust CI");
    assert!(
        output.status.success(),
        "Cargo test failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn real_cargo_project_compiles_before_after_multifile_apply_and_after_rollback() {
    let project = Fixture::new();
    let root = &project.0;
    let protected_before = fs::read(root.join("protected.txt")).unwrap();

    assert_cargo_tests_pass(root);

    let changes = [
        FileChange::Edit {
            path: "src/lib.rs".into(),
            old_text: ORIGINAL.into(),
            new_text: UPDATED.into(),
        },
        FileChange::Create {
            path: "src/math.rs".into(),
            content: MATH.into(),
        },
    ];
    let plan = workspace_patch::prepare(root, &changes).unwrap();
    assert_eq!(plan.files.len(), 2);
    let record = workspace_patch::apply(root, &plan).unwrap();

    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), UPDATED);
    assert_eq!(fs::read_to_string(root.join("src/math.rs")).unwrap(), MATH);
    assert_eq!(fs::read(root.join("protected.txt")).unwrap(), protected_before);
    let pending = root.join(".fiko_backups/workspace.pending.json");
    assert!(pending.is_file());
    assert_cargo_tests_pass(root);

    workspace_patch::rollback(root, &record).unwrap();
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), ORIGINAL);
    assert!(!root.join("src/math.rs").exists());
    assert_eq!(fs::read(root.join("protected.txt")).unwrap(), protected_before);
    assert!(!pending.exists());
    assert_cargo_tests_pass(root);
}
