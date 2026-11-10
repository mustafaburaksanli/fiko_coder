//! v0.8 Repair Controller + Verified Patch Lock.
//!
//! Onarim dongusunun kontrolu modelde degil burada durur: deneme sayaclari,
//! tekrar eden patch engeli ve aday durumlari. v0.8 ile birlikte gercek projeye
//! yalnizca `authorize_apply`in onayladigi, birebir ayni ve hala gecerli bir
//! dogrulama kaydi olan aday uygulanabilir. Bu kurallar system prompt'a degil
//! bu module aittir.

use std::collections::HashMap;
use std::fmt;

/// Hem onayli patch sayisi hem de gecici kopyadaki dogrulama denemeleri icin ust sinir.
pub const MAX_PATCH_ATTEMPTS: usize = 3;

/// (path, old_text, new_text) uclusu bir patch onerisini benzersiz tanimlar.
pub type Proposal = (String, String, String);

/// Dogrulama turu. Siralama onemlidir: Test, Check'ten guclu kabul edilir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerifyMode {
    Check,
    Test,
}

impl VerifyMode {
    pub fn parse(mode: &str) -> Option<Self> {
        match mode {
            "check" => Some(VerifyMode::Check),
            "test" => Some(VerifyMode::Test),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VerifyMode::Check => "check",
            VerifyMode::Test => "test",
        }
    }
}

/// Gecici kopyada basariyla dogrulanan adayin Rust tarafindaki kaydi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCandidate {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    /// Dogrulama aninda hedef dosyanin orijinal iceriginin ozeti.
    pub original_hash: u64,
    /// Adayin uygulanmis halinin (yeni dosya icerigi) ozeti.
    pub updated_hash: u64,
    /// Dogrulama aninda kopyalanan proje dosyalarinin ozeti.
    pub project_fingerprint: u64,
    pub mode: VerifyMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateStatus {
    /// Kaydedildi; henuz derleyici sonucu yok.
    Pending,
    /// Gecici kopyada cargo dogrulamasindan gecti ve kaydi hala gecerli.
    Verified,
    /// Gecici kopyada ya da gercek projede basarisiz oldu.
    Failed,
    /// Dogrulandi ama dosya/proje sonradan degisti; yeniden dogrulama gerekir.
    Stale,
}

impl CandidateStatus {
    pub fn label(self) -> &'static str {
        match self {
            CandidateStatus::Pending => "beklemede",
            CandidateStatus::Verified => "dogrulandi",
            CandidateStatus::Failed => "basarisiz",
            CandidateStatus::Stale => "eski dogrulama",
        }
    }
}

/// `authorize_apply`in kilidi acmama nedenleri.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyRejection {
    /// Onayli patch siniri doldu.
    AttemptLimit,
    /// Ayni aday daha once basarisiz oldu ya da sonuclanmadi.
    AlreadyTried(CandidateStatus),
    /// Bu aday hic dogrulanmadi.
    NotVerified,
    /// Ayni dosya icin dogrulanan aday var ama old_text/new_text farkli.
    Modified,
    /// Aday yalnizca daha zayif bir turle dogrulandi.
    InsufficientMode { have: VerifyMode, need: VerifyMode },
    /// Dogrulamadan sonra hedef dosya degisti.
    StaleFile,
    /// Dogrulamadan sonra proje durumu (diger dosyalar) degisti.
    StaleProject,
}

impl ApplyRejection {
    /// Yeniden dogrulama ile asilabilir mi? (AttemptLimit ve AlreadyTried asilamaz.)
    pub fn needs_verification(&self) -> bool {
        !matches!(
            self,
            ApplyRejection::AttemptLimit | ApplyRejection::AlreadyTried(_)
        )
    }
}

