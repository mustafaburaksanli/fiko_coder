//! The recovery restore command must never accept automation or a piped model response.

#[test]
fn recovery_restore_refuses_non_interactive_input() {
    use std::process::{Command, Stdio};

    let exe = env!("CARGO_BIN_EXE_fiko_coder");
    let output = Command::new(exe)
        .arg("--recovery-restore")
        .stdin(Stdio::piped())
        .output()
        .expect("binary must be executable");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("etkilesimli terminalden"), "{stderr}");
}
