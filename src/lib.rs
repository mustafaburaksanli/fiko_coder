//! FIKO CODER kutuphane hedefi.
//!
//! Moduller ikili (`src/main.rs`) ve `tests/` altindaki entegrasyon testleri
//! tarafindan paylasilir. Davranis ikili ile aynidir; bu dosya yalnizca
//! modulleri disa acar.
//!
//! - tools: dosya araclari, cargo, patch hazirlama, yedekleme / geri alma
//! - verification: adaylarin gecici kopyada derlenmesi, proje parmak izi
//! - repair_controller: deneme sayaclari, tekrar engeli, Verified Patch Lock
//! - agent: Ollama dongusu, arac secimi, kullanici onayi

pub mod agent;
pub mod repair_controller;
pub mod tools;
pub mod verification;
