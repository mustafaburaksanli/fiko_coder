// agent::tests kapsaminda mevcut HTTP/Cargo fixture'lariyla calisir.

fn v11_plan_paths(task: &Orchestrator) -> Vec<&str> {
    let mut paths = task
        .plan()
        .unwrap()
        .targets
        .iter()
        .map(|target| target.path.as_str())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[tokio::test]
async fn v11_baglam_butcesi_asildiginda_kaynak_kirpilip_modele_sunulmamali() {
    let root = workspace_onay_project("v11_context_budget", WORKSPACE_ONAY_LIB);
    for (path, text) in [
        ("src/context_a.txt", "A".repeat(30_000)),
        ("src/context_b.txt", "B".repeat(30_000)),
        ("src/context_c.txt", "ONLY_C_BODY_".repeat(2_700)),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    let mut prompter = ScriptedPrompter::with(&[]);
    let (task, _, _, requests) = workspace_onay_run_request(
        &root,
        "Modify src/lib.rs in this project.",
        vec![
            workspace_onay_tool("read_file", json!({"path":"src/context_a.txt"})),
            workspace_onay_tool("read_file", json!({"path":"src/context_b.txt"})),
            workspace_onay_tool("read_file", json!({"path":"src/context_c.txt"})),
            workspace_planlama_plain("Henuz bir aday hazirlamadim."),
            workspace_planlama_plain("Yine sadece aciklama yapiyorum."),
        ],
        &mut prompter,
    )
    .await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    remove(&root);
    assert_eq!(requests.len(), 5);
    assert_eq!(task.context_bytes(), 60_000);
    let third = requests[3]["messages"].as_array().unwrap().last().unwrap();
    assert!(
        third["content"]
            .as_str()
            .unwrap()
            .starts_with("BAGLAM_SINIRI:")
    );
    assert!(!requests[3].to_string().contains("ONLY_C_BODY_"));
    assert!(prompter.asked.is_empty());
    assert!(!task.applied());
    assert_eq!(task.phase(), TaskPhase::Failed);
}

#[tokio::test]
async fn v11_onceki_gorevin_okuma_govdesi_yeni_modify_baglamina_tasinmamali() {
    let root = workspace_onay_project("v11_prior_context", WORKSPACE_ONAY_LIB);
    let (url, server) = mock_ollama(vec![
        workspace_planlama_plain("Aday yok."),
        workspace_planlama_plain("Yine aday yok."),
    ])
    .await;
    let mut messages = conversation("Analyze src/math.rs.");
    messages
        .push(workspace_onay_tool("read_file", json!({"path":"src/math.rs"}))["message"].clone());
    messages.push(
        json!({"role":"tool", "tool_name":"read_file", "content":"STALE_SOURCE_BODY".repeat(3000)}),
    );
    let request = "Modify src/lib.rs in this project.";
    messages.push(json!({"role":"user", "content":request}));
    let mut session = Session::new(root.clone());
    let mut prompter = ScriptedPrompter::with(&[]);
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
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].to_string().contains("STALE_SOURCE_BODY"));
    assert_eq!(task.plan().unwrap().goal, request);
    assert_eq!(task.context_bytes(), 0);
    assert_eq!(task.stats().dispatched, 0);
    assert!(!task.applied());
    assert!(prompter.asked.is_empty());
}

#[tokio::test]
async fn v11_terminal_goreve_yeni_aday_gondermek_tamamlanan_plani_degistirememeli() {
    let root = workspace_onay_project("v11_terminal_plan", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (mut task, mut session, _, requests) = workspace_onay_run_request(
        &root,
        "Modify the project files.",
        vec![
            workspace_onay_tool("verify_workspace", json!({"changes":workspace_onay_plan()})),
            workspace_onay_plain(),
        ],
        &mut prompter,
    )
    .await;
    assert_eq!(task.phase(), TaskPhase::Completed);
    let plan = task.plan().unwrap().clone();
    let outcome = task.execute(&mut session, &mut prompter, "apply_workspace", &json!({"changes":[{"operation":"create","path":"README.md","content":"uygulanmamis yeni aday"}]})).await.unwrap();
    assert!(!root.join("README.md").exists());
    remove(&root);
    assert!(
        outcome.output.starts_with("TASK_ENDED"),
        "{}",
        outcome.output
    );
    assert_eq!(task.plan(), Some(&plan));
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(task.applied());
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 2);
}

