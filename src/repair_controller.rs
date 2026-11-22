//! v0.8 Repair Controller + Verified Patch Lock.
//!
//! Onarim dongusunun kontrolu modelde degil burada durur: deneme sayaclari,
//! tekrar eden patch engeli ve aday durumlari. v0.8 ile birlikte gercek projeye
//! yalnizca `authorize_apply`in onayladigi, birebir ayni ve hala gecerli bir
//! dogrulama kaydi olan aday uygulanabilir. Bu kurallar system prompt'a degil
//! bu module aittir.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::execution::{ExecutionState, TaskPhase};
use crate::workspace_patch::WorkspacePlan;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Yalniz gercek dogrulama basarisi uretir. changes, buyuk dosyalarda bile
/// dogrulanan tam planin ayni sinirli edit/create girdisiyle yeniden kurulmasini saglar.
pub(crate) struct PendingWorkspace {
    pub plan: WorkspacePlan,
    pub changes: Value,
}

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
    /// Modelin farkli anchor/girintiyle ayni gercek dosya adayini derletmesini engeller.
    prepared_candidates: HashSet<[u8; 32]>,
    workspace_proposals: HashMap<WorkspacePlan, CandidateStatus>,
    verified_workspaces: HashMap<WorkspacePlan, (u64, VerifyMode)>,
    pending_workspace: Option<PendingWorkspace>,
    /// Gorev icinde ulasilan en guclu dogrulama turu; apply icin alt sinir olur.
    required_mode: VerifyMode,
    /// v0.9: gorevin yurutme asamasi. Yalnizca gercek arac sonuclariyla ilerler.
    state: ExecutionState,
}

impl Default for RepairController {
    fn default() -> Self {
        Self {
            approved_attempts: 0,
            verify_attempts: 0,
            proposals: HashMap::new(),
            verified: HashMap::new(),
            prepared_candidates: HashSet::new(),
            workspace_proposals: HashMap::new(),
            verified_workspaces: HashMap::new(),
            pending_workspace: None,
            required_mode: VerifyMode::Check,
            state: ExecutionState::new(),
        }
    }
}

impl RepairController {
    /// Yalniz prepare_patch basarisi ve mevcut izin/kilit denetimlerinden sonra,
    /// gercek compile oncesi cagrilir. Verified Patch Lock kaydini degistirmez.
    pub(crate) fn register_prepared_candidate(
        &mut self,
        plan: &crate::tools::PatchPlan,
        mode: VerifyMode,
        project_fingerprint: u64,
    ) -> Result<(), String> {
        let original = Sha256::digest(plan.original.as_bytes());
        let updated = Sha256::digest(plan.updated.as_bytes());
        let fingerprint = project_fingerprint.to_le_bytes();
        let mut digest = Sha256::new();
        for field in [
            plan.path.as_os_str().as_encoded_bytes(),
            &original[..],
            &updated[..],
            mode.as_str().as_bytes(),
            &fingerprint,
        ] {
            digest.update((field.len() as u64).to_le_bytes());
            digest.update(field);
        }
        let identity: [u8; 32] = digest.finalize().into();
        if self.prepared_candidates.contains(&identity) {
            return Err("CANDIDATE_REPEAT: Ayni gercek dosya adayi bu dogrulama modunda zaten denendi. Farkli bir gercek aday hazirlayin; eski aday zaten dogrulandiysa birebir ayni path/old_text/new_text ile apply_patch kullanin.".into());
        }
        if self.prepared_candidates.len() >= crate::orchestrator::MAX_TOOL_CALLS_PER_TASK {
            return Err("Hazirlanmis aday kaydi gorevin arac cagrisi sinirina ulasti; yeni derleme baslatilmadi.".into());
        }
        self.prepared_candidates.insert(identity);
        Ok(())
    }

    pub(crate) fn pending_workspace_args(&self) -> Option<Value> {
        self.pending_workspace
            .as_ref()
            .map(|pending| serde_json::json!({"changes": pending.changes}))
    }

    pub(crate) fn clear_pending_workspace(&mut self) {
        self.pending_workspace = None;
    }

    pub(crate) fn take_pending_workspace(&mut self) -> Option<PendingWorkspace> {
        self.pending_workspace.take()
    }

    pub(crate) fn record_pending_workspace(&mut self, plan: &WorkspacePlan, changes: &Value) {
        self.pending_workspace = Some(PendingWorkspace {
            plan: plan.clone(),
            changes: changes.clone(),
        });
    }

