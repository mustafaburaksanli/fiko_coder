//! PostgreSQL hafizasi gozlem destegidir; patch/onay/dogrulama otoritesi degildir.

use std::path::PathBuf;

pub(crate) const CONTEXT_PREFIX: &str = "[FIKO HAFIZA VERISI]\n";

/// Sunucu yaniti ulasmasa bile hafiza mevcut agent akisina sinirsiz bekleme eklemez.
pub(crate) async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, MemoryError>>,
) -> Result<T, MemoryError> {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .map_err(|_| MemoryError::Unavailable)?
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub root_key: String,
    pub root_display: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMemory {
    pub path: String,
    /// None yalnizca dosyanin artik bulunmadigi gozlem icindir.
    pub sha256: Option<String>,
    pub bytes: i64,
    pub present: bool,
    pub symbols: Vec<String>,
    pub related_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryFileRole {
    Observed,
    Target,
}

impl MemoryFileRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Target => "target",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFileMemory {
    pub snapshot: FileMemory,
    pub role: MemoryFileRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStart {
    pub intent: String,
    pub summary: String,
    pub topics: Vec<String>,
    pub target_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMemory {
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFinish {
    pub phase: String,
    /// not_applied | verified | rolled_back | uncertain
    pub outcome: String,
    pub applied: Option<bool>,
    pub rolled_back: Option<bool>,
    pub commands: Vec<CommandMemory>,
    pub files: Vec<FileMemory>,
    pub target_paths: Vec<String>,
    /// Yalniz Rust uygulama kaydi. None: eski/eksik kanit; Some([]): yazim yok.
    pub modified_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery {
    pub intent: String,
    pub topics: Vec<String>,
    pub paths: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidate {
    pub project_id: i64,
    pub task_id: i64,
    pub intent: String,
    pub phase: String,
    pub outcome: String,
    pub applied: Option<bool>,
    pub rolled_back: Option<bool>,
    pub summary: String,
    pub topics: Vec<String>,
    pub files: Vec<TaskFileMemory>,
    pub modified_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    NotConfigured,
    Unavailable,
    SchemaMissing,
    SchemaConflict,
    InvalidData,
    Capacity,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotConfigured => "FIKO_DATABASE_URL ayarli degil; kalici hafiza devre disi",
            Self::Unavailable => {
                "PostgreSQL baglantisi/islemi kullanilamiyor (baglanti bilgisi gizlendi)"
            }
            Self::SchemaMissing => {
                "Hafiza semasi hazir veya guncel degil; --memory-migrate komutunu acikca calistirin"
            }
            Self::SchemaConflict => {
                "Hafiza semasi/surumu beklenenle uyusmuyor; kullanici verisine dokunulmadi"
            }
            Self::InvalidData => "Hafiza verisi guvenlik veya boyut denetiminden gecmedi",
            Self::Capacity => "Hafiza kotasi dolu; otomatik silme yapilmadi",
        })
    }
}

impl std::error::Error for MemoryError {}

/// DB URL'si hicbir Debug/Display ciktisina tasinmaz.
pub struct MemoryConfig {
    pub(crate) url: String,
    pub max_records: usize,
    pub max_bytes: usize,
}

impl MemoryConfig {
    pub fn new(url: String) -> Self {
        Self {
            url,
            max_records: 4,
            max_bytes: 4096,
        }
    }
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("FIKO_DATABASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())?;
        let number = |name: &str, default: usize, ceiling: usize| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(default)
                .min(ceiling)
        };
        Some(Self {
            url,
            max_records: number("FIKO_MEMORY_MAX_RECORDS", 4, 16),
            max_bytes: number("FIKO_MEMORY_MAX_BYTES", 4096, 16_384),
        })
    }
}

pub struct ActiveMemoryTask {
    pub id: i64,
    pub paths: std::collections::BTreeSet<String>,
    pub generate: bool,
}

pub struct MemoryRuntime {
    pub root: PathBuf,
    pub project: ProjectIdentity,
    pub project_id: i64,
    pub store: crate::memory_store::PgMemoryStore,
    pub max_records: usize,
    pub max_bytes: usize,
    pub active: Option<ActiveMemoryTask>,
}

/// Yalniz CLI'nin acik bakim komutundan cagrilir; normal baglanti DDL calistirmaz.
pub async fn migrate_configured() -> Result<(), MemoryError> {
    let config = MemoryConfig::from_env().ok_or(MemoryError::NotConfigured)?;
    bounded(crate::memory_store::PgMemoryStore::migrate(&config.url)).await
}

pub async fn check_configured() -> Result<(), MemoryError> {
    let config = MemoryConfig::from_env().ok_or(MemoryError::NotConfigured)?;
    bounded(async {
        let store = crate::memory_store::PgMemoryStore::connect(&config.url).await?;
        let result = store.health().await;
        store.close().await;
        result
    })
    .await
}

impl MemoryRuntime {
    pub async fn connect(root: PathBuf, config: MemoryConfig) -> Result<Self, MemoryError> {
        let project = crate::memory_retrieval::project_identity(&root)?;
        let store = crate::memory_store::PgMemoryStore::connect(&config.url).await?;
        let project_id = store.register_project(&project).await?;
        Ok(Self {
            root,
            project,
            project_id,
            store,
            max_records: config.max_records.min(16),
            max_bytes: config.max_bytes.min(16_384),
            active: None,
        })
    }

    pub async fn begin(
        &mut self,
        request: &str,
        intent: crate::intent::TaskIntent,
        paths: &[String],
    ) -> Result<crate::memory_retrieval::MemoryContext, MemoryError> {
        use crate::memory_retrieval as retrieval;
        let start = retrieval::safe_task_start(request, &intent.to_string(), paths);
        let id = self.store.begin_task(self.project_id, &start).await?;
        self.active = Some(ActiveMemoryTask {
            id,
            paths: start.target_paths.iter().cloned().collect(),
            generate: intent == crate::intent::TaskIntent::Generate,
        });
        // Generate kaynak gozlemi veya hafiza icin bile dosya okumaz.
        if intent == crate::intent::TaskIntent::Generate {
            return Ok(crate::memory_retrieval::MemoryContext::default());
        }
        let query = retrieval::query(request, &intent.to_string(), paths, self.max_records);
        let candidates = self.store.retrieve(self.project_id, &query).await?;
        Ok(retrieval::context(
            &self.root,
            self.project_id,
            &query,
            &candidates,
            self.max_records,
            self.max_bytes.saturating_sub(CONTEXT_PREFIX.len()),
        ))
    }

    pub fn observe_tool_paths(&mut self, name: &str, args: &serde_json::Value) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.generate {
            return;
        }
        let mut add = |path: &str| {
            if active.paths.len() < 32
                && let Ok(path) = crate::memory_retrieval::safe_path(path)
            {
                active.paths.insert(path);
            }
        };
        if matches!(
            name,
            "read_file" | "read_lines" | "search_in_file" | "verify_candidate" | "apply_patch"
        ) {
            if let Some(path) = args["path"].as_str() {
                add(path);
            }
        } else if matches!(name, "verify_workspace" | "apply_workspace")
            && let Some(changes) = args["changes"].as_array()
        {
            for change in changes.iter().take(16) {
                if let Some(path) = change["path"].as_str() {
                    add(path);
                }
            }
        }
    }

    pub async fn finish(&mut self, mut finish: TaskFinish) -> Result<(), MemoryError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        finish.target_paths = finish
            .target_paths
            .iter()
            .filter_map(|p| crate::memory_retrieval::safe_path(p).ok())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take(32)
            .collect();
        finish.files.clear();
        // Hafizanin desteklemedigi bir yazim yolu varsa listeyi eksiksizmis gibi daraltma.
        finish.modified_paths = finish.modified_paths.and_then(|paths| {
            paths
                .iter()
                .map(|path| crate::memory_retrieval::safe_path(path))
                .collect::<Result<std::collections::BTreeSet<_>, _>>()
                .ok()
                .map(|paths| paths.into_iter().collect())
        });
        if !active.generate {
            // Kaynak govdeleri depolanmaz. Son disk durumu, rollback DAHIL yeniden gozlenir.
            let paths: std::collections::BTreeSet<_> = finish
                .target_paths
                .iter()
                .cloned()
                .chain(active.paths)
                .collect();
            let mut remaining = 16 * 1024 * 1024;
            for path in paths.into_iter().take(32) {
                if let Ok(file) =
                    crate::memory_retrieval::observe_file_bounded(&self.root, &path, &mut remaining)
                {
                    finish.files.push(file);
                }
            }
        }
        self.store
            .finish_task(self.project_id, active.id, &finish)
            .await
    }
}

impl TaskFinish {
    pub fn interrupted() -> Self {
        Self {
            phase: "Interrupted".into(),
            outcome: "uncertain".into(),
            applied: None,
            rolled_back: None,
            commands: vec![],
            files: vec![],
            target_paths: vec![],
            modified_paths: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hafiza_yanit_vermezse_agent_sinirsiz_beklememeli() {
        let result = bounded(std::future::pending::<Result<(), MemoryError>>()).await;
        assert_eq!(result, Err(MemoryError::Unavailable));
    }

    #[test]
    fn kesilen_gorev_uygulanmadi_veya_basarili_olarak_uydurulmamali() {
        let finish = TaskFinish::interrupted();
        assert_eq!(finish.phase, "Interrupted");
        assert_eq!(finish.outcome, "uncertain");
        assert_eq!(finish.applied, None);
        assert_eq!(finish.rolled_back, None);
        assert!(finish.commands.is_empty());
    }
}