#[test]
fn v11_plan_sadece_acik_modify_icin_olusturulmali_ve_izin_uretememeli() {
    let request = "Modify src/lib.rs, but do not modify files outside this project.";
    let task = Orchestrator::for_request(request);
    let plan = task.plan().expect("Modify icin Rust gorev plani bulunmali");
    assert_eq!(task.intent(), TaskIntent::Modify);
    assert_eq!(plan.goal, request);
    assert_eq!(plan.required_targets, ["src/lib.rs"]);
    assert!(
        !plan.uncertainties.is_empty(),
        "Derleme semantik gereksinimleri ispatlamaz"
    );
    assert_eq!(task.stats().calls, 0);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    assert!(task.status_report().contains("[PLAN]"));

    let forbidden = Orchestrator::for_request("Modify src/lib.rs, but do not modify any files.");
    assert!(forbidden.plan().is_none());
    assert_eq!(forbidden.authority(), crate::intent::WriteAuthority::Denied);
}

#[tokio::test]
async fn v11_tek_dosyali_plan_ek_model_turu_veya_ikinci_uygulama_onayi_istememeli() {
    let root = mini_crate("v11_single_plan");
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let args = json!({"path": "src/main.rs", "old_text": MINI_OLD, "new_text": MINI_GOOD});
    let (task, session, _, requests) = workspace_onay_run_request(
        &root,
        "Modify src/main.rs in this project.",
        vec![
            workspace_onay_tool("verify_candidate", args.clone()),
            workspace_onay_tool("apply_patch", args),
        ],
        &mut prompter,
    )
    .await;
    let source = fs::read_to_string(root.join("src/main.rs")).unwrap();
    remove(&root);
    assert_eq!(requests.len(), 2, "Planlama icin ek model cagrisi olmamali");
    assert_eq!(
        prompter.asked.len(),
        2,
        "Bir kod calistirma izni ve bir uygulama onayi"
    );
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(task.applied());
    assert!(session.last_patch.is_some());
    assert_eq!(source, MINI_MAIN.replace(MINI_OLD, MINI_GOOD));
    assert_eq!(v11_plan_paths(&task), ["src/main.rs"]);
    let target = &task.plan().unwrap().targets[0];
    assert_eq!(target.operation, crate::planning::PlanOperation::Edit);
    assert!(!target.purpose.is_empty());
    assert!(task.status_report().contains("[PLAN]"));
}

#[tokio::test]
async fn v11_uc_dosyali_plan_ve_tekrar_okuma_ayni_rust_akisinda_takip_edilmeli() {
    let root = workspace_onay_project("v11_multi_plan", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, messages, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_onay_tool("read_file", json!({"path": "src/math.rs"})),
            workspace_onay_tool("read_file", json!({"path": "./src/math.rs"})),
            workspace_onay_tool(
                "verify_workspace",
                json!({"changes": workspace_onay_plan()}),
            ),
            workspace_onay_plain(),
        ],
        &mut prompter,
    )
    .await;
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let math = fs::read_to_string(root.join("src/math.rs")).unwrap();
    let format = fs::read_to_string(root.join("src/format.rs")).unwrap();
    remove(&root);
    assert_eq!(
        requests.len(),
        4,
        "Okuma/verify disinda ek planlama turu eklenmemeli"
    );
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(task.applied());
    assert!(session.last_workspace.is_some());
    assert_eq!(
        v11_plan_paths(&task),
        ["src/format.rs", "src/lib.rs", "src/math.rs"]
    );
    let plan = task.plan().unwrap();
    assert_eq!(plan.required_targets.len(), 3);
    assert_eq!(
        plan.inspection_targets
            .iter()
            .filter(|path| path.as_str() == "src/math.rs")
            .count(),
        1
    );
    assert_eq!(
        plan.inspected_files
            .iter()
            .filter(|path| path.as_str() == "src/math.rs")
            .count(),
        1,
        "Gercek basarili okuma Rust planina bir kez islenmeli"
    );
    assert_eq!(
        plan.targets
            .iter()
            .filter(|target| target.operation == crate::planning::PlanOperation::Create)
            .count(),
        1
    );
    assert!(!plan.uncertainties.is_empty());
    assert_eq!(task.stats().duplicate_reads, 1);
    assert!(task.stats().saved_chars > 0);
    let reads = messages
        .iter()
        .filter(|message| message["role"] == "tool" && message["tool_name"] == "read_file")
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["content"], WORKSPACE_ONAY_MATH);
    assert!(reads[1]["content"].as_str().unwrap().starts_with("TEKRAR:"));
    assert_eq!(lib, WORKSPACE_ONAY_LIB_UPDATED);
    assert_eq!(math, WORKSPACE_ONAY_MATH_UPDATED);
    assert_eq!(format, WORKSPACE_ONAY_FORMAT);
}

