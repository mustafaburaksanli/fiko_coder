// agent::tests kapsaminda include edilir; ayni uretim dongusu ve fixture'lar kullanilir.

fn workspace_planlama_read(path: &str) -> Value {
    workspace_onay_tool("read_file", json!({"path": path}))
}

fn workspace_planlama_plain(text: &str) -> Value {
    json!({"message": {"role": "assistant", "content": text}})
}

#[tokio::test]
async fn workspace_planlama_http_boyut_siniri_arac_argumanlarini_kirparak_uygulamamali() {
    let root = workspace_onay_project("planning_response_limit", WORKSPACE_ONAY_LIB);
    let reply = json!({"message": {"role": "assistant", "content": "x".repeat(MAX_MODIFY_RESPONSE_BYTES + 1),
        "tool_calls": [{"function": {"name": "apply_workspace", "arguments": {"changes": workspace_onay_plan()}}}]
    }});
    let (url, server) = mock_ollama(vec![reply]).await;
    let mut session = Session::new(root.clone());
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let mut messages = conversation(WORKSPACE_ONAY_REQUEST);
    let result = run_task_at(
        &reqwest::Client::builder().no_proxy().build().unwrap(),
        "kontrollu-model",
        &mut session,
        &mut messages,
        &mut prompter,
        &url,
    )
    .await;
    // Host sinirda baglantiyi kapatabilir; teste ait yazar beklemede kalmasin.
    server.abort();
    let _ = server.await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    assert!(!root.join("target").exists());
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
    let error = result
        .err()
        .expect("Asiri model yaniti tumuyle reddedilmeli")
        .to_string();
    assert!(error.contains("2 MiB sinirini asti"), "{error}");
    assert!(prompter.asked.is_empty());
    assert!(session.last_workspace.is_none());
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        0
    );
}

#[tokio::test]
async fn workspace_planlama_iki_okuma_sonrasi_aciklama_bir_kez_araca_yonlendirilmeli() {
    let root = workspace_onay_project("planning_retry_success", WORKSPACE_ONAY_LIB);
    let explanation =
        "old_text satir sonunda newline bulunabilir. Ayni noktayi yeniden acikliyorum. ".repeat(80);
    let (url, mut server) = mock_ollama(vec![
        workspace_planlama_read("src/math.rs"),
        workspace_planlama_read("src/lib.rs"),
        workspace_planlama_plain(&explanation),
        workspace_onay_tool(
            "verify_workspace",
            json!({"changes": workspace_onay_plan()}),
        ),
        workspace_onay_plain(),
    ])
    .await;
    let mut session = Session::new(root.clone());
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let mut messages = conversation(WORKSPACE_ONAY_REQUEST);
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
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let math = fs::read_to_string(root.join("src/math.rs")).unwrap();
    let format = fs::read_to_string(root.join("src/format.rs"));
    remove(&root);
    // Eski uretim aciklamadan sonra kapaniyordu. Bu red durumda mock'un
    // kullanilmayan cevabini bekleyip asil hatayi bir timeout ile gizleme.
    if !task.applied() {
        server.abort();
    }
    assert_eq!(
        task.phase(),
        TaskPhase::Completed,
        "Okuma sonrasi metinsel sapma kontrollu arac yeniden denemesine ulasmali"
    );
    let requests = (&mut server).await.unwrap();
    assert_eq!(requests.len(), 5);
    assert!(task.applied());
    assert_eq!(task.stats().calls, 4);
    assert_eq!(task.stats().dispatched, 4);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(prompter.asked.len(), 2);
    assert!(prompter.asked[0].contains("cargo check"));
    assert!(prompter.asked[1].contains("Tum workspace"));
    assert_eq!(lib, WORKSPACE_ONAY_LIB_UPDATED);
    assert_eq!(math, WORKSPACE_ONAY_MATH_UPDATED);
    assert_eq!(format.unwrap(), WORKSPACE_ONAY_FORMAT);
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        3
    );
    let retry_history = requests[3]["messages"].as_array().unwrap();
    let retry_note = retry_history.last().unwrap();
    assert_eq!(retry_note["role"], "user");
    assert_eq!(retry_note["content"], MODIFY_PLAN_RETRY);
    assert!(MODIFY_PLAN_RETRY.chars().count() <= 1000);
    assert_eq!(
        retry_history
            .iter()
            .filter(|message| message["role"] == "user" && message["content"] == MODIFY_PLAN_RETRY)
            .count(),
        1
    );
    assert_eq!(requests[3]["tools"], tool_definitions());
}

