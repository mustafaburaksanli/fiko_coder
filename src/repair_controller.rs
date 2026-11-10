//! v0.7 Repair Controller.
//!
//! Onarim dongusunun kontrolu modelde degil burada durur: deneme sayaclari,
//! tekrar eden patch engeli ve dogrulanmis / basarisiz aday durumlari.

use std::collections::HashMap;

/// Hem onayli patch sayisi hem de gecici kopyadaki dogrulama denemeleri icin ust sinir.
pub const MAX_PATCH_ATTEMPTS: usize = 3;

/// (path, old_text, new_text) uclusu bir patch onerisini benzersiz tanimlar.
pub type Proposal = (String, String, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateStatus {
    /// Kaydedildi; henuz derleyici sonucu yok.
    Pending,
    /// Gecici kopyada cargo dogrulamasindan gecti.
    Verified,
    /// Gecici kopyada ya da gercek projede basarisiz oldu.
    Failed,
}

impl CandidateStatus {
    pub fn label(self) -> &'static str {
        match self {
            CandidateStatus::Pending => "beklemede",
            CandidateStatus::Verified => "dogrulandi",
            CandidateStatus::Failed => "basarisiz",
        }
    }
}

fn key(path: &str, old_text: &str, new_text: &str) -> Proposal {
    (path.to_string(), old_text.to_string(), new_text.to_string())
}

#[derive(Default)]
pub struct RepairController {
    approved_attempts: usize,
    verify_attempts: usize,
    proposals: HashMap<Proposal, CandidateStatus>,
}

impl RepairController {
    /// apply_patch icin kayit. Dogrulanmis bir aday uygulanabilir; diger
    /// oneriler bu gorevde yalnizca bir kez kabul edilir.
    pub fn register(&mut self, path: &str, old_text: &str, new_text: &str) -> Result<(), String> {
        if self.approved_attempts >= MAX_PATCH_ATTEMPTS {
            return Err(format!(
                "Bu gorevde {MAX_PATCH_ATTEMPTS} onayli patch sinirina ulasildi."
            ));
        }
        let proposal = key(path, old_text, new_text);
        match self.proposals.get(&proposal) {
            Some(CandidateStatus::Verified) => Ok(()),
            Some(status) => Err(format!(
                "Ayni patch onerisi bu gorevde zaten denendi (durum: {}). Tekrar engellendi.",
                status.label()
            )),
            None if self.exhausted() => Err(format!(
                "Dogrulama deneme siniri ({MAX_PATCH_ATTEMPTS}) doldu; dogrulanmamis yeni patch kabul edilmiyor."
            )),
            None => {
                self.proposals.insert(proposal, CandidateStatus::Pending);
                Ok(())
            }
        }
    }