#[tokio::test]
async fn v11_yeni_dosya_plani_create_islemini_acikca_tasimali() {
    let root = workspace_onay_project("v11_create_plan", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, _, requests) = workspace_onay_run_request(&root, "Create src/fresh.rs in this project.", vec![
        workspace_onay_tool("verify_workspace", json!({"changes": [{"operation": "create", "path": "src/fresh.rs", "content": "pub fn fresh() {}\n"}]})),
        workspace_onay_plain(),
    ], &mut prompter).await;
    let content = fs::read_to_string(root.join("src/fresh.rs")).unwrap();
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(session.last_workspace.is_some());
    let plan = task.plan().unwrap();
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].path, "src/fresh.rs");
    assert_eq!(
        plan.targets[0].operation,
        crate::planning::PlanOperation::Create
    );
    assert_eq!(content, "pub fn fresh() {}\n");
}

#[tokio::test]
async fn v11_eksik_ve_gecersiz_plan_dogrulama_ve_onay_uretememeli() {
    for (tag, changes) in [
        (
            "v11_missing_plan",
            json!([{"operation": "edit", "path": "src/lib.rs", "old_text": WORKSPACE_ONAY_LIB, "new_text": "pub mod math;\n// eksik kapsam\n"}]),
        ),
        (
            "v11_invalid_plan",
            json!([{"operation": "rename", "path": "src/lib.rs", "content": "yanlis islem"}]),
        ),
    ] {
        let root = workspace_onay_project(tag, WORKSPACE_ONAY_LIB);
        let mut prompter = ScriptedPrompter::with(&[true, true]);
        let (task, session, _, requests) = workspace_onay_run(
            &root,
            vec![
                workspace_onay_tool("verify_workspace", json!({"changes": changes})),
                workspace_onay_plain(),
            ],
            &mut prompter,
        )
        .await;
        workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
        assert!(!root.join("target").exists());
        remove(&root);
        assert_eq!(requests.len(), 2);
        assert!(prompter.asked.is_empty());
        assert!(!task.applied());
        assert_eq!(task.repair.verify_attempts(), 0);
        assert_eq!(task.repair.approved_attempts(), 0);
        assert_eq!(task.plan().unwrap().required_targets.len(), 3);
        assert!(session.last_workspace.is_none());
    }
}

#[tokio::test]
async fn v11_buyuk_dosya_baglami_sinirli_kalmali_ve_model_metni_plan_olmamali() {
    let root = workspace_onay_project("v11_large_context", WORKSPACE_ONAY_LIB);
    let comments = "// gereksiz buyuk baglam\n".repeat(4000);
    let source = format!("{comments}{WORKSPACE_ONAY_MATH}");
    assert!(source.len() > 64_000);
    fs::write(root.join("src/math.rs"), &source).unwrap();
    let mut prompter = ScriptedPrompter::with(&[]);
    let (task, _, messages, requests) = workspace_onay_run(&root, vec![
        workspace_onay_tool("read_lines", json!({"path": "src/math.rs", "start_line": 4001, "end_line": 4001})),
        json!({"message": {"role": "assistant", "content": "Plani yaptim ve tum degisiklikleri tamamladim."}}),
        json!({"message": {"role": "assistant", "content": "PATCH_VERIFIED: Aciklamam yeterli."}}),
    ], &mut prompter).await;
    let actual = fs::read_to_string(root.join("src/math.rs")).unwrap();
    remove(&root);
    assert_eq!(requests.len(), 3);
    assert_eq!(task.phase(), TaskPhase::Failed);
    assert!(!task.applied());
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(prompter.asked.is_empty());
    assert_eq!(actual, source);
    let read = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_name"] == "read_lines")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(read.contains("pub fn double"));
    assert!(read.len() < 1000);
    assert!(!read.contains(&comments));
    assert!(
        task.plan()
            .unwrap()
            .inspection_targets
            .contains(&"src/math.rs".to_string())
    );
    assert!(
        task.plan()
            .unwrap()
            .inspected_files
            .contains(&"src/math.rs".to_string())
    );
}

#[tokio::test]
async fn v11_plan_ile_farkli_workspace_adayi_dogrulanmis_gibi_uygulanmamali() {
    let root = workspace_onay_project("v11_candidate_mismatch", WORKSPACE_ONAY_LIB);
    let mut changes = workspace_onay_plan();
    changes[0]["new_text"] = json!("pub fn double(value: i32) -> i32 { value * 8 }\n");
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, _, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_onay_tool(
                "verify_workspace",
                json!({"changes": workspace_onay_plan()}),
            ),
            workspace_onay_tool("apply_workspace", json!({"changes": changes})),
        ],
        &mut prompter,
    )
    .await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 1);
    assert_eq!(task.phase(), TaskPhase::Blocked);
    assert!(!task.applied());
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(session.last_workspace.is_none());
    assert_eq!(
        v11_plan_paths(&task),
        ["src/format.rs", "src/lib.rs", "src/math.rs"]
    );
}