    /// Cok dosyali kimlik tum hedefleri, eski durumlari ve yeni icerikleri kapsar.
    /// Tek dosyali akisin deneme butcesi ve asama makinesi paylasilir.
    pub fn register_workspace_candidate(
        &mut self,
        plan: &WorkspacePlan,
        mode: VerifyMode,
    ) -> Result<(), String> {
        if self.exhausted() || self.approved_attempts >= MAX_PATCH_ATTEMPTS {
            return Err(ApplyRejection::AttemptLimit.to_string());
        }
        match self.workspace_proposals.get(plan) {
            Some(CandidateStatus::Failed | CandidateStatus::Pending) => {
                return Err(
                    "Ayni basarisiz veya tamamlanmamis workspace adayi tekrar denenemez.".into(),
                );
            }
            Some(CandidateStatus::Verified)
                if self
                    .verified_workspaces
                    .get(plan)
                    .is_some_and(|(_, have)| *have >= mode) =>
            {
                return Err(
                    "Workspace adayi zaten dogrulandi; ayni changes ile apply_workspace kullan."
                        .into(),
                );
            }
            _ => {}
        }
        self.workspace_proposals
            .insert(plan.clone(), CandidateStatus::Pending);
        Ok(())
    }

    pub fn record_workspace_success(
        &mut self,
        plan: &WorkspacePlan,
        fingerprint: u64,
        mode: VerifyMode,
    ) {
        self.verify_attempts += 1;
        self.required_mode = self.required_mode.max(mode);
        self.workspace_proposals
            .insert(plan.clone(), CandidateStatus::Verified);
        self.verified_workspaces
            .insert(plan.clone(), (fingerprint, mode));
    }

    pub fn record_workspace_failure(&mut self, plan: &WorkspacePlan) {
        self.verify_attempts += 1;
        self.mark_workspace_failed(plan);
    }

    pub fn mark_workspace_failed(&mut self, plan: &WorkspacePlan) {
        self.verified_workspaces.remove(plan);
        self.workspace_proposals
            .insert(plan.clone(), CandidateStatus::Failed);
    }

    pub fn authorize_workspace(
        &mut self,
        plan: &WorkspacePlan,
        fingerprint: u64,
    ) -> Result<(), ApplyRejection> {
        if self.approved_attempts >= MAX_PATCH_ATTEMPTS {
            return Err(ApplyRejection::AttemptLimit);
        }
        match self.workspace_proposals.get(plan) {
            Some(status @ (CandidateStatus::Failed | CandidateStatus::Pending)) => {
                return Err(ApplyRejection::AlreadyTried(*status));
            }
            Some(CandidateStatus::Stale) => return Err(ApplyRejection::StaleProject),
            _ => {}
        }
        let Some(&(verified_fingerprint, mode)) = self.verified_workspaces.get(plan) else {
            return Err(ApplyRejection::NotVerified);
        };
        if mode < self.required_mode {
            return Err(ApplyRejection::InsufficientMode {
                have: mode,
                need: self.required_mode,
            });
        }
        if verified_fingerprint != fingerprint {
            self.verified_workspaces.remove(plan);
            self.workspace_proposals
                .insert(plan.clone(), CandidateStatus::Stale);
            return Err(ApplyRejection::StaleProject);
        }
        Ok(())
    }

    /// Gorevin su anki asamasi.
    pub fn phase(&self) -> TaskPhase {
        self.state.phase()
    }

    /// Asama gecisi; yalnizca arac akislari cagirir (dogrulama, onay, yazma).
    /// Izinsiz gecis uygulanmaz ve `false` doner; durum makinesi kaydini tutar.
    pub fn enter(&mut self, phase: TaskPhase) -> bool {
        self.state.enter(phase).is_ok()
    }

    pub fn execution(&self) -> &ExecutionState {
        &self.state
    }

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

    fn prepared_plan(old_text: &str, new_text: &str) -> crate::tools::PatchPlan {
        let original = "fn main() {\n    let value = 1;\n}\n".to_owned();
        let position = crate::tools::unique_line_block_match(&original, old_text).unwrap();
        let mut updated = original.clone();
        updated.replace_range(position..position + old_text.len(), new_text);
        crate::tools::PatchPlan {
            path: std::env::current_dir()
                .unwrap()
                .canonicalize()
                .unwrap()
                .join("src/main.rs"),
            original,
            updated,
        }
    }

    #[test]
    fn ayni_hazirlanmis_byte_farkli_anchor_ile_yeniden_derlenmemeli() {
        let mut controller = RepairController::default();
        let first = prepared_plan("let value = 1;", "let value = 2;");
        let equivalent = prepared_plan("    let value = 1;", "    let value = 2;");
        assert_eq!(first.original, equivalent.original);
        assert_eq!(first.updated, equivalent.updated);
        controller
            .register_prepared_candidate(&first, VerifyMode::Check, 11)
            .unwrap();
        let error = controller
            .register_prepared_candidate(&equivalent, VerifyMode::Check, 11)
            .unwrap_err();
        assert!(error.contains("CANDIDATE_REPEAT"));
        assert!(!error.contains("fn main()"));
        assert!(!error.contains(first.path.to_string_lossy().as_ref()));
        assert_eq!(controller.prepared_candidates.len(), 1);
        assert_eq!(controller.verify_attempts(), 0);
        assert_eq!(controller.approved_attempts(), 0);
        assert!(controller.proposals.is_empty());
        assert!(controller.verified.is_empty());
        assert_eq!(controller.phase(), TaskPhase::Created);
    }