impl fmt::Display for ApplyRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyRejection::AttemptLimit => write!(
                f,
                "Bu gorevde {MAX_PATCH_ATTEMPTS} onayli patch sinirina ulasildi."
            ),
            ApplyRejection::AlreadyTried(status) => write!(
                f,
                "Ayni patch onerisi bu gorevde zaten denendi (durum: {}). Tekrar engellendi.",
                status.label()
            ),
            ApplyRejection::NotVerified => {
                write!(f, "Aday verify_candidate ile dogrulanmamis.")
            }
            ApplyRejection::Modified => write!(
                f,
                "Aday, dogrulanan adaydan farkli (old_text veya new_text degisti); dogrulama baska bir icerige ait."
            ),
            ApplyRejection::InsufficientMode { have, need } => write!(
                f,
                "Aday yalnizca cargo {} ile dogrulandi; bu gorevde cargo {} seviyesi gerekiyor.",
                have.as_str(),
                need.as_str()
            ),
            ApplyRejection::StaleFile => write!(
                f,
                "Dogrulamadan sonra hedef dosya degisti; eski dogrulama gecersiz."
            ),
            ApplyRejection::StaleProject => write!(
                f,
                "Dogrulamadan sonra proje dosyalari degisti; eski dogrulama gecersiz."
            ),
        }
    }
}

/// Model ayni dosyayi "src/main.rs", "./src/main.rs" veya "src\\main.rs" olarak
/// gonderebilir; kilit anahtari bunlari tek bicime indirir. Guvenlik kontrolu
/// yine prepare_patch'tedir.
fn normalize_path(path: &str) -> String {
    let mut text = path.trim().replace('\\', "/");
    while let Some(rest) = text.strip_prefix("./") {
        text = rest.to_string();
    }
    text
}

fn key(path: &str, old_text: &str, new_text: &str) -> Proposal {
    (
        normalize_path(path),
        old_text.to_string(),
        new_text.to_string(),
    )
}

pub struct RepairController {
    approved_attempts: usize,
    verify_attempts: usize,
    proposals: HashMap<Proposal, CandidateStatus>,
    verified: HashMap<Proposal, VerifiedCandidate>,
    /// Gorev icinde ulasilan en guclu dogrulama turu; apply icin alt sinir olur.
    required_mode: VerifyMode,
}

impl Default for RepairController {
    fn default() -> Self {
        Self {
            approved_attempts: 0,
            verify_attempts: 0,
            proposals: HashMap::new(),
            verified: HashMap::new(),
            required_mode: VerifyMode::Check,
        }
    }
}

impl RepairController {
    /// apply_patch icin on kayit: deneme siniri ve tekrar engeli. Dogrulanmis
    /// veya eskimis aday gecer; diger oneriler bu gorevde yalnizca bir kez kabul edilir.
    /// Kilidin kendisi `authorize_apply`dadir.
    pub fn register(&mut self, path: &str, old_text: &str, new_text: &str) -> Result<(), String> {
        if self.approved_attempts >= MAX_PATCH_ATTEMPTS {
            return Err(ApplyRejection::AttemptLimit.to_string());
        }
        let proposal = key(path, old_text, new_text);
        match self.proposals.get(&proposal) {
            Some(CandidateStatus::Verified) | Some(CandidateStatus::Stale) => Ok(()),
            Some(status) => Err(ApplyRejection::AlreadyTried(*status).to_string()),
            None if self.exhausted() => Err(format!(
                "Dogrulama deneme siniri ({MAX_PATCH_ATTEMPTS}) doldu; dogrulanmamis yeni patch kabul edilmiyor."
            )),
            None => {
                self.proposals.insert(proposal, CandidateStatus::Pending);
                Ok(())
            }
        }
    }

