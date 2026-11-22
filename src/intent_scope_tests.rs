// agent::tests icinde include edilir; gercek agent/izin/plan yolu mock HTTP ile sinanir.

const INTENT_SCOPE_LIVE_REQUEST: &str =
    "Modify src/math.rs, preserve src/lib.rs without modifying it.";

fn intent_scope_math_plan() -> Value {
    json!([{
        "operation":"edit", "path":"src/math.rs",
        "old_text":WORKSPACE_ONAY_MATH, "new_text":WORKSPACE_ONAY_MATH_UPDATED,
    }])
}

fn intent_scope_last_tool<'a>(messages: &'a [Value], name: &str) -> &'a str {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool" && message["tool_name"] == name)
        .and_then(|message| message["content"].as_str())
        .expect("Beklenen gercek arac yaniti bulunamadi.")
}

fn intent_scope_assert_untouched(root: &Path) {
    workspace_onay_assert_original(root, WORKSPACE_ONAY_LIB);
    assert_eq!(
        fs::read(root.join("src/lib.rs")).unwrap(),
        WORKSPACE_ONAY_LIB.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        MINI_MAIN
    );
    assert!(!root.join(".fiko_backups").exists());
    assert!(!root.join("target").exists());
}

#[tokio::test]
async fn intent_scope_canli_virgullu_istek_korunan_dosyayi_okuyup_yalniz_mathi_onayla_uygulamali() {
    assert_eq!(
        crate::intent::classify(INTENT_SCOPE_LIVE_REQUEST).intent,
        TaskIntent::Modify
    );
    for approved in [true, false] {
        let root = workspace_onay_project(
            if approved {
                "scope_math_approved"
            } else {
                "scope_math_rejected"
            },
            WORKSPACE_ONAY_LIB,
        );
        let mut prompter = ScriptedPrompter::with(&[true, approved]);
        let (task, session, messages, requests) = workspace_onay_run_request(
            &root,
            INTENT_SCOPE_LIVE_REQUEST,
            vec![
                workspace_onay_tool("read_file", json!({"path":"src/lib.rs"})),
                workspace_onay_tool("verify_workspace", json!({"changes":intent_scope_math_plan(),"mode":"check"})),
                json!({"message":{"role":"assistant","content":"src/math.rs diff'i hazir. Uygulama icin ONAYLI yazin."}}),
            ],
            &mut prompter,
        ).await;
        assert_eq!(task.intent(), TaskIntent::Modify);
        assert_eq!(
            requests.len(),
            3,
            "Onay gecisi ek model kararina bagli olmamali."
        );
        assert_eq!(
            intent_scope_last_tool(requests[1]["messages"].as_array().unwrap(), "read_file"),
            WORKSPACE_ONAY_LIB
        );
        assert!(
            intent_scope_last_tool(
                requests[2]["messages"].as_array().unwrap(),
                "verify_workspace"
            )
            .starts_with("VERIFY_OK")
        );
        assert_eq!(
            prompter.asked.len(),
            2,
            "Gecici Cargo izni ve tek Rust uygulama onayi gerekir."
        );
        assert!(prompter.asked[0].contains("cargo check"));
        assert!(prompter.asked[1].contains("Tum workspace"));
        assert_eq!(task.stats().calls, 3);
        assert_eq!(task.stats().dispatched, 3);
        assert_eq!(task.repair.verify_attempts(), 1);
        assert_eq!(
            task.plan()
                .unwrap()
                .targets
                .iter()
                .map(|target| target.path.as_str())
                .collect::<Vec<_>>(),
            ["src/math.rs"]
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .count(),
            2
        );
        assert!(
            !messages
                .iter()
                .any(|message| message["role"] == "tool"
                    && message["tool_name"] == "apply_workspace"),
            "Host onayi icin model arac cagrisi uydurulmamali."
        );
        assert_eq!(
            fs::read(root.join("src/lib.rs")).unwrap(),
            WORKSPACE_ONAY_LIB.as_bytes(),
            "Korunan dosya byte duzeyinde ayni kalmali."
        );
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            MINI_MAIN
        );
        assert!(!root.join("src/format.rs").exists());
        let finish = task.memory_finish();
        if approved {
            assert_eq!(task.phase(), TaskPhase::Completed);
            assert!(task.applied());
            assert_eq!(task.repair.approved_attempts(), 1);
            assert_eq!(
                fs::read_to_string(root.join("src/math.rs")).unwrap(),
                WORKSPACE_ONAY_MATH_UPDATED
            );
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
            let applied = session.last_workspace.as_ref().unwrap();
            assert_eq!(applied.files.len(), 1);
            assert_eq!(applied.files[0].file.relative, "src/math.rs");
        } else {
            assert!(!task.applied());
            assert_eq!(task.phase(), TaskPhase::Blocked);
            assert_eq!(task.repair.approved_attempts(), 0);
            assert_eq!(finish.phase, "Rejected");
            assert_eq!(finish.modified_paths, Some(vec![]));
            assert_eq!(finish.outcome, "not_applied");
            assert_eq!(
                finish.commands.len(),
                1,
                "Yalniz gecici cargo check calismis olmali."
            );
            assert_eq!(finish.commands[0].command, "cargo check");
            assert_eq!(finish.commands[0].exit_code, Some(0));
            assert_eq!(
                fs::read_to_string(root.join("src/math.rs")).unwrap(),
                WORKSPACE_ONAY_MATH
            );
            assert!(session.last_workspace.is_none());
            assert!(!root.join(".fiko_backups").exists());
            assert!(root.join("target/fiko_workspace_verify").is_dir());
            assert!(!root.join("target/fiko_workspace_applied").exists());
        }
        assert!(session.last_patch.is_none());
        remove(&root);
    }
}

