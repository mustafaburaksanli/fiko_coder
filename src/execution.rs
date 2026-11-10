//! v0.9 Execution State Machine.
//!
//! Gorev durumu Rust tarafinda acikca tutulur. Gecisler yalnizca gercek arac
//! sonuclariyla (dogrulama raporu, kullanici onayi, dosya yazimi) yapilir;
//! modelin metin olarak "tamamlandi" veya "PATCH_VERIFIED" demesi durumu
//! degistirmez, cunku bu modulun metin kabul eden bir girisi yoktur.
//!
//! Asamalar:
//! - Created: gorev alindi, henuz arac cagrisi yok
//! - Inspecting: salt okunur araclar (read_file, read_lines, search_code, list_files, run_tests)
//! - Planning: aday hazir ya da yeni aday bekleniyor (VERIFY_OK / VERIFY_FAILED sonrasi)
//! - Verifying: aday gecici kopyada derleniyor
//! - AwaitingApproval: dogrulanmis aday icin kullanici onayi isteniyor
//! - Applying: onaylandi; yedek + yazma + gercek projede cargo check/test
//! - Completed / Blocked / Failed: son durumlar (geri donus yok)

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskPhase {
    Created,
    Inspecting,
    Planning,
    Verifying,
    AwaitingApproval,
    Applying,
    Completed,
    Blocked,
    Failed,
}

impl TaskPhase {
    pub fn label(self) -> &'static str {
        match self {
            TaskPhase::Created => "Created",
            TaskPhase::Inspecting => "Inspecting",
            TaskPhase::Planning => "Planning",
            TaskPhase::Verifying => "Verifying",
            TaskPhase::AwaitingApproval => "AwaitingApproval",
            TaskPhase::Applying => "Applying",
            TaskPhase::Completed => "Completed",
            TaskPhase::Blocked => "Blocked",
            TaskPhase::Failed => "Failed",
        }
    }

    /// Son durumlar: gorev bitti, baska gecis yok.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskPhase::Completed | TaskPhase::Blocked | TaskPhase::Failed
        )
    }

    /// Izinli gecis tablosu. Tablo, araclarin gercek akisini yansitir:
    /// onay yalnizca dogrulamadan sonra, yazma yalnizca onaydan sonra gelir.
    pub fn can_enter(self, next: TaskPhase) -> bool {
        use TaskPhase::*;
        match self {
            Created => matches!(
                next,
                Inspecting | Planning | Verifying | Blocked | Completed | Failed
            ),
            Inspecting | Planning => matches!(
                next,
                Inspecting | Planning | Verifying | AwaitingApproval | Blocked | Completed | Failed
            ),
            Verifying => matches!(next, Planning | AwaitingApproval | Blocked | Failed),
            AwaitingApproval => matches!(next, Applying | Blocked),
            Applying => matches!(next, Completed | Planning | Failed),
            Completed | Blocked | Failed => false,
        }
    }
}

impl fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: TaskPhase,
    pub to: TaskPhase,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} -> {} gecisi izinli degil", self.from, self.to)
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionState {
    phase: TaskPhase,
    trace: Vec<TaskPhase>,
    rejected: Vec<TransitionError>,
}

impl Default for ExecutionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionState {
    pub fn new() -> Self {
        Self {
            phase: TaskPhase::Created,
            trace: vec![TaskPhase::Created],
            rejected: Vec::new(),
        }
    }

    pub fn phase(&self) -> TaskPhase {
        self.phase
    }

    pub fn is_terminal(&self) -> bool {
        self.phase.is_terminal()
    }

    /// Gecisi uygular. Ayni asamaya tekrar girmek (Inspecting -> Inspecting)
    /// izi sisirmez. Izinsiz gecis uygulanmaz, kaydedilir ve hata doner.
    pub fn enter(&mut self, next: TaskPhase) -> Result<(), TransitionError> {
        if next == self.phase && !next.is_terminal() {
            return Ok(());
        }
        if !self.phase.can_enter(next) {
            let error = TransitionError {
                from: self.phase,
                to: next,
            };
            self.rejected.push(error.clone());
            return Err(error);
        }
        self.phase = next;
        self.trace.push(next);
        Ok(())
    }

    pub fn trace(&self) -> &[TaskPhase] {
        &self.trace
    }

    pub fn rejected_transitions(&self) -> &[TransitionError] {
        &self.rejected
    }

    /// "Created -> Inspecting -> Verifying -> ..." bicimindeki iz.
    pub fn trace_line(&self) -> String {
        self.trace
            .iter()
            .map(|phase| phase.label())
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tam_onarim_akisi_izlenmeli() {
        let mut state = ExecutionState::new();
        for phase in [
            TaskPhase::Inspecting,
            TaskPhase::Inspecting,
            TaskPhase::Verifying,
            TaskPhase::Planning,
            TaskPhase::AwaitingApproval,
            TaskPhase::Applying,
            TaskPhase::Completed,
        ] {
            state.enter(phase).unwrap();
        }
        assert_eq!(state.phase(), TaskPhase::Completed);
        assert!(state.is_terminal());
        assert_eq!(
            state.trace_line(),
            "Created -> Inspecting -> Verifying -> Planning -> AwaitingApproval -> Applying -> Completed"
        );
        assert!(state.rejected_transitions().is_empty());
    }

    #[test]
    fn son_durumdan_cikis_olmamali() {
        for terminal in [TaskPhase::Completed, TaskPhase::Blocked, TaskPhase::Failed] {
            let mut state = ExecutionState::new();
            state.enter(terminal).unwrap();
            for next in [
                TaskPhase::Created,
                TaskPhase::Inspecting,
                TaskPhase::Planning,
                TaskPhase::Verifying,
                TaskPhase::AwaitingApproval,
                TaskPhase::Applying,
                TaskPhase::Completed,
                TaskPhase::Blocked,
                TaskPhase::Failed,
            ] {
                assert!(state.enter(next).is_err(), "{terminal} -> {next}");
                assert_eq!(state.phase(), terminal);
            }
        }
    }

    #[test]
    fn onay_ve_yazma_sirasi_atlanamamali() {
        let mut state = ExecutionState::new();
        // Dogrulama olmadan onay/yazma asamasina gecilemez.
        assert!(state.enter(TaskPhase::AwaitingApproval).is_err());
        assert!(state.enter(TaskPhase::Applying).is_err());
        state.enter(TaskPhase::Verifying).unwrap();
        assert!(state.enter(TaskPhase::Applying).is_err());
        assert!(state.enter(TaskPhase::Inspecting).is_err());
        state.enter(TaskPhase::AwaitingApproval).unwrap();
        // Onay beklerken yeniden dogrulamaya donulemez; yalnizca uygula ya da engelle.
        assert!(state.enter(TaskPhase::Verifying).is_err());
        state.enter(TaskPhase::Blocked).unwrap();
        assert_eq!(state.rejected_transitions().len(), 5);
    }

    #[test]
    fn geri_alma_sonrasi_yeni_deneme_mumkun_olmali() {
        let mut state = ExecutionState::new();
        state.enter(TaskPhase::Verifying).unwrap();
        state.enter(TaskPhase::Planning).unwrap();
        state.enter(TaskPhase::AwaitingApproval).unwrap();
        state.enter(TaskPhase::Applying).unwrap();
        // Gercek projede test dustu, geri alindi, kullanici yeni deneme istedi.
        state.enter(TaskPhase::Planning).unwrap();
        state.enter(TaskPhase::Verifying).unwrap();
        state.enter(TaskPhase::Failed).unwrap();
        assert_eq!(state.trace().len(), 8);
    }
}
