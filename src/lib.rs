//! FIKO CODER kutuphane hedefi.
//!
//! Moduller ikili (`src/main.rs`) ve `tests/` altindaki entegrasyon testleri
//! tarafindan paylasilir. Davranis ikili ile aynidir; bu dosya yalnizca
//! modulleri disa acar.
//!
//! - tools: dosya araclari, cargo, patch hazirlama, yedekleme / geri alma
//! - verification: adaylarin gecici kopyada derlenmesi, proje parmak izi
//! - repair_controller: deneme sayaclari, tekrar engeli, Verified Patch Lock
//! - intent: gorev niyeti (Generate / Analyze / Explain / Modify / Test / Unknown)
//! - execution: gorev asamalari (Created ... Completed / Blocked / Failed)
//! - orchestrator: arac cagrilarinin yetki, tekrar ve arguman denetimi
//! - agent: Ollama dongusu, arac secimi, kullanici onayi

pub mod agent;
pub mod context;
pub mod discovery;
pub mod execution;
pub mod intent;
pub mod orchestrator;
pub mod planning;
pub mod repair_controller;
pub mod tools;
pub mod verification;
mod workspace_agent;
pub mod workspace_patch;
