// agent::tests kapsaminda include edilir; yeni eslestirme modu gercek HTTP/agent yolunda sinanir.

const MIXED_EOL_REQUEST: &str = "Modify src/math.rs, preserve src/lib.rs without modifying it.";
const MIXED_EOL_PREFIX: &str =
    "// LF ve CRLF birlikte korunmali.\r\npub fn double(x: i32) -> i32 { x * 2 }\n";
const MIXED_EOL_BLOCK: &str = "pub fn quadruple(x: i32) -> i32 {\r\n    x * 4\n}\r\n";
const MIXED_EOL_OLD_LF: &str = "pub fn quadruple(x: i32) -> i32 {\n    x * 4\n}\n";
const MIXED_EOL_SUFFIX: &str = "\n// Bu son kisim da byte duzeyinde korunmali.\r\n";
const MIXED_EOL_ADDITION: &str = concat!(
    "\npub fn quintuple(x: i32) -> i32 { x * 5 }\n",
    "\n#[cfg(test)]\nmod quintuple_tests {\n",
    "    use super::*;\n    #[test]\n    fn existing_and_new_functions() {\n",
    "        assert_eq!(double(3), 6);\n        assert_eq!(quadruple(3), 12);\n",
    "        assert_eq!(quintuple(3), 15);\n    }\n}\n",
);

fn mixed_eol_source() -> String {
    format!("{MIXED_EOL_PREFIX}{MIXED_EOL_BLOCK}{MIXED_EOL_SUFFIX}")
}

fn mixed_eol_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn mixed_eol_project(tag: &str, source: &str) -> PathBuf {
    let root = workspace_onay_project(tag, WORKSPACE_ONAY_LIB);
    fs::write(root.join("src/math.rs"), source).unwrap();
    root
}

fn mixed_eol_args(hash: &str, old: &str, new: &str) -> Value {
    json!({"path":"src/math.rs","old_text":old,"new_text":new,"mode":"check",
        "match_mode":"line_endings","expected_sha256":hash})
}

fn mixed_eol_string_tool(name: &str, args: &Value) -> Value {
    workspace_onay_tool(name, Value::String(args.to_string()))
}

async fn mixed_eol_run(
    root: &Path,
    replies: Vec<Value>,
    trusted: bool,
    answers: &[bool],
) -> (
    Orchestrator,
    Session,
    ScriptedPrompter,
    Vec<Value>,
    Vec<Value>,
) {
    let (url, server) = mock_ollama(replies).await;
    let mut session = if trusted {
        trusted_session(root)
    } else {
        Session::new(root.to_path_buf())
    };
    let mut messages = conversation(MIXED_EOL_REQUEST);
    let mut prompter = ScriptedPrompter::with(answers);
    let task = run_task_at(
        &reqwest::Client::builder().no_proxy().build().unwrap(),
        "kontrollu-model",
        &mut session,
        &mut messages,
        &mut prompter,
        &url,
    )
    .await
    .unwrap();
    (task, session, prompter, messages, server.await.unwrap())
}

fn mixed_eol_plain() -> Value {
    json!({"message":{"role":"assistant","content":"Rust sonucuna gore ana proje degismedi."}})
}

fn mixed_eol_assert_original(root: &Path, source: &str) {
    assert_eq!(
        fs::read(root.join("src/math.rs")).unwrap(),
        source.as_bytes()
    );
    assert_eq!(
        fs::read(root.join("src/lib.rs")).unwrap(),
        WORKSPACE_ONAY_LIB.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        MINI_MAIN
    );
}