#[tokio::test]
async fn workspace_planlama_iki_okuma_sonrasi_dogrudan_gecerli_plan_ek_retry_istememeli() {
    let root = workspace_onay_project("planning_direct", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, _, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_planlama_read("src/math.rs"),
            workspace_planlama_read("src/lib.rs"),
            workspace_onay_tool(
                "verify_workspace",
                json!({"changes": workspace_onay_plan()}),
            ),
            workspace_onay_plain(),
        ],
        &mut prompter,
    )
    .await;
    let math = fs::read_to_string(root.join("src/math.rs")).unwrap();
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let format = fs::read_to_string(root.join("src/format.rs")).unwrap();
    remove(&root);
    assert_eq!(requests.len(), 4);
    assert_eq!(task.phase(), TaskPhase::Completed);
    assert!(task.applied());
    assert!(session.last_workspace.is_some());
    assert_eq!(task.stats().dispatched, 4);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 1);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(math, WORKSPACE_ONAY_MATH_UPDATED);
    assert_eq!(lib, WORKSPACE_ONAY_LIB_UPDATED);
    assert_eq!(format, WORKSPACE_ONAY_FORMAT);
    for request in requests {
        assert_eq!(request["options"]["num_predict"], 4096);
    }
}

#[tokio::test]
async fn workspace_planlama_uzun_tekrarlanan_aciklama_tek_retry_ve_sinirli_gecmisle_durmali() {
    let root = workspace_onay_project("planning_long_repeat", WORKSPACE_ONAY_LIB);
    let long = "old_text satir sonlari hakkinda ayni uzun aciklama tekrar ediliyor. ".repeat(2000);
    assert!(long.len() > 80_000);
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, messages, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_planlama_read("src/math.rs"),
            workspace_planlama_read("src/lib.rs"),
            workspace_planlama_plain(&long),
            workspace_planlama_plain(&long),
        ],
        &mut prompter,
    )
    .await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    assert!(!root.join("target").exists());
    remove(&root);
    assert_eq!(
        requests.len(),
        4,
        "Yalniz bir kontrollu yeniden deneme olmali"
    );
    assert_eq!(task.phase(), TaskPhase::Failed);
    assert!(!task.applied());
    assert!(session.last_workspace.is_none());
    assert_eq!(task.stats().calls, 2);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(prompter.asked.is_empty());
    let prose: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["content"].as_str())
        .filter(|text| !text.is_empty())
        .collect();
    assert_eq!(prose.len(), 2);
    for text in prose {
        assert!(
            text.chars().count() <= 2_200,
            "Uzun model aciklamasi kontrolsuz saklandi"
        );
        assert!(text.contains("KISALTILDI"), "Kisaltma acik olmali: {text}");
        assert!(!text.contains(&long));
    }
    assert!(
        requests[3].to_string().len() < 40_000,
        "Retry buyuk tekrar metnini modele geri yuklememeli"
    );
    assert_eq!(requests[3]["options"]["num_predict"], 4096);
}

#[tokio::test]
async fn workspace_planlama_hic_arac_yoksa_ikinci_serbest_metin_yazma_yapamamali() {
    let root = workspace_onay_project("planning_no_tools", WORKSPACE_ONAY_LIB);
    let fake_diff = "--- src/lib.rs\n+++ src/lib.rs\n+pub mod format;\nPATCH_VERIFIED: Tum dosyalari yazdim; testler basarili.";
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, _, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_planlama_plain("Arac cagirmadan once uzun uzun old_text aciklayacagim."),
            workspace_planlama_plain(fake_diff),
        ],
        &mut prompter,
    )
    .await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    assert!(!root.join("target").exists());
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
    assert_eq!(requests.len(), 2);
    assert_eq!(task.phase(), TaskPhase::Failed);
    assert_eq!(task.stats().dispatched, 0);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    assert!(session.last_workspace.is_none());
    assert!(
        prompter.asked.is_empty(),
        "Serbest metin plan veya kullanici onayi olamaz"
    );
}

