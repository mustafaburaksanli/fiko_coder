//! Cok dosyali adaylar icin mevcut izin, kilit ve onay akisinin genisletilmesi.
use std::{fs, io};

use serde_json::Value;

use crate::agent::{PatchEffect, Prompter, Session, ToolOutcome};
use crate::execution::TaskPhase;
use crate::repair_controller::{RepairController, VerifyMode};
use crate::verification::{self, VerificationKind};
use crate::workspace_patch::{self, FileChange, WorkspacePlan};

/// JSON yalnizca aday tanimidir; dosya ve proje durumu prepare ile dogrulanir.
pub(crate) fn parse_changes(args: &Value) -> Result<Vec<FileChange>, String> {
    let items = args["changes"]
        .as_array()
        .ok_or("changes bir JSON dizisi olmali.")?;
    if items.is_empty() || items.len() > 16 {
        return Err("changes 1-16 dosya icermeli.".into());
    }
    let mut changes = Vec::new();
    let mut bytes = 0usize;
    for item in items {
        let object = item
            .as_object()
            .ok_or("Her degisiklik bir JSON nesnesi olmali.")?;
        let field = |name: &str| -> Result<String, String> {
            let text = item[name]
                .as_str()
                .ok_or_else(|| format!("{name} metin olmali."))?;
            if text.len() > 128_000 {
                return Err(format!("{name} 128 KB sinirini asiyor."));
            }
            Ok(text.to_owned())
        };
        let path = field("path")?;
        if path.trim().is_empty() || path.len() > 1024 {
            return Err("Hedef yol bos veya cok uzun.".into());
        }
        let (change, fields) = match item["operation"].as_str() {
            Some("edit") => {
                let old_text = field("old_text")?;
                let new_text = field("new_text")?;
                bytes += old_text.len() + new_text.len();
                (
                    FileChange::Edit {
                        path,
                        old_text,
                        new_text,
                    },
                    &[
                        "operation",
                        "path",
                        "old_text",
                        "new_text",
                        "match_mode",
                        "expected_sha256",
                    ][..],
                )
            }
            Some("create") => {
                let content = field("content")?;
                bytes += content.len();
                (
                    FileChange::Create { path, content },
                    &["operation", "path", "content"][..],
                )
            }
            _ => {
                return Err(
                    "operation yalniz edit veya create olabilir; silme/tasima desteklenmiyor."
                        .into(),
                );
            }
        };
        if object.keys().any(|key| !fields.contains(&key.as_str())) || bytes > 512_000 {
            return Err("Bilinmeyen degisiklik alani veya toplam 512 KB siniri asildi.".into());
        }
        changes.push(change);
    }
    Ok(changes)
}

fn prepare(session: &Session, args: &Value) -> Result<WorkspacePlan, String> {
    if verification::verification_kind(&session.root)? != VerificationKind::Cargo {
        return Err(
            "Dogrulama calistirilmadi: proje turu desteklenmiyor; workspace uygulanmadi.".into(),
        );
    }
    let changes = parse_changes(args)?;
    let options = args["changes"]
        .as_array()
        .ok_or("changes bir JSON dizisi olmali.")?
        .iter()
        .enumerate()
        .map(|(index, item)| {
            crate::tools::PatchMatchOptions::from_arguments(item)
                .map_err(|error| workspace_patch::error_for_change(error, index))
        })
        .collect::<Result<Vec<_>, _>>()?;
    workspace_patch::prepare_with_options(&session.root, &changes, &options)
}

pub(crate) async fn verify<P: Prompter>(
    session: &mut Session,
    repair: &mut RepairController,
    prompter: &mut P,
    args: &Value,
) -> io::Result<ToolOutcome> {
    repair.clear_pending_workspace();
    let plan = match prepare(session, args) {
        Ok(plan) => plan,
        Err(error) => return Ok(ToolOutcome::preparation_error(error)),
    };
    let Some(mode) = VerifyMode::parse(args["mode"].as_str().unwrap_or("check")) else {
        return Ok(ToolOutcome::output(
            "ERROR: Gecersiz workspace dogrulama modu.",
        ));
    };
    verify_plan(
        session,
        repair,
        prompter,
        &plan,
        mode,
        Some(&args["changes"]),
    )
    .await
}

