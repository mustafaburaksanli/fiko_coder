// Regression tests for verified single-file handoff.
// The real HTTP/model loop is used; no live Ollama service is required.
fn handoff_tool(name: &str, args: Value) -> Value {
    json!({"message":{"role":"assistant","tool_calls":[{"function":{"name":name,"arguments":args}}]}})
}

async fn handoff_run(
    tag: &str,
    new_text: &str,
    approvals: &[bool],
) -> (Orchestrator, Session, ScriptedPrompter, Vec<Value>) {
    let root = mini_crate(tag);
    let mut session = Session::new(root);
    let mut prompter = ScriptedPrompter::with(approvals);
    let (url, server) = mock_ollama(vec![
        handoff_tool("verify_candidate", patch_args(new_text)),
        json!({"message":{"role":"assistant","content":"apply_patch cagiriyorum, degisiklik tamamlandi."}}),
    ]).await;
    let mut messages = conversation("Modify src/main.rs with a safe small change, preserve all existing code.");
    let task = run_task_at(
        &reqwest::Client::builder().no_proxy().build().unwrap(),
        "kontrollu-model",
        &mut session,
        &mut messages,
        &mut prompter,
        &url,
    ).await.unwrap();
    let requests = server.await.unwrap();
    (task, session, prompter, requests)
}

#[tokio::test]
async fn tek_dosya_verify_sonrasi_aracsiz_model_onayla_uygulanmali() {
    let (task, session, prompter, requests) =
        handoff_run("handoff_approved", MINI_GOOD, &[true, true]).await;
    let source = fs::read_to_string(session.root.join("src/main.rs")).unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 2, "verify consent and real apply approval");
    assert_eq!(task.stats().dispatched, 2);
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(task.applied());
    assert!(source.contains(MINI_GOOD));
    assert_eq!(task.memory_finish().outcome, "verified");
    remove(&session.root);
}

#[tokio::test]
async fn tek_dosya_handoff_onayi_reddedilirse_yazilmamali() {
    let (task, session, prompter, requests) =
        handoff_run("handoff_denied", MINI_GOOD, &[true, false]).await;
    let source = fs::read_to_string(session.root.join("src/main.rs")).unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(task.stats().dispatched, 2);
    assert!(!task.applied());
    assert_eq!(task.phase(), TaskPhase::Blocked);
    assert_eq!(source, MINI_MAIN);
    assert!(!session.root.join(".fiko_backups").exists());
    remove(&session.root);
}

#[tokio::test]
async fn modelin_aracsiz_basari_iddiasi_dogrulanmamis_yazma_sayilmamali() {
    let root = mini_crate("handoff_unverified");
    let mut session = Session::new(root.clone());
    let mut prompter = ScriptedPrompter::with(&[]);
    let (url, server) = mock_ollama(vec![
        json!({"message":{"role":"assistant","content":"PATCH_VERIFIED: kodu degistirdim"}}),
    ]).await;
    let mut messages = conversation("Modify src/main.rs to improve the code.");
    let task = run_task_at(
        &reqwest::Client::builder().no_proxy().build().unwrap(),
        "kontrollu-model",
        &mut session,
        &mut messages,
        &mut prompter,
        &url,
    ).await.unwrap();
    let _ = server.await.unwrap();
    assert_eq!(task.stats().dispatched, 0);
    assert!(!task.applied());
    assert!(prompter.asked.is_empty());
    assert_eq!(fs::read_to_string(root.join("src/main.rs")).unwrap(), MINI_MAIN);
    remove(&root);
}