#[tokio::test]
async fn intent_scope_modelin_korunan_dosyayi_verify_ve_apply_ile_eklemesi_reddedilmeli() {
    let lib_edit = json!({"operation":"edit","path":"src/lib.rs","old_text":WORKSPACE_ONAY_LIB,"new_text":"pub mod math;\n// Yetkisiz model degisikligi.\n"});
    let mut both = intent_scope_math_plan().as_array().unwrap().clone();
    both.push(lib_edit.clone());
    let attempts = [
        ("verify_workspace", json!({"changes":both,"mode":"check"})),
        ("apply_workspace", json!({"changes":[lib_edit]})),
        (
            "verify_candidate",
            json!({"path":"src/lib.rs","old_text":WORKSPACE_ONAY_LIB,"new_text":"pub mod math;\n// Yetkisiz.\n","mode":"check"}),
        ),
        (
            "apply_patch",
            json!({"path":"./src/./lib.rs","old_text":WORKSPACE_ONAY_LIB,"new_text":"pub mod math;\n// Yetkisiz.\n"}),
        ),
    ];
    for (index, (name, args)) in attempts.into_iter().enumerate() {
        let root = workspace_onay_project(&format!("scope_protected_{index}"), WORKSPACE_ONAY_LIB);
        let mut prompter = ScriptedPrompter::with(&[]);
        let (task, session, messages, requests) = workspace_onay_run_request(
            &root,
            INTENT_SCOPE_LIVE_REQUEST,
            vec![
                workspace_onay_tool(name, args),
                json!({"message":{"role":"assistant","content":"PATCH_VERIFIED: Iki dosya da guncellendi ve testler gecti."}}),
            ],
            &mut prompter,
        ).await;
        assert_eq!(requests.len(), 2);
        let rejected = intent_scope_last_tool(&messages, name);
        assert!(
            crate::orchestrator::is_failure(rejected),
            "Korunan hedef reddedilmeli: {rejected}"
        );
        assert_eq!(task.intent(), TaskIntent::Modify);
        assert_eq!(task.stats().calls, 1);
        assert_eq!(
            task.stats().dispatched,
            0,
            "Kapsam denetimi handler/Cargo/onaydan once olmali: {name}"
        );
        assert_eq!(task.repair.verify_attempts(), 0);
        assert_eq!(task.repair.approved_attempts(), 0);
        assert!(
            prompter.asked.is_empty(),
            "Korunan dosya icin onay sorulmamali: {:?}",
            prompter.asked
        );
        assert!(!task.applied());
        assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
        assert!(task.memory_finish().commands.is_empty());
        assert!(session.last_patch.is_none());
        assert!(session.last_workspace.is_none());
        intent_scope_assert_untouched(&root);
        remove(&root);
    }
}