    /// verify_candidate icin kayit. Basarisiz aday tekrar dogrulanamaz; dogrulanmis
    /// aday yalnizca daha guclu bir turle (check -> test) yeniden dogrulanabilir.
    pub fn register_candidate(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
        mode: VerifyMode,
    ) -> Result<(), String> {
        if self.exhausted() {
            return Err(format!(
                "Dogrulama deneme siniri ({MAX_PATCH_ATTEMPTS}/{MAX_PATCH_ATTEMPTS}) doldu. Yeni deneme yok."
            ));
        }
        let proposal = key(path, old_text, new_text);
        match self.proposals.get(&proposal) {
            Some(CandidateStatus::Verified) => {
                let have = self
                    .verified
                    .get(&proposal)
                    .map(|v| v.mode)
                    .unwrap_or(VerifyMode::Check);
                if mode > have {
                    self.proposals.insert(proposal, CandidateStatus::Pending);
                    return Ok(());
                }
                Err(
                    "Bu patch zaten dogrulandi; verify_candidate yerine ayni path/old_text/new_text ile apply_patch cagir."
                        .into(),
                )
            }
            Some(CandidateStatus::Failed) => Err(
                "Ayni basarisiz patch tekrar denenemez. Derleyici hatasini oku ve FARKLI bir new_text oner."
                    .into(),
            ),
            Some(CandidateStatus::Pending) => Err(
                "Bu patch onerisi daha once gonderildi ve sonuclanmadi (old_text eslesmedi veya dogrulama baslatilamadi). Farkli bir oneri gonder."
                    .into(),
            ),
            Some(CandidateStatus::Stale) | None => {
                self.proposals.insert(proposal, CandidateStatus::Pending);
                Ok(())
            }
        }
    }

    /// Basarili dogrulamayi kaydeder; deneme sayacini artirir ve gerekirse
    /// gorevin zorunlu dogrulama seviyesini yukseltir.
    pub fn record_success(&mut self, candidate: VerifiedCandidate) {
        self.verify_attempts += 1;
        if candidate.mode > self.required_mode {
            self.required_mode = candidate.mode;
        }
        let proposal = key(&candidate.path, &candidate.old_text, &candidate.new_text);
        self.proposals
            .insert(proposal.clone(), CandidateStatus::Verified);
        self.verified.insert(proposal, candidate);
    }

    /// Basarisiz dogrulamayi kaydeder ve deneme sayacini artirir.
    pub fn record_failure(&mut self, path: &str, old_text: &str, new_text: &str) {
        self.verify_attempts += 1;
        self.mark_failed(path, old_text, new_text);
    }

    /// Gercek projede basarisiz olup geri alinan (ya da sandbox'ta duşen) patch'i isaretler.
    pub fn mark_failed(&mut self, path: &str, old_text: &str, new_text: &str) {
        let proposal = key(path, old_text, new_text);
        self.verified.remove(&proposal);
        self.proposals.insert(proposal, CandidateStatus::Failed);
    }

    /// v0.8 kilidi: uygulanacak aday, dogrulanan adayla birebir ayni olmali;
    /// dogrulama turu gorevin gerektirdigi seviyede olmali; dosya ve proje
    /// dogrulamadan bu yana degismemis olmali. Eskiyen kayit silinir ve aday
    /// `Stale` olur; boylece yeniden dogrulanabilir.
    pub fn authorize_apply(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
        original_hash: u64,
        updated_hash: u64,
        project_fingerprint: u64,
    ) -> Result<VerifiedCandidate, ApplyRejection> {
        if self.approved_attempts >= MAX_PATCH_ATTEMPTS {
            return Err(ApplyRejection::AttemptLimit);
        }
        let proposal = key(path, old_text, new_text);
        match self.proposals.get(&proposal) {
            Some(CandidateStatus::Failed) => {
                return Err(ApplyRejection::AlreadyTried(CandidateStatus::Failed));
            }
            Some(CandidateStatus::Stale) => return Err(ApplyRejection::StaleFile),
            Some(CandidateStatus::Verified) => {}
            Some(CandidateStatus::Pending) | None => {
                let same_file_verified = self
                    .verified
                    .keys()
                    .any(|(verified_path, _, _)| *verified_path == proposal.0);
                return Err(if same_file_verified {
                    ApplyRejection::Modified
                } else {
                    ApplyRejection::NotVerified
                });
            }
        }

        let Some(record) = self.verified.get(&proposal) else {
            return Err(ApplyRejection::NotVerified);
        };
        if record.mode < self.required_mode {
            return Err(ApplyRejection::InsufficientMode {
                have: record.mode,
                need: self.required_mode,
            });
        }
        if record.original_hash != original_hash || record.updated_hash != updated_hash {
            self.invalidate(&proposal);
            return Err(ApplyRejection::StaleFile);
        }
        if record.project_fingerprint != project_fingerprint {
            self.invalidate(&proposal);
            return Err(ApplyRejection::StaleProject);
        }
        Ok(record.clone())
    }