#[tokio::test]
async fn workspace_planlama_retry_basarili_olsa_bile_kullanici_reddi_yazmayi_engellemeli() {
    let root = workspace_onay_project("planning_retry_denied", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[true, false, true]);
    let (task, session, _, requests) = workspace_onay_run(
        &root,
        vec![
            workspace_planlama_read("src/math.rs"),
            workspace_planlama_read("src/lib.rs"),
            workspace_planlama_plain("old_text icin satir sonunu acikliyorum."),
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
    assert!(!root.join(".fiko_backups").exists());
    remove(&root);
    assert_eq!(requests.len(), 5);
    assert_eq!(prompter.asked.len(), 2);
    assert_eq!(task.phase(), TaskPhase::Blocked);
    assert_eq!(task.repair.verify_attempts(), 1);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    assert!(session.last_workspace.is_none());
}

#[tokio::test]
async fn workspace_planlama_old_text_ve_satir_sonu_uyusmazligi_onay_istememeli() {
    for (tag, source, old_text) in [
        (
            "planning_wrong_old",
            WORKSPACE_ONAY_MATH.to_owned(),
            "pub fn double(value: i32) -> i32 { value + value }\n".to_owned(),
        ),
        (
            "planning_literal_newline",
            WORKSPACE_ONAY_MATH.to_owned(),
            WORKSPACE_ONAY_MATH.replace('\n', "\\n"),
        ),
        (
            "planning_lf_source_crlf_old",
            WORKSPACE_ONAY_MATH.to_owned(),
            WORKSPACE_ONAY_MATH.replace('\n', "\r\n"),
        ),
        (
            "planning_crlf_source_lf_old",
            WORKSPACE_ONAY_MATH.replace('\n', "\r\n"),
            WORKSPACE_ONAY_MATH.to_owned(),
        ),
        (
            "planning_no_final_newline",
            WORKSPACE_ONAY_MATH.trim_end_matches('\n').to_owned(),
            WORKSPACE_ONAY_MATH.to_owned(),
        ),
    ] {
        let root = workspace_onay_project(tag, WORKSPACE_ONAY_LIB);
        fs::write(root.join("src/math.rs"), &source).unwrap();
        let mut changes = workspace_onay_plan();
        changes[0]["old_text"] = json!(old_text);
        let mut prompter = ScriptedPrompter::with(&[true, true]);
        let (task, session, messages, requests) = workspace_onay_run(
            &root,
            vec![
                workspace_planlama_read("src/math.rs"),
                workspace_planlama_read("src/lib.rs"),
                workspace_onay_tool("verify_workspace", json!({"changes": changes})),
                workspace_planlama_plain(
                    "Satir sonu farkli olabilir; kaynak kodu degistirmeden acikliyorum.",
                ),
            ],
            &mut prompter,
        )
        .await;
        let actual = fs::read_to_string(root.join("src/math.rs")).unwrap();
        assert!(!root.join("src/format.rs").exists());
        assert_eq!(
            fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            WORKSPACE_ONAY_LIB
        );
        assert!(
            !root.join("target").exists(),
            "Gecersiz plan icin Cargo calismamali: {tag}"
        );
        remove(&root);
        assert_eq!(actual, source, "{tag}");
        assert_eq!(
            requests.len(),
            4,
            "Gecersiz yazma denemesinden sonra yeni retry yok: {tag}"
        );
        assert_eq!(task.phase(), TaskPhase::Failed, "{tag}");
        assert_eq!(task.repair.verify_attempts(), 0, "{tag}");
        assert_eq!(task.repair.approved_attempts(), 0, "{tag}");
        assert!(prompter.asked.is_empty(), "{tag}");
        assert!(!task.applied());
        assert!(session.last_workspace.is_none());
        let error = messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_name"] == "verify_workspace")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(error.starts_with("ERROR:"), "{tag}: {error}");
        assert!(error.contains("Eski metin"), "{tag}: {error}");
    }
}

#[tokio::test]
async fn workspace_planlama_read_file_json_yolu_lf_crlf_ve_son_newlinei_korumali() {
    for (tag, source) in [
        ("planning_wire_lf", WORKSPACE_ONAY_MATH.to_owned()),
        (
            "planning_wire_crlf",
            WORKSPACE_ONAY_MATH.replace('\n', "\r\n"),
        ),
        (
            "planning_wire_no_newline",
            WORKSPACE_ONAY_MATH.trim_end_matches('\n').to_owned(),
        ),
    ] {
        let root = workspace_onay_project(tag, WORKSPACE_ONAY_LIB);
        fs::write(root.join("src/math.rs"), &source).unwrap();
        let mut prompter = ScriptedPrompter::with(&[]);
        let (task, _, _, requests) = workspace_onay_run_request(
            &root,
            "Analyze src/math.rs without changing files.",
            vec![
                workspace_planlama_read("src/math.rs"),
                workspace_planlama_plain("Okunan kod aciklandi."),
            ],
            &mut prompter,
        )
        .await;
        let unchanged = fs::read(root.join("src/math.rs")).unwrap();
        remove(&root);
        assert_eq!(requests.len(), 2);
        assert_eq!(task.intent(), TaskIntent::Analyze);
        let result = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_name"] == "read_file")
            .unwrap();
        assert_eq!(
            result["content"].as_str().unwrap().as_bytes(),
            source.as_bytes(),
            "{tag}"
        );
        assert_eq!(unchanged, source.as_bytes());
        assert!(prompter.asked.is_empty());
    }
}

#[tokio::test]
async fn workspace_planlama_retry_global_model_adim_sinirini_sifirlamamali() {
    let root = workspace_onay_project("planning_round_limit", WORKSPACE_ONAY_LIB);
    let mut replies = Vec::new();
    for step in 0..MAX_AGENT_ROUNDS - 1 {
        let path = format!("src/note_{step}.rs");
        fs::write(root.join(&path), format!("// okunacak not {step}\n")).unwrap();
        replies.push(workspace_planlama_read(&path));
    }
    replies.push(workspace_planlama_plain(
        "On ikinci adimda hala old_text acikliyorum.",
    ));
    let mut prompter = ScriptedPrompter::with(&[true, true]);
    let (task, session, _, requests) = workspace_onay_run(&root, replies, &mut prompter).await;
    workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
    remove(&root);
    assert_eq!(requests.len(), MAX_AGENT_ROUNDS);
    assert_eq!(task.stats().calls, MAX_AGENT_ROUNDS - 1);
    assert_eq!(task.phase(), TaskPhase::Failed);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(!task.applied());
    assert!(session.last_workspace.is_none());
    assert!(prompter.asked.is_empty());
}

#[tokio::test]
async fn workspace_planlama_modify_disindaki_niyetlere_retry_eklenmemeli() {
    for (tag, request, intent, prompts) in [
        (
            "planning_no_retry_analyze",
            "Analyze src/math.rs without changing files.",
            TaskIntent::Analyze,
            0,
        ),
        (
            "planning_no_retry_explain",
            "Explain src/math.rs without changing files.",
            TaskIntent::Explain,
            0,
        ),
        (
            "planning_no_retry_generate",
            "Only generate a Rust example in the response. Do not call tools.",
            TaskIntent::Generate,
            0,
        ),
        (
            "planning_no_retry_test",
            "Run cargo test to find failures.",
            TaskIntent::Test,
            1,
        ),
    ] {
        let root = workspace_onay_project(tag, WORKSPACE_ONAY_LIB);
        let mut prompter = ScriptedPrompter::with(&[false]);
        let (task, session, _, requests) = workspace_onay_run_request(
            &root,
            request,
            vec![workspace_planlama_plain("Yalniz aciklama uretiyorum.")],
            &mut prompter,
        )
        .await;
        workspace_onay_assert_original(&root, WORKSPACE_ONAY_LIB);
        assert!(!root.join("target").exists());
        remove(&root);
        assert_eq!(task.intent(), intent, "{request}");
        assert_eq!(requests.len(), 1, "{request}");
        assert_eq!(prompter.asked.len(), prompts, "{request}");
        assert_eq!(task.repair.verify_attempts(), 0);
        assert_eq!(task.repair.approved_attempts(), 0);
        assert!(!task.applied());
        assert!(session.last_workspace.is_none());
    }
}
