//! FIKO CODER v0.7 - CODE VERIFICATION ENGINE
//!
//! Moduller:
//! - tools: dosya araclari, cargo, patch hazirlama, yedekleme / geri alma
//! - verification: adaylarin gecici kopyada derlenmesi (verify_candidate)
//! - repair_controller: deneme sayaclari ve tekrar engeli
//! - agent: Ollama dongusu, arac secimi, kullanici onayi

mod agent;
mod repair_controller;
mod tools;
mod verification;

use std::io::{self, Write};

use serde_json::{Value, json};

use agent::{Prompter, Session, TerminalPrompter};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "qwen-code-local:9b".into());
    let root = std::env::current_dir()?.canonicalize()?;
    let client = reqwest::Client::new();
    let mut session = Session::new(root.clone());
    let mut prompter = TerminalPrompter;

    let system: Value = json!({ "role": "system", "content": agent::system_prompt() });
    let mut messages: Vec<Value> = vec![system.clone()];

    println!("=================================");
    println!("  FIKO CODER v0.7 - CODE VERIFICATION ENGINE");
    println!("=================================");
    println!("Model: {model}");
    println!("Proje: {}", root.display());
    println!(
        "Araclar: read_file, list_files, read_lines, search_code, run_tests, verify_candidate, apply_patch"
    );
    println!(
        "Akis: verify_candidate (gecici kopya) -> kullanici onayi -> apply_patch -> cargo check + cargo test"
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