    /// verify_candidate icin kayit. Ayni oneri ikinci kez dogrulanamaz.
    pub fn register_candidate(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> Result<(), String> {
        if self.exhausted() {
            return Err(format!(
                "Dogrulama deneme siniri ({MAX_PATCH_ATTEMPTS}/{MAX_PATCH_ATTEMPTS}) doldu. Yeni deneme yok."
            ));
        }
        let proposal = key(path, old_text, new_text);
        match self.proposals.get(&proposal) {
            Some(CandidateStatus::Verified) => Err(
                "Bu patch zaten dogrulandi; verify_candidate yerine ayni path/old_text/new_text ile apply_patch cagir."
                    .into(),
            ),
            Some(CandidateStatus::Failed) => Err(
                "Ayni basarisiz patch tekrar denenemez. Derleyici hatasini oku ve FARKLI bir new_text oner."
                    .into(),
            ),
            Some(CandidateStatus::Pending) => Err(
                "Bu patch onerisi daha once gonderildi ve sonuclanmadi (old_text eslesmedi veya dogrulama baslatilamadi). Farkli bir oneri gonder."
                    .into(),
            ),
            None => {
                self.proposals.insert(proposal, CandidateStatus::Pending);
                Ok(())
            }
        }
    }

    /// Gecici kopyadaki cargo sonucunu kaydeder ve deneme sayacini artirir.
    pub fn record_verification(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
        success: bool,
    ) {
        self.verify_attempts += 1;
        let status = if success {
            CandidateStatus::Verified
        } else {
            CandidateStatus::Failed
        };
        self.proposals.insert(key(path, old_text, new_text), status);
    }

    /// Gercek projede basarisiz olup geri alinan patch'i isaretler.
    pub fn mark_failed(&mut self, path: &str, old_text: &str, new_text: &str) {
        self.proposals
            .insert(key(path, old_text, new_text), CandidateStatus::Failed);
    }

    pub fn approve(&mut self) {
        self.approved_attempts += 1;
    }

    pub fn approved_attempts(&self) -> usize {
        self.approved_attempts
    }

    pub fn verify_attempts(&self) -> usize {
        self.verify_attempts
    }

    pub fn remaining_verifications(&self) -> usize {
        MAX_PATCH_ATTEMPTS.saturating_sub(self.verify_attempts)
    }

    /// Dogrulama denemeleri tukendi mi? Basarili ucuncu deneme de sayaci doldurur;
    /// dogrulanmis aday yine de uygulanabilir.
    pub fn exhausted(&self) -> bool {
        self.verify_attempts >= MAX_PATCH_ATTEMPTS
    }

    pub fn is_verified(&self, path: &str, old_text: &str, new_text: &str) -> bool {
        self.status(path, old_text, new_text) == Some(CandidateStatus::Verified)
    }

    pub fn status(&self, path: &str, old_text: &str, new_text: &str) -> Option<CandidateStatus> {
        self.proposals.get(&key(path, old_text, new_text)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ayni_patch_tekrar_engellenmeli() {
        let mut controller = RepairController::default();
        assert!(controller.register("src/main.rs", "eski", "yeni").is_ok());
        assert!(controller.register("src/main.rs", "eski", "yeni").is_err());
        assert!(controller.register("src/main.rs", "eski", "baska").is_ok());
    }

    #[test]
    fn uc_onayli_deneme_siniri() {
        let mut controller = RepairController::default();
        for i in 0..MAX_PATCH_ATTEMPTS {
            assert!(
                controller
                    .register("src/main.rs", &format!("eski{i}"), "yeni")
                    .is_ok()
            );
            controller.approve();
        }
        assert!(
            controller
                .register("src/main.rs", "yeni-eski", "yeni")
                .is_err()
        );
    }

    #[test]
    fn tekrar_eden_basarisiz_aday_engellenmeli() {
        let mut controller = RepairController::default();
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "bozuk")
                .is_ok()
        );
        controller.record_verification("src/main.rs", "eski", "bozuk", false);
        assert_eq!(
            controller.status("src/main.rs", "eski", "bozuk"),
            Some(CandidateStatus::Failed)
        );
        // Ayni basarisiz aday ne dogrulanabilir ne de uygulanabilir.
        let err = controller
            .register_candidate("src/main.rs", "eski", "bozuk")
            .unwrap_err();
        assert!(err.contains("FARKLI"));
        assert!(controller.register("src/main.rs", "eski", "bozuk").is_err());
        // Farkli bir oneri hala kabul edilir.
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "duzgun")
                .is_ok()
        );
    }

    #[test]
    fn uc_dogrulama_denemesi_siniri() {
        let mut controller = RepairController::default();
        for i in 0..MAX_PATCH_ATTEMPTS {
            let new_text = format!("bozuk{i}");
            assert!(!controller.exhausted());
            assert!(
                controller
                    .register_candidate("src/main.rs", "eski", &new_text)
                    .is_ok()
            );
            controller.record_verification("src/main.rs", "eski", &new_text, false);
        }
        assert!(controller.exhausted());
        assert_eq!(controller.remaining_verifications(), 0);
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "bozuk9")
                .is_err()
        );
        // Dogrulanmamis yeni patch artik uygulanamaz.
        assert!(
            controller
                .register("src/main.rs", "eski", "bozuk9")
                .is_err()
        );
    }

    #[test]
    fn dogrulanmis_aday_apply_icin_izinli_olmali() {
        let mut controller = RepairController::default();
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "yeni")
                .is_ok()
        );
        controller.record_verification("src/main.rs", "eski", "yeni", true);
        assert!(controller.is_verified("src/main.rs", "eski", "yeni"));
        // Dogrulanmis aday tekrar dogrulanmaz ama apply_patch icin kabul edilir.
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "yeni")
                .is_err()
        );
        assert!(controller.register("src/main.rs", "eski", "yeni").is_ok());
    }
}