#[tokio::test]
async fn intent_scope_global_yazma_yasagi_math_modify_istegini_yetkilendirmemeli() {
    let request = "Modify src/math.rs, but do not modify any files.";
    assert!(crate::intent::classify(request).write_forbidden);
    let root = workspace_onay_project("scope_global_prohibition", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[]);
    let (task, session, messages, requests) = workspace_onay_run_request(
        &root,
        request,
        vec![
            workspace_onay_tool("verify_workspace", json!({"changes":intent_scope_math_plan(),"mode":"check"})),
            json!({"message":{"role":"assistant","content":"Model dosyayi degistirdigini soyluyor."}}),
        ],
        &mut prompter,
    ).await;
    assert_eq!(requests.len(), 2);
    assert!(crate::orchestrator::is_failure(intent_scope_last_tool(
        &messages,
        "verify_workspace"
    )));
    assert_eq!(task.authority(), crate::intent::WriteAuthority::Denied);
    assert_eq!(task.stats().calls, 1);
    assert_eq!(task.stats().dispatched, 0);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(
        prompter.asked.is_empty(),
        "Acik global yasak yeni izin sorusuna donusmemeli."
    );
    assert!(!task.applied());
    assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
    assert!(task.memory_finish().commands.is_empty());
    assert!(session.last_patch.is_none());
    assert!(session.last_workspace.is_none());
    intent_scope_assert_untouched(&root);
    remove(&root);
}

#[tokio::test]
async fn intent_scope_belirsiz_preserve_it_zamiri_otomatik_yazma_ve_onay_uretmemeli() {
    let request = "Modify src/math.rs and src/lib.rs, but preserve it without modifying it.";
    let root = workspace_onay_project("scope_ambiguous_preserve", WORKSPACE_ONAY_LIB);
    let mut prompter = ScriptedPrompter::with(&[]);
    let (task, session, messages, requests) = workspace_onay_run_request(
        &root,
        request,
        vec![
            workspace_onay_tool("verify_workspace", json!({"changes":intent_scope_math_plan(),"mode":"check"})),
            json!({"message":{"role":"assistant","content":"Korunacak dosyanin acikca belirtilmesi gerekiyor; degisiklik uygulanmadi."}}),
        ],
        &mut prompter,
    ).await;
    assert_eq!(requests.len(), 2);
    assert!(crate::orchestrator::is_failure(intent_scope_last_tool(
        &messages,
        "verify_workspace"
    )));
    assert_ne!(task.authority(), crate::intent::WriteAuthority::Granted);
    assert_eq!(task.stats().calls, 1);
    assert_eq!(task.stats().dispatched, 0);
    assert_eq!(task.repair.verify_attempts(), 0);
    assert_eq!(task.repair.approved_attempts(), 0);
    assert!(
        prompter.asked.is_empty(),
        "Cozulmemis koruma kapsami basit onayla asilamamali."
    );
    assert!(!task.applied());
    assert_eq!(task.memory_finish().modified_paths, Some(vec![]));
    assert!(task.memory_finish().commands.is_empty());
    assert!(session.last_patch.is_none());
    assert!(session.last_workspace.is_none());
    intent_scope_assert_untouched(&root);
    remove(&root);
}
