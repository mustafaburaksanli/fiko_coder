// agent::tests icinde include edilir; uretim dongusu ve kontrollu HTTP fixture'i kullanilir.

const PATCH_RECOVERY_MATH_LF: &str = concat!(
    "pub fn double(x: i32) -> i32 {\n",
    "    x * 2\n",
    "}\n",
    "\n",
    "pub fn triple(x: i32) -> i32 {\n",
    "    x * 3\n",
    "}\n",
    "\n",
    "#[cfg(test)]\n",
    "mod tests {\n",
    "    use super::*;\n",
    "\n",
    "    #[test]\n",
    "    fn existing_functions() {\n",
    "        assert_eq!(double(3), 6);\n",
    "        assert_eq!(triple(3), 9);\n",
    "    }\n",
    "}\n",
    "\n",
    "// Unique final anchor; preserve the preceding functions.\n",
    "pub fn preserved_anchor(x: i32) -> i32 {\n",
    "    // Keep the unique block intact while extending this module.\n",
    "    x\n",
    "}\n",
);

const PATCH_RECOVERY_ANCHOR_LF: &str = concat!(
    "pub fn preserved_anchor(x: i32) -> i32 {\n",
    "    // Keep the unique block intact while extending this module.\n",
    "    x\n",
    "}\n",
);

const PATCH_RECOVERY_APPEND_LF: &str = concat!(
    "\n",
    "pub fn quadruple(x: i32) -> i32 { x * 4 }\n",
    "\n",
    "#[cfg(test)]\n",
    "mod quadruple_tests {\n",
    "    use super::*;\n",
    "    #[test]\n",
    "    fn quadruple_preserves_existing_functions() {\n",
    "        assert_eq!(quadruple(4), 16);\n",
    "        assert_eq!(double(4), 8);\n",
    "        assert_eq!(triple(4), 12);\n",
    "        assert_eq!(preserved_anchor(4), 4);\n",
    "    }\n",
    "}\n",
);

fn patch_recovery_last_tool<'a>(messages: &'a [Value], name: &str) -> &'a str {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool" && message["tool_name"] == name)
        .and_then(|message| message["content"].as_str())
        .expect("Beklenen gercek tool yaniti bulunamadi.")
}

fn patch_recovery_json_error(output: &str) -> Value {
    serde_json::from_str(
        output
            .strip_prefix("ERROR: ")
            .expect("Tani basarili arac yaniti gibi sunulmamali."),
    )
    .expect("Arac hatasi yapilandirilmis JSON olmali.")
}