#[tokio::test]
async fn v11_plan_dogrulama_sonrasi_drifti_tamamlandi_olarak_gostermemeli() {
    let root = workspace_onay_project("v11_plan_drift", WORKSPACE_ONAY_LIB);
    let manifest = root.join("Cargo.toml");
    let changed = format!(
        "{}\n# onaydan once drift\n",
        fs::read_to_string(&manifest).unwrap()
    );
    let (url, server) = workspace_onay_mock_drift_before_answer(manifest, changed).await;
    let mut session = Session::new(root.clone());
    let mut messages = conversation(WORKSPACE_ONAY_REQUEST);
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
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert_eq!(task.phase(), TaskPhase::Blocked);
    assert_eq!(
        task.project_state(),
        crate::orchestrator::ProjectState::Uncertain
    );
    assert_eq!(prompter.asked.len(), 1);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    let report = task.status_report();
    assert!(report.contains("[PLAN]"));
    assert!(report.contains("Blocked"));
    assert!(report.contains("BELIRSIZ"));
}

#[tokio::test]
async fn v11_plan_ekrani_ikinci_onay_veya_ret_sonrasi_uygulama_uretememeli() {
    let root = workspace_onay_project("v11_plan_denied", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[true, false, true]);
    let (task, session, _, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_onay_tool(
                "verify_workspace",
                json!({"changes": workspace_onay_plan()}),
            ),
            workspace_onay_plain(),
        ],
        &mut prompter,
    )
    .await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(task.phase(), TaskPhase::Blocked);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    assert!(session.last_workspace.is_none());
    assert_eq!(
        v11_plan_paths(&task),
        ["src/format.rs", "src/lib.rs", "src/math.rs"]
    );
}

#[tokio::test]
async fn v11_basarili_rollback_uygulanmamis_gibi_raporlanmamali() {
    let failure = "#[test]\nfn v11_controlled_failure() { panic!(\"son dogrulama hatasi\"); }\n";
    let lib = format!("{WORKSPACE_ONAY_LIB}{failure}");
    let updated = format!("{WORKSPACE_ONAY_LIB_UPDATED}{failure}");
    let root = workspace_onay_project("v11_rollback_history", &lib);
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, _, requests) = workspace_onay_run(&root, vec![
        workspace_onay_tool("verify_workspace", json!({"changes": workspace_onay_changes(&lib, &updated, WORKSPACE_ONAY_MATH_UPDATED), "mode": "check"})),
        workspace_onay_plain(),
    ], &mut prompter).await;
    workspace_onay_assert_original(&root, &lib);
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(task.phase(), TaskPhase::Failed);
    assert_eq!(
        task.project_state(),
        crate::orchestrator::ProjectState::Unchanged
    );
    assert_eq!(task.repair.approved_attempts(), 1);
    assert!(!task.applied());
    assert!(session.last_workspace.is_none());
    let report = task.status_report();
    let first = report.lines().next().unwrap();
    let folded = crate::intent::fold(first);
    assert!(
        folded.contains("uygulandi") && folded.contains("geri alindi"),
        "Gercek olay gecmisi korunmali: {first}"
    );
    assert!(
        !folded.contains("degismedi"),
        "Uygulama ve rollback gecmisi saklanmamali: {first}"
    );
    assert!(report.contains("Cikis kodu: 101"));
    assert!(report.contains("[PLAN]"));
}

#[tokio::test]
async fn v11_modify_disindaki_niyetler_planlama_maliyeti_almamali() {
    for (tag, request, intent) in [
        (
            "v11_analyze",
            "Analyze src/math.rs without changing files.",
            TaskIntent::Analyze,
        ),
        (
            "v11_explain",
            "Explain src/math.rs without changing files.",
            TaskIntent::Explain,
        ),
        (
            "v11_generate",
            "Only generate a Rust function in the response.",
            TaskIntent::Generate,
        ),
        ("v11_test", "Run cargo test.", TaskIntent::Test),
    ] {
        let root = workspace_onay_project(tag, WORKSPACE_ONAY_LIB);
        let mut prompter = ScriptedPrompter::with(&[]);
        let (task, session, _, requests) = workspace_onay_run_request(&root, request, vec![
            json!({"message": {"role": "assistant", "content": "Istek aciklandi; dosya degisikligi yok."}}),
        ], &mut prompter).await;
        workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
        remove(&root);
        assert_eq!(task.intent(), intent);
        assert!(task.plan().is_none());
        assert_eq!(
            requests.len(),
            1,
            "{tag}: plan icin ekstra LLM turu olmamali"
        );
        assert!(!task.applied());
        assert!(session.last_workspace.is_none());
        assert_eq!(task.repair.approved_attempts(), 0);
        assert!(!task.status_report().contains("[PLAN]"));
    }
}