#[tokio::test]
async fn mixed_eol_json_lf_korunmali_ve_hashli_optin_yeniden_okumadan_onayla_uygulanmali() {
    let source = mixed_eol_source();
    let root = mixed_eol_project("mixed_eol_recovery", &source);
    let hash = mixed_eol_hash(&fs::read(root.join("src/math.rs")).unwrap());
    let new_text = format!("{MIXED_EOL_OLD_LF}{MIXED_EOL_ADDITION}");
    let strict = json!({"path":"src/math.rs","old_text":MIXED_EOL_OLD_LF,"new_text":new_text,"mode":"check"});
    let opted_in = mixed_eol_args(&hash, MIXED_EOL_OLD_LF, &new_text);
    let (task, session, prompter, messages, requests) = mixed_eol_run(
        &root,
        vec![
            workspace_onay_tool("read_file", json!({"path":"src/math.rs"})),
            workspace_onay_tool(
                "read_lines",
                json!({"path":"src/math.rs","start_line":3,"end_line":5}),
            ),
            mixed_eol_string_tool("verify_candidate", &strict),
            mixed_eol_string_tool("verify_candidate", &opted_in),
            mixed_eol_string_tool("apply_patch", &opted_in),
        ],
        false,
        &[true, true],
    )
    .await;
    assert_eq!(
        requests.len(),
        5,
        "Opt-in icin ek model turu veya tekrar okuma gerekmez."
    );
    assert_eq!(
        patch_recovery_last_tool(requests[1]["messages"].as_array().unwrap(), "read_file"),
        source
    );
    let lines = patch_recovery_last_tool(requests[2]["messages"].as_array().unwrap(), "read_lines");
    let (header, display) = lines.split_once('\n').unwrap();
    let metadata: Value =
        serde_json::from_str(header.strip_prefix("[READ_LINES] ").unwrap()).unwrap();
    assert_eq!(
        metadata["returned_range"],
        json!({"start_line":3,"end_line":5})
    );
    assert_eq!(
        display,
        "   3 | pub fn quadruple(x: i32) -> i32 {\n   4 |     x * 4\n   5 | }"
    );
    assert!(
        !display.contains('\r'),
        "Numarali gorunum kaynak EOL byte'larini tasimaz."
    );
    let assistant = requests[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .unwrap();
    let call = &assistant["tool_calls"][0];
    assert!(
        call["function"]["arguments"].is_string(),
        "Gercek JSON-string arguments yolu sinanmali."
    );
    assert_eq!(tool_arguments(call), strict);
    assert_eq!(tool_arguments(call)["old_text"], MIXED_EOL_OLD_LF);
    let diagnostic = patch_recovery_json_error(patch_recovery_last_tool(
        requests[3]["messages"].as_array().unwrap(),
        "verify_candidate",
    ));
    assert_eq!(diagnostic["error"], "PATCH_MATCH");
    assert_eq!(diagnostic["reason"], "line_endings_mismatch");
    assert_eq!(diagnostic["match_count"], 0);
    assert_eq!(diagnostic["normalized_match_count"], 1);
    assert_eq!(diagnostic["recovery"]["match_mode"], "line_endings");
    assert_eq!(diagnostic["recovery"]["expected_sha256"], hash);
    assert_eq!(
        diagnostic["recovery"]["byte_range"],
        json!({"start":MIXED_EOL_PREFIX.len(),"end":MIXED_EOL_PREFIX.len()+MIXED_EOL_BLOCK.len()})
    );
    assert_eq!(
        diagnostic["recovery"]["range_sha256"],
        mixed_eol_hash(MIXED_EOL_BLOCK.as_bytes())
    );
    assert!(
        patch_recovery_last_tool(
            requests[4]["messages"].as_array().unwrap(),
            "verify_candidate"
        )
        .starts_with("VERIFY_OK")
    );
    assert!(patch_recovery_last_tool(&messages, "apply_patch").starts_with("PATCH_VERIFIED"));
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert_eq!(task.stats().calls, 5);
    assert_eq!(task.stats().dispatched, 5);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(prompter.asked.len(), 2);
    assert!(prompter.asked[0].contains("cargo check"));
    assert!(prompter.asked[1].contains("Patch uygulansin"));
    let finish = task.memory_finish();
    assert_eq!(finish.modified_paths, Some(vec!["src/math.rs".into()]));
    assert_eq!(
        finish
            .commands
            .iter()
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>(),
        ["cargo check", "cargo check", "cargo test"]
    );
    assert!(
        finish
            .commands
            .iter()
            .all(|command| command.status == "succeeded" && command.exit_code == Some(0))
    );
    let expected = format!("{MIXED_EOL_PREFIX}{new_text}{MIXED_EOL_SUFFIX}");
    assert_eq!(
        fs::read(root.join("src/math.rs")).unwrap(),
        expected.as_bytes(),
        "Yalniz eslesen aralik degismeli; new_text ve dis EOL byte'lari korunmali."
    );
    assert_eq!(
        fs::read(root.join("src/lib.rs")).unwrap(),
        WORKSPACE_ONAY_LIB.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        MINI_MAIN
    );
    assert_eq!(session.last_patch.as_ref().unwrap().original, source);
    assert_eq!(
        fs::read(&session.last_patch.as_ref().unwrap().backup_path).unwrap(),
        source.as_bytes()
    );
    remove(&root);
}

#[tokio::test]
async fn mixed_eol_optin_ambiguity_noktalama_ve_eski_hash_cargodan_once_reddedilmeli() {
    let original = mixed_eol_source();
    for (tag, source, old, stale, reason) in [
        (
            "ambiguous",
            format!(
                "{MIXED_EOL_PREFIX}{MIXED_EOL_BLOCK}pub fn quadruple(x: i32) -> i32 {{\n    x * 4\r\n}}\n{MIXED_EOL_SUFFIX}"
            ),
            MIXED_EOL_OLD_LF.to_owned(),
            false,
            "ambiguous",
        ),
        (
            "punctuation",
            original.clone(),
            MIXED_EOL_OLD_LF.replace("x * 4", "x * 4;"),
            false,
            "not_found",
        ),
        (
            "stale_hash",
            format!("{original}// Disaridan degisti.\n"),
            MIXED_EOL_OLD_LF.to_owned(),
            true,
            "source_changed",
        ),
    ] {
        let root = mixed_eol_project(&format!("mixed_eol_{tag}"), &source);
        let hash = if stale {
            mixed_eol_hash(original.as_bytes())
        } else {
            mixed_eol_hash(&fs::read(root.join("src/math.rs")).unwrap())
        };
        let args = mixed_eol_args(
            &hash,
            &old,
            &format!("{MIXED_EOL_OLD_LF}{MIXED_EOL_ADDITION}"),
        );
        let (task, session, prompter, messages, requests) = mixed_eol_run(
            &root,
            vec![
                mixed_eol_string_tool("verify_candidate", &args),
                mixed_eol_plain(),
            ],
            true,
            &[],
        )
        .await;
        assert_eq!(requests.len(), 2);
        let error =
            patch_recovery_json_error(patch_recovery_last_tool(&messages, "verify_candidate"));
        assert_eq!(error["error"], "PATCH_MATCH");
        assert_eq!(error["reason"], reason);
        if stale {
            assert_eq!(error["field"], "expected_sha256");
        }
        if reason == "ambiguous" {
            assert_eq!(error["match_count"], 2);
            assert_eq!(error["normalized_match_count"], 2);
        }
        assert_eq!(task.repair.verify_attempts(), 0);
        assert_eq!(task.repair.approved_attempts(), 0);
        assert!(!task.applied());
        assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
        assert!(task.memory_finish().commands.is_empty());
        assert!(prompter.asked.is_empty());
        assert!(session.last_patch.is_none());
        mixed_eol_assert_original(&root, &source);
        assert!(!root.join("target").exists());
        assert!(!root.join(".fiko_backups").exists());
        remove(&root);
    }
}

#[tokio::test]
async fn mixed_eol_gercek_derleme_hatasi_ana_dosyayi_degistirmemeli() {
    let source = mixed_eol_source();
    let root = mixed_eol_project("mixed_eol_compile_failed", &source);
    let hash = mixed_eol_hash(&fs::read(root.join("src/math.rs")).unwrap());
    let replacement = format!("{MIXED_EOL_OLD_LF}\npub fn quintuple() -> i32 {{ \"bozuk\" }}\n");
    let args = mixed_eol_args(&hash, MIXED_EOL_OLD_LF, &replacement);
    let (task, session, prompter, messages, requests) = mixed_eol_run(
        &root,
        vec![
            mixed_eol_string_tool("verify_candidate", &args),
            mixed_eol_plain(),
        ],
        true,
        &[],
    )
    .await;
    assert_eq!(requests.len(), 2);
    assert!(patch_recovery_last_tool(&messages, "verify_candidate").starts_with("VERIFY_FAILED"));
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    let finish = task.memory_finish();
    assert_eq!(finish.modified_paths, Some(vec![]));
    assert_eq!(finish.commands.len(), 1);
    assert_eq!(finish.commands[0].command, "cargo check");
    assert_eq!(finish.commands[0].status, "failed");
    assert!(finish.commands[0].exit_code.is_some_and(|code| code != 0));
    assert!(prompter.asked.is_empty());
    assert!(session.last_patch.is_none());
    mixed_eol_assert_original(&root, &source);
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
}

#[tokio::test]
async fn mixed_eol_son_test_hatasinda_karisik_byte_dizisi_tam_geri_alinmali() {
    let source = mixed_eol_source();
    let root = mixed_eol_project("mixed_eol_rollback", &source);
    let hash = mixed_eol_hash(&fs::read(root.join("src/math.rs")).unwrap());
    let replacement = format!(
        "{MIXED_EOL_OLD_LF}{}",
        MIXED_EOL_ADDITION.replace("quintuple(3), 15", "quintuple(3), 16")
    );
    let args = mixed_eol_args(&hash, MIXED_EOL_OLD_LF, &replacement);
    let (task, session, prompter, _, requests) = mixed_eol_run(
        &root,
        vec![
            mixed_eol_string_tool("verify_candidate", &args),
            mixed_eol_string_tool("apply_patch", &args),
        ],
        true,
        &[true, false],
    )
    .await;
    assert_eq!(requests.len(), 2);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(
        prompter.asked.len(),
        2,
        "Uygulama onayi ve hata sonrasi devam karari ayri olmali."
    );
    assert!(!task.applied());
    let finish = task.memory_finish();
    assert_eq!(finish.outcome, "rolled_back");
    assert_eq!(finish.rolled_back, Some(true));
    assert_eq!(finish.modified_paths, Some(vec!["src/math.rs".into()]));
    assert_eq!(finish.commands.len(), 3);
    assert_eq!(finish.commands[2].command, "cargo test");
    assert_eq!(finish.commands[2].status, "failed");
    assert_eq!(finish.commands[2].exit_code, Some(101));
    assert!(session.last_patch.is_none());
    mixed_eol_assert_original(&root, &source);
    remove(&root);
}

#[tokio::test]
async fn mixed_eol_optin_korunan_lib_icin_yazma_yetkisi_uretmemeli() {
    let source = mixed_eol_source();
    let root = mixed_eol_project("mixed_eol_protected", &source);
    let hash = mixed_eol_hash(&fs::read(root.join("src/lib.rs")).unwrap());
    let args = json!({"path":"src/lib.rs","old_text":WORKSPACE_ONAY_LIB,
        "new_text":"pub mod math;\n// Izinsiz.\n","mode":"check",
        "match_mode":"line_endings","expected_sha256":hash});
    let (task, session, prompter, messages, requests) = mixed_eol_run(
        &root,
        vec![
            mixed_eol_string_tool("verify_candidate", &args),
            mixed_eol_plain(),
        ],
        false,
        &[],
    )
    .await;
    assert_eq!(requests.len(), 2);
    assert!(patch_recovery_last_tool(&messages, "verify_candidate").starts_with("INTENT_BLOCKED"));
    assert_eq!(task.stats().dispatched, 0);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(prompter.asked.is_empty());
    assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
    assert!(task.memory_finish().commands.is_empty());
    assert!(session.last_patch.is_none());
    mixed_eol_assert_original(&root, &source);
    assert!(!root.join("target").exists());
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
}

#[tokio::test]
async fn mixed_eol_dogrulanmis_optin_sonrasi_dis_degisim_apply_onayindan_once_reddedilmeli() {
    let source = mixed_eol_source();
    let root = mixed_eol_project("mixed_eol_verified_drift", &source);
    let hash = mixed_eol_hash(&fs::read(root.join("src/math.rs")).unwrap());
    let replacement = format!("{MIXED_EOL_OLD_LF}{MIXED_EOL_ADDITION}");
    let args = mixed_eol_args(&hash, MIXED_EOL_OLD_LF, &replacement);
    let verify_call = mixed_eol_string_tool("verify_candidate", &args);
    let verify_args = tool_arguments(&verify_call["message"]["tool_calls"][0]);
    let mut session = Session::new(root.clone());
    let mut task = Orchestrator::for_request(MIXED_EOL_REQUEST);
    let mut prompter = ScriptedPrompter::with(&[true]);
    let verified = execute_task_tool(
        &mut task,
        &mut session,
        &mut prompter,
        "verify_candidate",
        &verify_args,
    )
    .await
    .unwrap();
    assert!(
        verified.output.starts_with("VERIFY_OK"),
        "Harici degisim gercek basarili Cargo dogrulamasindan sonra olusmali."
    );
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(
        fs::read(root.join("src/math.rs")).unwrap(),
        source.as_bytes()
    );
    let externally_changed = format!("{source}// Harici degisiklik kalmali.\r\n");
    fs::write(root.join("src/math.rs"), &externally_changed).unwrap();
    let apply_call = mixed_eol_string_tool("apply_patch", &args);
    let apply_args = tool_arguments(&apply_call["message"]["tool_calls"][0]);
    assert_eq!(
        verify_args, apply_args,
        "Apply ayni eski kaynak SHA ve opt-in istegiyle denenmeli."
    );
    let rejected = execute_task_tool(
        &mut task,
        &mut session,
        &mut prompter,
        "apply_patch",
        &apply_args,
    )
    .await
    .unwrap();
    let error = patch_recovery_json_error(&rejected.output);
    assert_eq!(error["error"], "PATCH_MATCH");
    assert_eq!(error["field"], "expected_sha256");
    assert_eq!(error["reason"], "source_changed");
    assert_eq!(
        prompter.asked.len(),
        1,
        "Yalniz gecici Cargo izni sorulmali; eski aday icin uygulama onayi istenmemeli."
    );
    assert!(prompter.asked[0].contains("cargo check"));
    assert_eq!(task.stats().calls, 2);
    assert_eq!(task.stats().dispatched, 2);
    assert_eq!(
        task.repair.verify_attempts(),
        1,
        "Drift sessiz yeniden derleme baslatmamali."
    );
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    task.finish_answer("Harici degisim nedeniyle eski aday uygulanmadi.");
    let finish = task.memory_finish();
    assert_eq!(finish.modified_paths, Some(vec![]));
    assert_eq!(finish.outcome, "not_applied");
    assert_eq!(finish.commands.len(), 1);
    assert_eq!(finish.commands[0].command, "cargo check");
    assert_eq!(finish.commands[0].status, "succeeded");
    assert_eq!(finish.commands[0].exit_code, Some(0));
    mixed_eol_assert_original(&root, &externally_changed);
    assert!(session.last_patch.is_none());
    assert!(session.last_workspace.is_none());
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
}

#[tokio::test]
async fn mixed_eol_strict_apply_hatasi_ayni_old_new_optin_dogrulamasini_kilitlememeli() {
    let source = mixed_eol_source();
    let root = mixed_eol_project("mixed_eol_strict_apply_recovery", &source);
    let hash = mixed_eol_hash(&fs::read(root.join("src/math.rs")).unwrap());
    let replacement = format!("{MIXED_EOL_OLD_LF}{MIXED_EOL_ADDITION}");
    let strict = json!({"path":"src/math.rs","old_text":MIXED_EOL_OLD_LF,"new_text":replacement});
    let opted_in = mixed_eol_args(&hash, MIXED_EOL_OLD_LF, &replacement);
    let (task, session, prompter, messages, requests) = mixed_eol_run(
        &root,
        vec![
            mixed_eol_string_tool("apply_patch", &strict),
            mixed_eol_string_tool("verify_candidate", &opted_in),
            mixed_eol_string_tool("apply_patch", &opted_in),
        ],
        false,
        &[true, true],
    )
    .await;
    assert_eq!(
        requests.len(),
        3,
        "Hazirlanamayan strict aday Pending kaydiyla toparlanmayi engellememeli."
    );
    let error = patch_recovery_json_error(patch_recovery_last_tool(
        requests[1]["messages"].as_array().unwrap(),
        "apply_patch",
    ));
    assert_eq!(error["error"], "PATCH_MATCH");
    assert_eq!(error["reason"], "line_endings_mismatch");
    assert_eq!(error["recovery"]["expected_sha256"], hash);
    let verify_messages = requests[2]["messages"].as_array().unwrap();
    assert!(patch_recovery_last_tool(verify_messages, "verify_candidate").starts_with("VERIFY_OK"));
    assert!(patch_recovery_last_tool(&messages, "apply_patch").starts_with("PATCH_VERIFIED"));
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert_eq!(task.stats().calls, 3);
    assert_eq!(task.stats().dispatched, 3);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(prompter.asked.len(), 2);
    assert!(prompter.asked[0].contains("cargo check"));
    assert!(prompter.asked[1].contains("Patch uygulansin"));
    let finish = task.memory_finish();
    assert_eq!(finish.modified_paths, Some(vec!["src/math.rs".into()]));
    assert_eq!(finish.outcome, "verified");
    assert_eq!(
        finish
            .commands
            .iter()
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>(),
        ["cargo check", "cargo check", "cargo test"]
    );
    assert!(
        finish
            .commands
            .iter()
            .all(|command| command.status == "succeeded" && command.exit_code == Some(0))
    );
    let expected = format!("{MIXED_EOL_PREFIX}{replacement}{MIXED_EOL_SUFFIX}");
    assert_eq!(
        fs::read(root.join("src/math.rs")).unwrap(),
        expected.as_bytes()
    );
    assert_eq!(
        fs::read(root.join("src/lib.rs")).unwrap(),
        WORKSPACE_ONAY_LIB.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        MINI_MAIN
    );
    assert_eq!(session.last_patch.as_ref().unwrap().original, source);
    assert!(session.last_workspace.is_none());
    remove(&root);
}