#[tokio::test]
async fn patch_recovery_crlf_old_text_ve_eof_duzeltilince_gercek_onayla_uygulanmali() {
    assert_eq!(PATCH_RECOVERY_MATH_LF.lines().count(), 24);
    let root = workspace_onay_project("patch_recovery_crlf", WORKSPACE_ONAY_LIB);
    let original = PATCH_RECOVERY_MATH_LF.replace('\n', "\r\n");
    let anchor = PATCH_RECOVERY_ANCHOR_LF.replace('\n', "\r\n");
    let replacement = format!("{anchor}{}", PATCH_RECOVERY_APPEND_LF.replace('\n', "\r\n"));
    let expected = format!("{original}{}", PATCH_RECOVERY_APPEND_LF.replace('\n', "\r\n"));
    fs::write(root.join("src/math.rs"), &original).unwrap();
    let bad = json!({"path":"src/math.rs","old_text":PATCH_RECOVERY_ANCHOR_LF,"new_text":replacement,"mode":"check"});
    let corrected = json!({"path":"src/math.rs","old_text":anchor,"new_text":replacement,"mode":"check"});
    let (url, server) = mock_ollama(vec![
        workspace_onay_tool("read_file", json!({"path":"src/math.rs"})),
        workspace_onay_tool("verify_candidate", bad),
        workspace_onay_tool("read_lines", json!({"path":"src/math.rs","start_line":21,"end_line":30})),
        workspace_onay_tool("read_lines", json!({"path":"src/math.rs","start_line":21,"end_line":24})),
        workspace_onay_tool("verify_candidate", corrected.clone()),
        workspace_onay_tool("apply_patch", corrected),
    ])
    .await;
    let mut session = Session::new(root.clone());
    let mut messages = conversation("Modify src/math.rs: preserve every existing function and add quadruple with its regression test inside the same file.");
    let mut prompter = ScriptedPrompter::with(&[true, true]);
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
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 6, "Ek model/planning yeniden denemesi olmamali.");
    assert!(requests.len() <= 12);
    assert!(requests.iter().all(|request| !request["messages"].as_array().unwrap().iter()
        .any(|message| message["role"] == "user" && message["content"] == MODIFY_PLAN_RETRY)));
    let read = patch_recovery_last_tool(requests[1]["messages"].as_array().unwrap(), "read_file");
    assert_eq!(read, original, "read_file CRLF byte'larini ham kaynakta korumali.");
    let diagnostic = patch_recovery_json_error(patch_recovery_last_tool(
        requests[2]["messages"].as_array().unwrap(), "verify_candidate",
    ));
    assert_eq!(diagnostic["error"], "PATCH_MATCH");
    assert_eq!(diagnostic["field"], "old_text");
    assert_eq!(diagnostic["reason"], "line_endings_mismatch");
    assert_eq!(diagnostic["match_count"], 0);
    assert_eq!(diagnostic["normalized_match_count"], 1);
    assert_eq!(diagnostic["total_lines"], 24);
    assert_eq!(diagnostic["line_endings"]["file"]["crlf"], 24);
    assert_eq!(diagnostic["line_endings"]["old_text"]["lf"], 4);
    let range = patch_recovery_json_error(patch_recovery_last_tool(
        requests[3]["messages"].as_array().unwrap(), "read_lines",
    ));
    assert_eq!(range["error"], "range_exceeds_eof");
    assert_eq!(range["requested_range"], json!({"start_line":21,"end_line":30}));
    assert_eq!(range["total_lines"], 24);
    assert!(range["returned_range"].is_null());
    assert_eq!(range["suggested_range"], json!({"start_line":21,"end_line":24}));
    assert_eq!(range["range_adjusted"], false);
    let recovered = patch_recovery_last_tool(requests[4]["messages"].as_array().unwrap(), "read_lines");
    let (header, body) = recovered.split_once('\n').unwrap();
    let metadata: Value = serde_json::from_str(header.strip_prefix("[READ_LINES] ").unwrap()).unwrap();
    assert_eq!(metadata["requested_range"], json!({"start_line":21,"end_line":24}));
    assert_eq!(metadata["returned_range"], json!({"start_line":21,"end_line":24}));
    assert_eq!(metadata["total_lines"], 24);
    assert_eq!(metadata["range_adjusted"], false);
    assert_eq!(metadata["output_truncated"], false);
    assert_eq!(metadata["line_truncated"], false);
    assert_eq!(body, PATCH_RECOVERY_ANCHOR_LF.lines().enumerate()
        .map(|(offset,line)| format!("{:>4} | {line}", offset+21)).collect::<Vec<_>>().join("\n"));
    assert!(patch_recovery_last_tool(requests[5]["messages"].as_array().unwrap(), "verify_candidate").starts_with("VERIFY_OK"));
    assert_eq!(task.intent(), TaskIntent::Modify);
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(task.applied());
    assert_eq!(task.stats().calls, 6);
    assert_eq!(task.stats().dispatched, 6);
    assert_eq!(task.repair.verify_attempts(), 1, "Eslesme hatasi Cargo denemesi degildir.");
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(prompter.asked.len(), 2, "Bir gecici Cargo izni ve tek uygulama onayi gerekir.");
    assert!(prompter.asked[0].contains("cargo check"));
    assert!(prompter.asked[1].contains("Patch uygulansin"));
    let finish = task.memory_finish();
    assert_eq!(finish.modified_paths, Some(vec!["src/math.rs".into()]));
    assert_eq!(finish.commands.iter().map(|command| command.command.as_str()).collect::<Vec<_>>(), ["cargo check", "cargo check", "cargo test"]);
    assert!(finish.commands.iter().all(|command| command.status == "succeeded" && command.exit_code == Some(0)));
    assert_eq!(fs::read_to_string(root.join("src/math.rs")).unwrap(), expected);
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), WORKSPACE_ONAY_LIB);
    assert_eq!(fs::read_to_string(root.join("src/main.rs")).unwrap(), MINI_MAIN);
    let record = session.last_patch.as_ref().unwrap();
    assert_eq!(record.original, original);
    assert_eq!(fs::read_to_string(&record.backup_path).unwrap(), original);
    assert!(session.last_workspace.is_none());
    remove(&root);
}