async fn verify_plan<P: Prompter>(
    session: &mut Session,
    repair: &mut RepairController,
    prompter: &mut P,
    plan: &WorkspacePlan,
    mode: VerifyMode,
    pending_changes: Option<&Value>,
) -> io::Result<ToolOutcome> {
    if !session.consent.allowed(mode, prompter)? {
        repair.enter(TaskPhase::Blocked);
        return Ok(ToolOutcome::stop(
            "VERIFY_REJECTED: Gecici kopyada kod calistirma izni reddedildi; kaynaklara yazilmadi.",
            "Workspace dogrulama izni yok.",
        )
        .rejected());
    }
    if let Err(error) = repair.register_workspace_candidate(plan, mode) {
        return Ok(ToolOutcome::output(format!("ERROR: {error}")));
    }
    repair.enter(TaskPhase::Verifying);
    match verification::verify_workspace(&session.root, plan, mode).await {
        Ok(report) => {
            let success = report.success();
            if success {
                repair.record_workspace_success(plan, report.project_fingerprint, mode);
                if let Some(changes) = pending_changes {
                    repair.record_pending_workspace(plan, changes);
                }
            } else {
                repair.record_workspace_failure(plan);
            }
            repair.enter(TaskPhase::Planning);
            let status = if success {
                "VERIFY_OK"
            } else {
                "VERIFY_FAILED"
            };
            let outcome = ToolOutcome::output(format!(
                "{status}: Workspace gecici kopyasi; ana proje degismedi.\n{}",
                report.render()
            ))
            .with_processes(
                report
                    .results
                    .iter()
                    .map(|result| result.observation())
                    .collect(),
            );
            Ok(if success {
                outcome.with_verified_candidate()
            } else {
                outcome
            })
        }
        Err(error) => {
            repair.record_workspace_failure(plan);
            repair.enter(TaskPhase::Blocked);
            Ok(ToolOutcome::stop(
                format!("ERROR: Workspace dogrulamasi tamamlanamadi: {error}"),
                "Dogrulama tamamlanamadi; kaynak projenin durumu teyit edilemedi.",
            )
            .with_patch_effect(PatchEffect::Uncertain))
        }
    }
}

fn stop(repair: &mut RepairController, message: String, effect: PatchEffect) -> ToolOutcome {
    repair.enter(TaskPhase::Blocked);
    ToolOutcome::stop(
        format!("APPLY_BLOCKED: {message}"),
        "Workspace uygulanmadi.",
    )
    .with_patch_effect(effect)
}