    #[test]
    fn gercek_satir_sonu_farki_ve_check_test_yukseltmesi_ayri_aday_olmali() {
        let mut controller = RepairController::default();
        let plan = prepared_plan("let value = 1;", "let value = 2;");
        controller
            .register_prepared_candidate(&plan, VerifyMode::Check, 11)
            .unwrap();
        controller
            .register_prepared_candidate(&plan, VerifyMode::Test, 11)
            .unwrap();
        assert!(
            controller
                .register_prepared_candidate(&plan, VerifyMode::Test, 11)
                .is_err()
        );
        let mut updated_newline = prepared_plan("let value = 1;", "let value = 2;");
        updated_newline.updated.pop();
        controller
            .register_prepared_candidate(&updated_newline, VerifyMode::Check, 11)
            .unwrap();
        let mut original_newline = prepared_plan("let value = 1;", "let value = 2;");
        original_newline.original.pop();
        controller
            .register_prepared_candidate(&original_newline, VerifyMode::Check, 11)
            .unwrap();
        controller
            .register_prepared_candidate(&plan, VerifyMode::Check, 12)
            .unwrap();
        assert_eq!(controller.prepared_candidates.len(), 5);
        assert_eq!(
            controller.required_mode(),
            VerifyMode::Check,
            "Yeni kayit kilit seviyesini degistirmez."
        );
    }

    fn workspace_plan() -> WorkspacePlan {
        WorkspacePlan {
            files: ["src/a.rs", "src/b.rs"]
                .iter()
                .map(|path| crate::workspace_patch::PlannedFile {
                    relative: (*path).into(),
                    path: (*path).into(),
                    original: Some("eski".into()),
                    updated: "yeni".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn workspace_kilidi_tum_hedefleri_ve_icerikleri_baglamali() {
        let mut controller = RepairController::default();
        let plan = workspace_plan();
        assert_eq!(
            controller.authorize_workspace(&plan, 33),
            Err(ApplyRejection::NotVerified)
        );
        controller
            .register_workspace_candidate(&plan, VerifyMode::Check)
            .unwrap();
        controller.record_workspace_success(&plan, 33, VerifyMode::Check);
        assert_eq!(controller.authorize_workspace(&plan, 33), Ok(()));
        let mut changed = plan.clone();
        changed.files[1].updated = "baska".into();
        assert_eq!(
            controller.authorize_workspace(&changed, 33),
            Err(ApplyRejection::NotVerified)
        );
        changed = plan.clone();
        changed.files.remove(1);
        assert_eq!(
            controller.authorize_workspace(&changed, 33),
            Err(ApplyRejection::NotVerified)
        );
        changed = plan.clone();
        changed.files[1].original = None;
        assert_eq!(
            controller.authorize_workspace(&changed, 33),
            Err(ApplyRejection::NotVerified)
        );
    }

    #[test]
    fn workspace_kilidi_stale_mod_ve_ortak_deneme_sinirini_korumali() {
        let mut controller = RepairController::default();
        let plan = workspace_plan();
        controller.record_workspace_success(&plan, 33, VerifyMode::Check);
        assert_eq!(
            controller.authorize_workspace(&plan, 34),
            Err(ApplyRejection::StaleProject)
        );
        assert_eq!(
            controller.authorize_workspace(&plan, 33),
            Err(ApplyRejection::StaleProject)
        );
        controller
            .register_workspace_candidate(&plan, VerifyMode::Test)
            .unwrap();
        controller.record_workspace_success(&plan, 34, VerifyMode::Test);
        let mut other = plan.clone();
        other.files[0].updated = "baska".into();
        controller.record_workspace_success(&other, 34, VerifyMode::Check);
        assert_eq!(
            controller.authorize_workspace(&other, 34),
            Err(ApplyRejection::InsufficientMode {
                have: VerifyMode::Check,
                need: VerifyMode::Test
            })
        );
        assert!(
            controller
                .register_workspace_candidate(&other, VerifyMode::Test)
                .is_err()
        );
        assert_eq!(controller.authorize_workspace(&plan, 34), Ok(()));
        for _ in 0..MAX_PATCH_ATTEMPTS {
            controller.approve();
        }
        assert_eq!(
            controller.authorize_workspace(&plan, 34),
            Err(ApplyRejection::AttemptLimit)
        );
    }

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
