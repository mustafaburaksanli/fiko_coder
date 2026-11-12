//! Kaynak kodu tasimayan, Rust sahipli gorev plani.
//!
//! Plan bir izin veya dogrulama kaydi degildir. Hedefler yalnizca mevcut
//! araclarin yapilandirilmis girdilerinden alinir; serbest model metninden
//! kod/plan ayiklanmaz. Ilerleme, mevcut TaskPhase'in salt bir gorunumudur.

use std::collections::HashSet;
use std::path::{Component, Path};

use serde_json::Value;

use crate::discovery::check_relative_path;
use crate::execution::TaskPhase;
use crate::intent::workspace_write_targets;
use crate::workspace_patch::MAX_WORKSPACE_FILES;

const MAX_GOAL_CHARS: usize = 2_000;
const MAX_PATH_BYTES: usize = 1_024;
const MAX_INSPECTIONS: usize = 32;
const MAX_NOTE_CHARS: usize = 1_200;
const MAX_REPORT_CHARS: usize = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOperation {
    Edit,
    Create,
}

impl PlanOperation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Create => "create",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTarget {
    pub path: String,
    pub operation: PlanOperation,
    /// Islem niyeti; semantik gereksinimin gerceklesme kaniti degildir.
    pub purpose: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanConstraint {
    ProjectRootOnly,
    PreserveUserRestrictions,
    VerifiedCandidateRequired,
    ExplicitApprovalRequired,
}

impl PlanConstraint {
    fn label(self) -> &'static str {
        match self {
            Self::ProjectRootOnly => "yalniz izinli proje ici hedefler",
            Self::PreserveUserRestrictions => "kullanici kisitlamalari gecerlidir",
            Self::VerifiedCandidateRequired => "ayni aday icin Rust dogrulamasi gerekir",
            Self::ExplicitApprovalRequired => "tek acik uygulama onayi gerekir",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanVerification {
    /// Bir niyettir; proje turunu secmez veya Cargo calistirmaz.
    CargoCheckAndTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanProgress {
    Planned,
    Inspecting,
    ReadyForVerification,
    Verifying,
    Verified,
    AwaitingApproval,
    Applying,
    Completed,
    Blocked,
    Failed,
}

/// Mevcut Rust gorev durumuna bagli plan verisi. Bagimsiz stage alani yoktur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlan {
    pub goal: String,
    pub required_targets: Vec<String>,
    pub inspection_targets: Vec<String>,
    pub inspected_files: Vec<String>,
    pub targets: Vec<PlanTarget>,
    pub constraints: Vec<PlanConstraint>,
    pub verification: PlanVerification,
    pub uncertainties: Vec<String>,
    scope_error: Option<String>,
}

impl TaskPlan {
    pub fn new(request: &str) -> Self {
        // Kapsam tum orijinal istekten cikarilir; gorunum butcesi kapsam
        // kontrolunu sessizce daraltamaz.
        let requested = workspace_write_targets(request);
        let mut required_targets: Vec<String> = Vec::new();
        let mut scope_error = None;
        for path in requested {
            match normalized_path(&path) {
                Ok(path) => {
                    if !required_targets
                        .iter()
                        .any(|old| path_key(old) == path_key(&path))
                    {
                        if required_targets.len() == MAX_WORKSPACE_FILES {
                            scope_error = Some(format!(
                                "Istek {MAX_WORKSPACE_FILES} acik hedef sinirini asiyor; daha kucuk gorev gerekir."
                            ));
                            break;
                        }
                        required_targets.push(path);
                    }
                }
                Err(error) => {
                    scope_error = Some(format!("Istenen hedef guvenli degil: {error}"));
                    break;
                }
            }
        }
        let mut uncertainties = vec![
            "Cargo basarisi dogal dildeki tum gereksinimlerin karsilandigini kanitlamaz.".into(),
        ];
        if required_targets.is_empty() {
            uncertainties
                .push("Hedef dosyalar kesin degil; once proje/simge incelemesi gerekir.".into());
        }
        if required_targets.iter().any(|path| !path.contains('/')) {
            uncertainties.push("Yalniz dosya adi verilen hedefin tam konumu, gercek aday ve diff ile teyit edilmelidir.".into());
        }
        if let Some(error) = &scope_error {
            uncertainties.push(error.clone());
        }
        if request.chars().count() > MAX_GOAL_CHARS {
            uncertainties
                .push("Hedef gorunumu kisaltildi; orijinal kullanici istegi esas kalir.".into());
        }
        Self {
            goal: bounded(request, MAX_GOAL_CHARS),
            inspection_targets: required_targets.clone(),
            required_targets,
            inspected_files: Vec::new(),
            targets: Vec::new(),
            constraints: vec![
                PlanConstraint::ProjectRootOnly,
                PlanConstraint::PreserveUserRestrictions,
                PlanConstraint::VerifiedCandidateRequired,
                PlanConstraint::ExplicitApprovalRequired,
            ],
            verification: PlanVerification::CargoCheckAndTest,
            uncertainties,
            scope_error,
        }
    }

    /// Buraya gelen args mevcut dispatcher tarafindan dogrulanmis olmalidir.
    /// Yine de metadata bicimi ve yol guvenligi burada da sinirlandirilir.
    pub fn candidate_targets(name: &str, args: &Value) -> Result<Vec<PlanTarget>, String> {
        let target = |path: &Value, operation| -> Result<PlanTarget, String> {
            let path = path.as_str().ok_or("Plan hedef yolu metin olmali.")?;
            Ok(PlanTarget {
                path: normalized_path(path)?,
                operation,
                purpose: match operation {
                    PlanOperation::Edit => "Mevcut dosyada istenen satir/blok duzenlemesi".into(),
                    PlanOperation::Create => "Istenen yeni dosyayi olusturma".into(),
                },
            })
        };
        let targets = match name {
            "verify_candidate" | "apply_patch" => {
                vec![target(&args["path"], PlanOperation::Edit)?]
            }
            "verify_workspace" | "apply_workspace" => {
                let changes = args["changes"]
                    .as_array()
                    .ok_or("changes bir dizi olmali.")?;
                if changes.is_empty() || changes.len() > MAX_WORKSPACE_FILES {
                    return Err(format!("Plan 1-{MAX_WORKSPACE_FILES} hedef icermeli."));
                }
                changes
                    .iter()
                    .map(|change| {
                        let operation = match change["operation"].as_str() {
                            Some("edit") => PlanOperation::Edit,
                            Some("create") => PlanOperation::Create,
                            _ => return Err("Plan islemi yalniz edit veya create olabilir.".into()),
                        };
                        target(&change["path"], operation)
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
            _ => return Err("Bu arac bir degisiklik adayi tanimlamaz.".into()),
        };
        validate_targets(&targets)?;
        Ok(targets)
    }

    /// Salt metadata kapsam kontrolu; dosya icerigi, fingerprint ve ayni aday
    /// kilidinin yerini almaz. Yeni/degistirilmis aday yine mevcut Rust
    /// dogrulama yolundan gecmek zorundadir.
    pub fn check_scope(&self, targets: &[PlanTarget]) -> Result<(), String> {
        if let Some(error) = &self.scope_error {
            return Err(error.clone());
        }
        validate_targets(targets)?;
        let actual: HashSet<_> = targets
            .iter()
            .map(|target| normalized_path(&target.path).map(|path| path_key(&path)))
            .collect::<Result<_, _>>()?;
        let missing: Vec<_> = self
            .required_targets
            .iter()
            .filter(|path| !actual.contains(&path_key(path)))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Plan acik degisiklik hedeflerini kapsamiyor: {}.",
                missing.join(", ")
            ));
        }
        Ok(())
    }

    /// Basarisiz girdi mevcut plani degistirmez. Ayni metadata tekrar
    /// geldiyse ek plan kaydi veya model cagrisi uretilmez.
    pub fn prepare_candidate(&mut self, name: &str, args: &Value) -> Result<(), String> {
        let targets = Self::candidate_targets(name, args)?;
        // v1.0 tek dosya istekleri "main.rs icindeki hatayi duzelt" gibi
        // yalniz dosya adi verebilir. Bu eski yolda tek mevcut .rs hedefin
        // tam yolu Rust patch/lock/onay zincirinde belirlenir. Acik yol veya
        // cok dosyali kapsam bu uyumluluk istisnasina girmez.
        let basename_request = matches!(name, "verify_candidate" | "apply_patch")
            && self.scope_error.is_none()
            && self.required_targets.len() == 1
            && !self.required_targets[0].contains('/')
            && targets.len() == 1
            && Path::new(&targets[0].path).file_name().is_some_and(|name| {
                path_key(&name.to_string_lossy()) == path_key(&self.required_targets[0])
            });
        if !basename_request {
            self.check_scope(&targets)?;
        }
        for target in &targets {
            if target.operation == PlanOperation::Edit
                && self.inspection_targets.len() < MAX_INSPECTIONS
                && !self
                    .inspection_targets
                    .iter()
                    .any(|path| path_key(path) == path_key(&target.path))
            {
                self.inspection_targets.push(target.path.clone());
            }
        }
        self.targets = targets;
        Ok(())
    }

    /// Root bu metodu yalniz basarili, gercek okuma sonrasinda cagirir.
    pub fn inspect(&mut self, path: &str) {
        if let Ok(path) = normalized_path(path)
            && self.inspected_files.len() < MAX_INSPECTIONS
            && !self
                .inspected_files
                .iter()
                .any(|old| path_key(old) == path_key(&path))
        {
            self.inspected_files.push(path);
        }
    }

    /// Durum saklanmaz; tek Rust state machine'in anlik gorunumu hesaplanir.
    pub fn progress(&self, phase: TaskPhase, verified: bool) -> PlanProgress {
        match phase {
            TaskPhase::Created => PlanProgress::Planned,
            TaskPhase::Inspecting | TaskPhase::Planning if verified => PlanProgress::Verified,
            TaskPhase::Inspecting => PlanProgress::Inspecting,
            TaskPhase::Planning if self.targets.is_empty() => PlanProgress::Planned,
            TaskPhase::Planning => PlanProgress::ReadyForVerification,
            TaskPhase::Verifying => PlanProgress::Verifying,
            TaskPhase::AwaitingApproval => PlanProgress::AwaitingApproval,
            TaskPhase::Applying => PlanProgress::Applying,
            TaskPhase::Completed => PlanProgress::Completed,
            TaskPhase::Blocked => PlanProgress::Blocked,
            TaskPhase::Failed => PlanProgress::Failed,
        }
    }

    /// Modele verilen kisa yonlendirme. Orijinal istek ve arac kaniti model
    /// gecmisinde kalir; bu gorunum tek basina uygulama yetkisi vermez.
    pub fn note(&self) -> String {
        let paths = if self.targets.is_empty() {
            self.required_targets.join(", ")
        } else {
            self.targets
                .iter()
                .map(|target| format!("{}:{}", target.operation.label(), target.path))
                .collect::<Vec<_>>()
                .join(", ")
        };
        bounded(
            &format!(
                "[GOREV PLANI] Hedef: {}\nKapsam: {}\nOnce ilgili simge/dosyalari bul; buyuk dosyada search_in_file/read_lines kullan. Tum hedefleri birlikte dogrula. Plan izin veya basari kaniti degildir; Cargo semantik gereksinimleri tek basina ispatlamaz.",
                bounded(&self.goal, 240),
                if paths.is_empty() {
                    "belirsiz; inceleme gerekli"
                } else {
                    &paths
                }
            ),
            MAX_NOTE_CHARS,
        )
    }

    pub fn report(&self, phase: TaskPhase, verified: bool) -> String {
        let constraints = self
            .constraints
            .iter()
            .map(|constraint| constraint.label())
            .collect::<Vec<_>>()
            .join("; ");
        bounded(
            &format!(
                "[PLAN] Asama: {:?} (Rust: {phase}); hedef: {}\nHedefler: {}\nIncelenen: {}\nKisitlar: {constraints}\nYaklasim: desteklenen Cargo projesinde gecici dogrulama; uygulama sonrasi cargo check + cargo test.\nBelirsizlikler: {}",
                self.progress(phase, verified),
                bounded(&self.goal, 240),
                if self.targets.is_empty() {
                    self.required_targets.join(", ")
                } else {
                    self.targets
                        .iter()
                        .map(|target| {
                            format!(
                                "{}:{} ({})",
                                target.operation.label(),
                                target.path,
                                target.purpose
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                },
                self.inspected_files.join(", "),
                self.uncertainties.join(" ")
            ),
            MAX_REPORT_CHARS,
        )
    }
}

fn normalized_path(path: &str) -> Result<String, String> {
    if path.len() > MAX_PATH_BYTES {
        return Err("Plan hedef yolu 1024 byte sinirini asiyor.".into());
    }
    check_relative_path(path)?;
    let path = path.replace('\\', "/");
    let path = Path::new(&path)
        .components()
        .filter_map(|component| {
            if let Component::Normal(name) = component {
                Some(name.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() {
        return Err("Plan hedefi bir dosya yolu olmali.".into());
    }
    Ok(path)
}

fn path_key(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

fn validate_targets(targets: &[PlanTarget]) -> Result<(), String> {
    if targets.is_empty() || targets.len() > MAX_WORKSPACE_FILES {
        return Err(format!("Plan 1-{MAX_WORKSPACE_FILES} hedef icermeli."));
    }
    let mut paths = HashSet::new();
    for target in targets {
        let path = normalized_path(&target.path)?;
        if !paths.insert(path_key(&path)) {
            return Err("Ayni plan hedefi icin cakisan islemler var.".into());
        }
        if target.purpose.chars().count() > 800 {
            return Err("Plan hedef amaci 800 karakter sinirini asiyor.".into());
        }
    }
    Ok(())
}

fn bounded(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let marker = " [... kisaltildi]";
    let mut out: String = text
        .chars()
        .take(limit.saturating_sub(marker.chars().count()))
        .collect();
    out.push_str(marker);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn eski_tek_dosyali_dosya_adi_istegi_tam_yol_kisitini_gevsetmemeli() {
        let mut plan = TaskPlan::new("main.rs icindeki hatayi duzelt");
        plan.prepare_candidate("apply_patch", &json!({"path": "src/main.rs"}))
            .unwrap();
        assert_eq!(plan.targets[0].path, "src/main.rs");
        assert!(
            TaskPlan::new("Modify src/main.rs.")
                .prepare_candidate("apply_patch", &json!({"path":"src/nested/main.rs"}))
                .is_err()
        );
        assert!(
            TaskPlan::new("Modify main.rs.")
                .prepare_candidate(
                    "verify_workspace",
                    &json!({"changes":[{"operation":"edit","path":"src/main.rs"}]})
                )
                .is_err()
        );
    }

    fn three_targets() -> Value {
        json!({"changes": [
            {"operation": "edit", "path": "src/math.rs", "old_text": "eski", "new_text": "yeni"},
            {"operation": "create", "path": "src/format.rs", "content": "icerik"},
            {"operation": "edit", "path": "src/lib.rs", "old_text": "eski", "new_text": "yeni"}
        ]})
    }

    #[test]
    fn uc_dosyali_plan_yapilandirilmis_islemleri_tasimali() {
        let mut plan =
            TaskPlan::new("Modify src/math.rs, create src/format.rs and update src/lib.rs.");
        plan.prepare_candidate("verify_workspace", &three_targets())
            .unwrap();
        assert_eq!(plan.targets.len(), 3);
        assert_eq!(plan.targets[0].operation, PlanOperation::Edit);
        assert_eq!(plan.targets[1].operation, PlanOperation::Create);
        assert_eq!(
            plan.required_targets,
            ["src/math.rs", "src/format.rs", "src/lib.rs"]
        );
        assert!(
            plan.constraints
                .contains(&PlanConstraint::ExplicitApprovalRequired)
        );
        let same = plan.clone();
        plan.prepare_candidate("verify_workspace", &three_targets())
            .unwrap();
        assert_eq!(plan, same, "Ayni kapsam yinelenen plan uretmemeli");
    }

    #[test]
    fn tek_dosya_ve_yeni_dosya_planlari_eski_araclarla_kurulmali() {
        let mut single = TaskPlan::new("Modify src/lib.rs.");
        single
            .prepare_candidate(
                "verify_candidate",
                &json!({"path": "src/lib.rs", "old_text": "a", "new_text": "b"}),
            )
            .unwrap();
        assert_eq!(single.targets[0].operation, PlanOperation::Edit);
        let mut create = TaskPlan::new("Create README.md.");
        create.prepare_candidate("apply_workspace", &json!({"changes": [{"operation": "create", "path": "README.md", "content": "yeni"}]})).unwrap();
        assert_eq!(create.targets[0].operation, PlanOperation::Create);
    }

    #[test]
    fn eksik_ve_gecersiz_plan_onceki_plani_degistirmemeli() {
        let mut plan = TaskPlan::new("Modify src/math.rs and src/lib.rs.");
        plan.prepare_candidate("verify_workspace", &three_targets())
            .unwrap();
        let before = plan.clone();
        for args in [
            json!({"changes": []}),
            json!({"changes": [{"operation": "edit", "path": "src/lib.rs"}]}),
            json!({"changes": [{"operation": "delete", "path": "src/lib.rs"}]}),
            json!({"changes": [{"operation": "edit", "path": "../secret.rs"}]}),
            json!({"changes": [{"operation": "create", "path": "src/main_v05_backup.txt"}]}),
            json!({"changes": [{"operation": "edit", "path": "src/lib.rs"}, {"operation": "create", "path": "./src/lib.rs"}]}),
        ] {
            assert!(
                plan.prepare_candidate("verify_workspace", &args).is_err(),
                "{args}"
            );
            assert_eq!(plan, before);
        }
    }

    #[test]
    fn kapsam_eslesmesi_semantik_veya_dogrulama_kaniti_degildir() {
        let mut plan = TaskPlan::new("Add an email validation function and tests.");
        assert!(plan.required_targets.is_empty());
        assert!(plan.uncertainties.len() >= 2);
        plan.prepare_candidate(
            "verify_candidate",
            &json!({"path": "src/lib.rs", "old_text": "eski", "new_text": "yanlis is mantigi"}),
        )
        .unwrap();
        let text = plan.report(TaskPhase::Planning, false);
        assert!(text.contains("ReadyForVerification"));
        assert!(text.contains("kanitlamaz"));
        assert!(!text.contains("Completed"));
        assert!(
            !text.contains("yanlis is mantigi"),
            "Kaynak kodu plana kopyalanmamali"
        );
    }

    #[test]
    fn plan_ilerlemesi_yalniz_mevcut_rust_asamasindan_turetilmeli() {
        let plan = TaskPlan::new("Modify src/lib.rs.");
        assert_eq!(
            plan.progress(TaskPhase::Created, false),
            PlanProgress::Planned
        );
        assert_eq!(
            plan.progress(TaskPhase::Inspecting, false),
            PlanProgress::Inspecting
        );
        assert_eq!(
            plan.progress(TaskPhase::Planning, true),
            PlanProgress::Verified
        );
        assert_eq!(
            plan.progress(TaskPhase::AwaitingApproval, true),
            PlanProgress::AwaitingApproval
        );
        assert_eq!(
            plan.progress(TaskPhase::Applying, true),
            PlanProgress::Applying
        );
        assert_eq!(plan.progress(TaskPhase::Failed, true), PlanProgress::Failed);
        assert_eq!(
            plan.progress(TaskPhase::Blocked, true),
            PlanProgress::Blocked
        );
    }

    #[test]
    fn kapsam_uyusmazligi_ve_asiri_hedef_sayisi_sessizce_gecmemeli() {
        let plan = TaskPlan::new("Modify src/math.rs, src/format.rs and src/lib.rs.");
        let targets =
            TaskPlan::candidate_targets("verify_candidate", &json!({"path": "src/lib.rs"}))
                .unwrap();
        assert!(
            plan.check_scope(&targets)
                .unwrap_err()
                .contains("src/math.rs")
        );
        let request = format!(
            "Modify {}.",
            (0..17)
                .map(|i| format!("src/file_{i}.rs"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let plan = TaskPlan::new(&request);
        assert!(plan.check_scope(&targets).is_err());
    }

    #[test]
    fn inceleme_ve_model_notu_sinirli_ve_yolsal_olarak_guvenli_olmali() {
        let mut plan = TaskPlan::new(&format!(
            "Modify src/lib.rs. {}",
            "uzun hedef ".repeat(1000)
        ));
        plan.inspect("src/lib.rs");
        plan.inspect("./src/lib.rs");
        plan.inspect("../secret.rs");
        plan.inspect(".env");
        assert_eq!(plan.inspected_files, ["src/lib.rs"]);
        assert!(plan.note().chars().count() <= MAX_NOTE_CHARS);
        assert!(plan.report(TaskPhase::Inspecting, false).chars().count() <= MAX_REPORT_CHARS);
        assert!(plan.goal.chars().count() <= MAX_GOAL_CHARS);
        assert!(
            plan.note()
                .contains("Plan izin veya basari kaniti degildir")
        );
    }
}
