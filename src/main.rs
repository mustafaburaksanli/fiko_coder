//! FIKO CODER v1.2.1 - POSTGRESQL PERSISTENT MEMORY ENGINE
//!
//! Moduller:
//! - tools: dosya araclari, cargo, patch hazirlama, yedekleme / geri alma
//! - verification: adaylarin gecici kopyada derlenmesi (verify_candidate), proje parmak izi
//! - repair_controller: deneme sayaclari, tekrar engeli ve dogrulanmis patch kilidi
//! - intent / execution / orchestrator: gorev niyeti, asama makinesi, arac denetimi
//! - agent: Ollama dongusu, arac secimi, kullanici onayi

use std::io::{self, Write};

use serde_json::{Value, json};

use fiko_coder::agent::{self, Prompter, Session, TerminalPrompter};
use fiko_coder::tools;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "qwen-code-local:9b".into());
    if model == "--memory-migrate" || model == "--memory-check" {
        let result = if model == "--memory-migrate" {
            fiko_coder::memory::migrate_configured().await
        } else {
            fiko_coder::memory::check_configured().await
        };
        match result {
            Ok(()) => println!(
                "[HAFIZA] PostgreSQL {} basarili.",
                if model == "--memory-migrate" {
                    "migration"
                } else {
                    "baglanti/sema kontrolu"
                }
            ),
            Err(error) => {
                eprintln!("[HAFIZA] {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    let root = std::env::current_dir()?.canonicalize()?;
    if model == "--recovery-audit" {
        match fiko_coder::workspace_recovery::audit(&root) {
            Ok(report) => println!("{report}"),
            Err(error) => {
                eprintln!("[RECOVERY] {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    let client = reqwest::Client::new();
    let mut session = Session::new(root.clone());
    match fiko_coder::memory::MemoryConfig::from_env() {
        None => println!(
            "[HAFIZA] FIKO_DATABASE_URL ayarli degil; kalici hafiza devre disi. Mevcut gorev akisi kullanilacak."
        ),
        Some(config) => match session.enable_memory(config).await {
            Ok(()) => println!("[HAFIZA] PostgreSQL baglantisi ve proje kimligi hazir."),
            Err(error) => println!(
                "[HAFIZA] {error}. Kalici hafiza kullanilamiyor; mevcut gorev akisi devam ediyor."
            ),
        },
    }
    let mut prompter = TerminalPrompter;

    let system: Value = json!({ "role": "system", "content": agent::system_prompt() });
    let mut messages: Vec<Value> = vec![system.clone()];

    println!("=================================");
    println!("  FIKO CODER v1.2.1 - POSTGRESQL PERSISTENT MEMORY ENGINE");
    println!("=================================");
    println!("Model: {model}");
    println!("Proje: {}", root.display());
    println!(
        "Araclar: read_file, list_files, read_lines, search_code, search_in_file, run_tests, verify_candidate, apply_patch, verify_workspace, apply_workspace"
    );
    println!(
        "Niyet: gorev turu (Generate/Analyze/Explain/Modify/Test) Rust tarafinda belirlenir; Generate yalnizca yanitta kod uretir, arac calistirmaz. Yalnizca Modify dosya degisikligine yetki verir; belirsizse sorulur."
    );
    println!(
        "Akis: verify_candidate (gecici kopya) -> kullanici onayi -> apply_patch -> cargo check + cargo test"
    );
    println!(
        "Kilit: yalnizca dogrulanmis ve birebir ayni aday uygulanir; dosya/proje degisirse dogrulama eskir."
    );
    println!(
        "Not: Gecici kopya gercek bir sandbox degildir; cargo build script calistirabilir. Izin oturumda bir kez sorulur."
    );
    println!("Manuel geri alma: 'geri al' | Cikis: 'cikis'\n");

    loop {
        print!("Sen > ");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break;
        }
        let input = input.trim();
        if input == "cikis" {
            break;
        }
        if input.is_empty() {
            continue;
        }
        // Rollback modelin araci degil: sadece kullanicinin acik komutuyla.
        if matches!(input.to_lowercase().as_str(), "geri al" | "rollback") {
            if let Some(record) = session.last_workspace.as_ref() {
                if prompter.ask_yes("Son workspace planinin tum dosyalari geri alinsin mi?")? {
                    match fiko_coder::workspace_patch::rollback(&session.root, record) {
                        Ok(()) => {
                            session.last_workspace = None;
                            println!("[ROLLBACK] Workspace planinin tum hedefleri geri alindi.");
                        }
                        Err(error) => println!(
                            "[ROLLBACK ERROR] Workspace tam geri alinamadi; proje durumu belirsiz: {error}"
                        ),
                    }
                }
                messages.truncate(1);
                continue;
            }
            match session.last_patch.as_ref() {
                None => println!("[ROLLBACK] Bu oturumda geri alinacak basarili patch yok."),
                Some(record) => {
                    println!("[ROLLBACK] Hedef: {}", record.path.display());
                    if prompter.ask_yes("Son basarili patch geri alinsin mi?")? {
                        match tools::rollback_patch(record) {
                            Ok(()) => {
                                session.last_patch = None;
                                println!("[ROLLBACK] Basarili. Eski icerik geri yuklendi.");
                            }
                            Err(e) => println!("[ROLLBACK ERROR] {e}"),
                        }
                    }
                }
            }
            // Modelle paylasilan eski patch durumunu guncellemek icin sohbeti sifirla.
            messages.truncate(1);
            continue;
        }

        messages.push(json!({ "role": "user", "content": input }));
        if let Err(e) =
            agent::run_task(&client, &model, &mut session, &mut messages, &mut prompter).await
        {
            // Yarim kalmis arac cagrisi gecmisi bir sonraki istegi bozabilir; sohbeti sifirla.
            println!("[HATA] Ajan dongusu kesildi: {e}");
            println!("[HATA] Ollama calisiyor mu? Sohbet gecmisi sifirlandi.\n");
            messages.truncate(1);
        }
    }
    Ok(())
}