pub(crate) async fn apply<P: Prompter>(
    session: &mut Session,
    repair: &mut RepairController,
    prompter: &mut P,
    args: &Value,
) -> io::Result<ToolOutcome> {
    // Bekleyen dogrulanmis aday varken model veya host baska icerige gecemez.
    // Kayit tek kullanimlidir; ret/gecersiz giris tekrar onay uretemez.
    let pending = repair.take_pending_workspace();
    if let Some(pending) = &pending
        && let Err(error) = workspace_patch::revalidate(&session.root, &pending.plan)
    {
        return Ok(stop(
            repair,
            format!("Dogrulanmis workspace adayi eskidi: {error}. Yeni dogrulama gerekir."),
            PatchEffect::Uncertain,
        ));
    }
    let plan = match prepare(session, args) {
        Ok(plan) => plan,
        Err(error) => return Ok(ToolOutcome::preparation_error(error)),
    };
    if pending.as_ref().is_some_and(|pending| pending.plan != plan) {
        return Ok(stop(repair, "Uygulanacak plan Rust'in dogruladigi bekleyen adaydan farkli. Yeni verify_workspace gerekir.".into(), PatchEffect::None));
    }
    let fingerprint = match verification::project_fingerprint(&session.root) {
        Ok(value) => value,
        Err(error) => return Ok(stop(repair, error, PatchEffect::None)),
    };
    let mut auto_verify_report = None;
    let mut processes = Vec::new();
    if let Err(rejection) = repair.authorize_workspace(&plan, fingerprint) {
        if pending.is_some() {
            return Ok(stop(
                repair,
                format!(
                    "Bekleyen dogrulama gecersiz: {rejection}. Yeni dogrulama ve onay gerekir."
                ),
                PatchEffect::Uncertain,
            ));
        }
        if !rejection.needs_verification() {
            return Ok(stop(repair, rejection.to_string(), PatchEffect::None));
        }
        let mode = repair.required_mode();
        let outcome = verify_plan(session, repair, prompter, &plan, mode, None).await?;
        if outcome.stop.is_some() {
            return Ok(outcome);
        }
        processes.extend(outcome.processes);
        auto_verify_report = Some(outcome.output);
    }
    // Gecici calistirma sirasindaki kaynak degisikligi de eski kilidi acamaz.
    let current_fingerprint = match workspace_patch::revalidate(&session.root, &plan)
        .and_then(|()| verification::project_fingerprint(&session.root))
    {
        Ok(value) => value,
        Err(error) => {
            return Ok(stop(
                repair,
                format!("{error}\n{}", auto_verify_report.as_deref().unwrap_or("")),
                PatchEffect::Uncertain,
            )
            .with_processes(processes));
        }
    };
    if let Err(error) = repair.authorize_workspace(&plan, current_fingerprint) {
        return Ok(stop(
            repair,
            format!("{error}\n{}", auto_verify_report.as_deref().unwrap_or("")),
            PatchEffect::None,
        )
        .with_processes(processes));
    }
    repair.enter(TaskPhase::AwaitingApproval);
    println!(
        "\n========== WORKSPACE DIFF ==========\n{}",
        plan.diff_summary()
    );
    println!(
        "Tek onay: tum hedefler yedeklenecek, uygulanacak, gercek projede cargo check + cargo test calisacak. Hatada tum plan geri alinmaya calisilacak; crash-safe atomiklik yoktur."
    );
    if !prompter.ask_yes("Tum workspace degisiklikleri uygulansin ve dogrulansin mi?")? {
        return Ok(stop(
            repair,
            "Kullanici tum workspace planini reddetti; dosyalar degismedi.".into(),
            PatchEffect::None,
        )
        .rejected()
        .with_processes(processes));
    }
    let after_approval = workspace_patch::revalidate(&session.root, &plan)
        .and_then(|()| verification::project_fingerprint(&session.root))
        .and_then(|fingerprint| {
            repair
                .authorize_workspace(&plan, fingerprint)
                .map_err(|error| error.to_string())
        });
    if let Err(error) = after_approval {
        return Ok(stop(
            repair,
            format!("Onay sirasinda aday/proje eskidi: {error}. Yeni dogrulama ve onay gerekir."),
            PatchEffect::Uncertain,
        )
        .with_processes(processes));
    }
    repair.approve();
    repair.enter(TaskPhase::Applying);
    let record = match workspace_patch::apply(&session.root, &plan) {
        Ok(record) => record,
        Err(error) => {
            repair.mark_workspace_failed(&plan);
            repair.enter(TaskPhase::Failed);
            let restored = error.restored
                && verification::project_fingerprint(&session.root).ok()
                    == Some(current_fingerprint);
            return Ok(ToolOutcome::stop(
                format!(
                    "WORKSPACE_APPLY_FAILED: {}\nTum onceki proje durumu teyit edildi: {restored}",
                    error.message
                ),
                "Workspace uygulamasi basarisiz.",
            )
            .with_patch_effect(
                if restored
                    && error
                        .record
                        .as_ref()
                        .is_some_and(|record| record.has_writes())
                {
                    PatchEffect::Restored
                } else if restored {
                    PatchEffect::None
                } else {
                    PatchEffect::Uncertain
                },
            )
            .with_processes(processes));
        }
    };
    let verification = verification::verify_workspace_applied(&session.root).await;
    let modified_paths = record.modified_paths();
    let (passed, evidence) = match verification {
        Ok(results) => {
            processes.extend(results.iter().map(|result| result.observation()));
            let success = results.len() == 2
                && results
                    .iter()
                    .all(|result| result.status == crate::tools::ExecutionStatus::Succeeded);
            (
                success,
                results
                    .iter()
                    .map(|result| result.report.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        }
        Err(error) => (false, format!("Dogrulama tamamlanamadi: {error}")),
    };
    let targets_unchanged = plan.files.iter().all(|file| {
        crate::discovery::resolve_existing(&session.root, &file.relative)
            .ok()
            .as_ref()
            == Some(&file.path)
            && fs::metadata(&file.path).is_ok_and(|metadata| metadata.len() <= 128_000)
            && fs::read_to_string(&file.path).ok().as_ref() == Some(&file.updated)
    });
    if passed && targets_unchanged {
        session.last_patch = None;
        session.last_workspace = Some(record);
        repair.enter(TaskPhase::Completed);
        return Ok(ToolOutcome::stop(format!("PATCH_VERIFIED: Tum workspace hedefleri uygulandi; cargo check + cargo test gecti.\n{evidence}"), "Workspace dogrulandi.").with_patch_effect(PatchEffect::Verified).with_modified_paths(modified_paths).with_processes(processes));
    }
    repair.mark_workspace_failed(&plan);
    repair.enter(TaskPhase::Failed);
    let rollback = workspace_patch::rollback(&session.root, &record);
    let restored = rollback.is_ok()
        && verification::project_fingerprint(&session.root).ok() == Some(current_fingerprint);
    let detail = match rollback {
        Ok(()) if restored => "Tum plan geri alindi ve onceki proje fingerprint'i teyit edildi.".into(),
        Ok(()) => "Plan hedefleri geri alindi; proje fingerprint'i degisik. Projenin tam geri dondugu iddia edilemez.".into(),
        Err(error) => format!("KRITIK: Rollback tamamlanamadi: {error}. Yedekleri kontrol edin; proje durumu belirsiz."),
    };
    Ok(ToolOutcome::stop(
        format!(
            "WORKSPACE_FAILED: Son dogrulama veya hedef kontrolu basarisiz.\n{evidence}\n{detail}"
        ),
        "Workspace basarisiz; basari raporlanamaz.",
    )
    .with_patch_effect(if restored {
        PatchEffect::Restored
    } else {
        PatchEffect::Uncertain
    })
    .with_modified_paths(modified_paths)
    .with_processes(processes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fiko_workspace_options_{}_{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(
                path.join("Cargo.toml"),
                "[package]\nname='fixture'\nversion='0.1.0'\n",
            )
            .unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            assert!(
                self.0
                    .starts_with(std::env::temp_dir().canonicalize().unwrap())
            );
            assert!(
                self.0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("fiko_workspace_options_")
            );
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn workspace_raw_edit_metadata_hazirlamaya_tasinmali_create_strict_kalmali() {
        let project = Project::new();
        let original = "fn value() -> u8 {\r\n    1\n}\r\n";
        let replacement = "fn value() -> u8 {\n    3\r\n}\n";
        fs::write(project.0.join("src/lib.rs"), original).unwrap();
        let session = Session::new(project.0.clone());
        let mut args = serde_json::json!({"changes":[
            {"operation":"edit","path":"src/lib.rs","old_text":"fn value() -> u8 {\n    1\n}\n","new_text":replacement,
                "match_mode":"line_endings","expected_sha256":format!("{:x}",sha2::Sha256::digest(original.as_bytes()))},
            {"operation":"create","path":"src/new.rs","content":"pub fn new() {}\r\n"}
        ]});
        let changes = parse_changes(&args).unwrap();
        assert_eq!(
            changes[0],
            FileChange::Edit {
                path: "src/lib.rs".into(),
                old_text: "fn value() -> u8 {\n    1\n}\n".into(),
                new_text: replacement.into()
            }
        );
        let plan = prepare(&session, &args).unwrap();
        assert_eq!(plan.files[0].original.as_deref(), Some(original));
        assert_eq!(plan.files[0].updated, replacement);
        assert_eq!(plan.files[1].updated, "pub fn new() {}\r\n");
        args["changes"][0]
            .as_object_mut()
            .unwrap()
            .remove("expected_sha256");
        assert!(prepare(&session, &args).is_err());
        args["changes"][1]["match_mode"] = serde_json::json!("line_endings");
        assert!(parse_changes(&args).is_err());
        assert_eq!(
            fs::read(project.0.join("src/lib.rs")).unwrap(),
            original.as_bytes()
        );
        assert!(!project.0.join("src/new.rs").exists());
        assert!(!project.0.join(".fiko_backups").exists());
    }
}