    fn invalidate(&mut self, proposal: &Proposal) {
        self.verified.remove(proposal);
        self.proposals
            .insert(proposal.clone(), CandidateStatus::Stale);
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

    /// Bu gorevde apply icin gereken en dusuk dogrulama turu.
    pub fn required_mode(&self) -> VerifyMode {
        self.required_mode
    }

    /// Test ve inceleme yardimcisi; akis karari `authorize_apply`dadir.
    #[cfg(test)]
    pub fn is_verified(&self, path: &str, old_text: &str, new_text: &str) -> bool {
        self.status(path, old_text, new_text) == Some(CandidateStatus::Verified)
    }

    #[cfg(test)]
    pub fn verified_mode(&self, path: &str, old_text: &str, new_text: &str) -> Option<VerifyMode> {
        self.verified
            .get(&key(path, old_text, new_text))
            .map(|v| v.mode)
    }

    #[cfg(test)]
    pub fn status(&self, path: &str, old_text: &str, new_text: &str) -> Option<CandidateStatus> {
        self.proposals.get(&key(path, old_text, new_text)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verified(path: &str, old: &str, new: &str, mode: VerifyMode) -> VerifiedCandidate {
        VerifiedCandidate {
            path: path.into(),
            old_text: old.into(),
            new_text: new.into(),
            original_hash: 11,
            updated_hash: 22,
            project_fingerprint: 33,
            mode,
        }
    }

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
                .register_candidate("src/main.rs", "eski", "bozuk", VerifyMode::Check)
                .is_ok()
        );
        controller.record_failure("src/main.rs", "eski", "bozuk");
        assert_eq!(
            controller.status("src/main.rs", "eski", "bozuk"),
            Some(CandidateStatus::Failed)
        );
        // Ayni basarisiz aday ne dogrulanabilir ne de uygulanabilir.
        let err = controller
            .register_candidate("src/main.rs", "eski", "bozuk", VerifyMode::Check)
            .unwrap_err();
        assert!(err.contains("FARKLI"));
        assert!(controller.register("src/main.rs", "eski", "bozuk").is_err());
        // Farkli bir oneri hala kabul edilir.
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "duzgun", VerifyMode::Check)
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
                    .register_candidate("src/main.rs", "eski", &new_text, VerifyMode::Check)
                    .is_ok()
            );
            controller.record_failure("src/main.rs", "eski", &new_text);
        }
        assert!(controller.exhausted());
        assert_eq!(controller.remaining_verifications(), 0);
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "bozuk9", VerifyMode::Check)
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
                .register_candidate("src/main.rs", "eski", "yeni", VerifyMode::Check)
                .is_ok()
        );
        controller.record_success(verified("src/main.rs", "eski", "yeni", VerifyMode::Check));
        assert!(controller.is_verified("src/main.rs", "eski", "yeni"));
        // Dogrulanmis aday ayni seviyede tekrar dogrulanmaz ama apply_patch icin kabul edilir.
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "yeni", VerifyMode::Check)
                .is_err()
        );
        assert!(controller.register("src/main.rs", "eski", "yeni").is_ok());
    }

    // ---------------- v0.8 Verified Patch Lock ----------------

    #[test]
    fn basarili_dogrulama_kilidi_acmali() {
        let mut controller = RepairController::default();
        controller.record_success(verified("src/main.rs", "eski", "yeni", VerifyMode::Check));
        let record = controller
            .authorize_apply("src/main.rs", "eski", "yeni", 11, 22, 33)
            .expect("birebir ayni aday uygulanabilmeli");
        assert_eq!(record.mode, VerifyMode::Check);
        // Yol bicimi farkli yazilsa da ayni dosyadir.
        assert!(
            controller
                .authorize_apply("./src\\main.rs", "eski", "yeni", 11, 22, 33)
                .is_ok()
        );
    }

    #[test]
    fn basarisiz_dogrulama_kilidi_acmamali() {
        let mut controller = RepairController::default();
        controller.record_failure("src/main.rs", "eski", "bozuk");
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski", "bozuk", 11, 22, 33),
            Err(ApplyRejection::AlreadyTried(CandidateStatus::Failed))
        );
        // Hic dogrulanmamis aday da kilidi acamaz.
        assert_eq!(
            controller.authorize_apply("src/lib.rs", "eski", "yeni", 11, 22, 33),
            Err(ApplyRejection::NotVerified)
        );
        assert!(ApplyRejection::NotVerified.needs_verification());
        assert!(!ApplyRejection::AlreadyTried(CandidateStatus::Failed).needs_verification());
    }

    #[test]
    fn degistirilmis_aday_reddedilmeli() {
        let mut controller = RepairController::default();
        controller.record_success(verified("src/main.rs", "eski", "yeni", VerifyMode::Check));
        // new_text farkli
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski", "yeni2", 11, 22, 33),
            Err(ApplyRejection::Modified)
        );
        // old_text farkli
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski2", "yeni", 11, 22, 33),
            Err(ApplyRejection::Modified)
        );
        // Degistirilmis aday yeniden dogrulanmak zorundadir; orijinal kayit bozulmaz.
        assert!(
            controller
                .authorize_apply("src/main.rs", "eski", "yeni", 11, 22, 33)
                .is_ok()
        );
    }

    #[test]
    fn eski_dogrulama_gecersiz_sayilmali() {
        let mut controller = RepairController::default();
        controller.record_success(verified("src/main.rs", "eski", "yeni", VerifyMode::Check));
        // Hedef dosya degisti (orijinal icerik ozeti farkli).
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski", "yeni", 99, 22, 33),
            Err(ApplyRejection::StaleFile)
        );
        assert_eq!(
            controller.status("src/main.rs", "eski", "yeni"),
            Some(CandidateStatus::Stale)
        );
        // Eskiyen aday artik hashler eslesse bile uygulanamaz; yeniden dogrulama gerekir.
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski", "yeni", 11, 22, 33),
            Err(ApplyRejection::StaleFile)
        );
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "yeni", VerifyMode::Check)
                .is_ok()
        );
        controller.record_success(verified("src/main.rs", "eski", "yeni", VerifyMode::Check));
        // Proje durumu degisti (baska dosya).
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski", "yeni", 11, 22, 44),
            Err(ApplyRejection::StaleProject)
        );
        assert_eq!(
            controller.status("src/main.rs", "eski", "yeni"),
            Some(CandidateStatus::Stale)
        );
    }

    #[test]
    fn check_dogrulamasi_test_gereken_gorevde_yetmemeli() {
        let mut controller = RepairController::default();
        controller.record_success(verified("src/main.rs", "eski", "a", VerifyMode::Test));
        assert_eq!(controller.required_mode(), VerifyMode::Test);
        controller.record_success(verified("src/main.rs", "eski", "b", VerifyMode::Check));
        // Yalnizca check ile dogrulanan farkli aday, test ile dogrulananin yerine gecemez.
        assert_eq!(
            controller.authorize_apply("src/main.rs", "eski", "b", 11, 22, 33),
            Err(ApplyRejection::InsufficientMode {
                have: VerifyMode::Check,
                need: VerifyMode::Test
            })
        );
        // Test ile dogrulanan aday uygulanabilir.
        assert!(
            controller
                .authorize_apply("src/main.rs", "eski", "a", 11, 22, 33)
                .is_ok()
        );
        // Check ile dogrulanan aday, test seviyesine yukseltmek icin yeniden dogrulanabilir.
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "b", VerifyMode::Test)
                .is_ok()
        );
        assert!(
            controller
                .register_candidate("src/main.rs", "eski", "a", VerifyMode::Test)
                .is_err()
        );
    }
}