#[tokio::test]
async fn patch_recovery_ayni_hatali_old_text_yol_ve_new_text_degisince_tekrar_derlenmemeli() {
    let root = workspace_onay_project("patch_recovery_repeat", WORKSPACE_ONAY_LIB);
    let bad_old = "pub fn double(value: i32) -> i32 { value * 2";
    let paths = ["src/math.rs", "./src/math.rs", "src\\./math.rs"];
    let replies = paths.iter().enumerate().map(|(index,path)| workspace_onay_tool(
        "verify_candidate", json!({"path":path,"old_text":bad_old,
            "new_text":format!("pub fn double(value: i32) -> i32 {{ value * {} }}", index+3),"mode":"check"}),
    )).collect();
    let (url, server) = mock_ollama(replies).await;
    let mut session = trusted_session(&root);
    let mut messages = conversation("Modify src/math.rs and preserve the existing function.");
    let mut prompter = ScriptedPrompter::with(&[]);
    let task = run_task_at(
        &reqwest::Client::builder().no_proxy().build().unwrap(), "kontrollu-model", &mut session,
        &mut messages, &mut prompter, &url,
    ).await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3, "Ucuncu esdeger hatali oneride durmali.");
    let first = patch_recovery_json_error(patch_recovery_last_tool(requests[1]["messages"].as_array().unwrap(), "verify_candidate"));
    assert_eq!(first["error"], "PATCH_MATCH");
    assert_eq!(first["reason"], "partial_line");
    assert_eq!(first["match_count"], 0);
    assert_eq!(first["literal_match_count"], 1);
    let skipped = patch_recovery_json_error(patch_recovery_last_tool(requests[2]["messages"].as_array().unwrap(), "verify_candidate"));
    let stopped = patch_recovery_json_error(patch_recovery_last_tool(&messages, "verify_candidate"));
    assert_eq!(skipped["error"], "PATCH_MATCH_RETRY");
    assert_eq!(skipped["attempt"], 2);
    assert_eq!(skipped["stopped"], false);
    assert_eq!(stopped["error"], "PATCH_MATCH_RETRY");
    assert_eq!(stopped["attempt"], 3);
    assert_eq!(stopped["limit"], 3);
    assert_eq!(stopped["stopped"], true);
    assert_eq!(skipped["argument_fingerprint"], stopped["argument_fingerprint"]);
    assert_eq!(task.stats().calls, 3);
    assert_eq!(task.stats().dispatched, 1);
    assert_eq!(task.stats().repeated_failures, 2);
    assert_eq!(task.stats().loop_stops, 1);
    assert_eq!(task.phase(), TaskPhase::Blocked);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
    assert!(task.memory_finish().commands.is_empty());
    assert!(prompter.asked.is_empty());
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    assert_eq!(fs::read_to_string(root.join("src/main.rs")).unwrap(), MINI_MAIN);
    assert!(!root.join("target").exists());
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
}

#[tokio::test]
async fn patch_recovery_workspace_ikinci_eslesme_hatasi_tum_plani_yazmadan_reddetmeli() {
    let root = workspace_onay_project("patch_recovery_workspace", WORKSPACE_ONAY_LIB);
    let changes = json!([
        {"operation":"edit","path":"src/math.rs","old_text":WORKSPACE_ONAY_MATH,"new_text":WORKSPACE_ONAY_MATH_UPDATED},
        {"operation":"edit","path":"src/lib.rs","old_text":"pub mod absent;\n","new_text":"pub mod math;\npub mod absent;\n"}
    ]);
    let (url, server) = mock_ollama(vec![
        workspace_onay_tool("verify_workspace", json!({"changes":changes,"mode":"check"})),
        json!({"message":{"role":"assistant","content":"Ikinci dosyanin eski blogu eslesmedi; plan uygulanmadi."}}),
    ]).await;
    let mut session = Session::new(root.clone());
    let mut messages = conversation("Modify src/math.rs and src/lib.rs together while preserving the existing code.");
    let mut prompter = ScriptedPrompter::with(&[]);
    let task = run_task_at(
        &reqwest::Client::builder().no_proxy().build().unwrap(), "kontrollu-model", &mut session,
        &mut messages, &mut prompter, &url,
    ).await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2, "Hata sonrasi ek planning yeniden denemesi olmamali.");
    let error = patch_recovery_json_error(patch_recovery_last_tool(requests[1]["messages"].as_array().unwrap(), "verify_workspace"));
    assert_eq!(error["error"], "PATCH_MATCH");
    assert_eq!(error["field"], "old_text");
    assert_eq!(error["reason"], "not_found");
    assert_eq!(error["total_lines"], 1);
    assert_eq!(task.intent(), TaskIntent::Modify);
    assert_eq!(task.phase(), TaskPhase::Failed);
    assert_eq!(task.stats().calls, 1);
    assert_eq!(task.stats().dispatched, 1);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
    assert!(task.memory_finish().commands.is_empty());
    assert!(prompter.asked.is_empty(), "Gecersiz plan Cargo/uygulama izni istememeli.");
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    assert_eq!(fs::read_to_string(root.join("src/main.rs")).unwrap(), MINI_MAIN);
    assert!(session.last_patch.is_none());
    assert!(session.last_workspace.is_none());
    assert!(!root.join("target").exists());
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
}
