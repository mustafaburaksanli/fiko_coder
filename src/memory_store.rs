//! Parametreli, proje kapsamli PostgreSQL deposu. Baglanti otomatik sema kurmaz.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgConnection, PgPool, Row};

use crate::memory::{
    CommandMemory, FileMemory, MemoryCandidate, MemoryError, MemoryFileRole, MemoryQuery,
    ProjectIdentity, TaskFileMemory, TaskFinish, TaskStart,
};
use crate::memory_retrieval::{safe_path, validate_file};

const MIGRATIONS: [&str; 2] = [
    include_str!("../migrations/0001_memory.sql"),
    include_str!("../migrations/0002_provenance.sql"),
];
const SCHEMA_LOCK: i64 = 6_074_618_810_000;
const PROJECT_LOCK_BASE: i64 = 6_074_618_820_000;
const MAX_PROJECTS: i64 = 128;
const MAX_FILES: i64 = 2048;
const MAX_TASKS: i64 = 512;
const MAX_TASK_FILES: usize = 32;

#[derive(Clone)]
pub struct PgMemoryStore {
    pool: PgPool,
}

fn database_error(error: sqlx::Error) -> MemoryError {
    match error {
        sqlx::Error::Database(error)
            if matches!(
                error.code().as_deref(),
                Some("23502" | "23503" | "23505" | "23514")
            ) =>
        {
            MemoryError::InvalidData
        }
        _ => MemoryError::Unavailable,
    }
}

fn checksum(migration: &str) -> String {
    // Ayni migration Windows CRLF ve Unix LF checkout'larinda ayni kimligi tasir.
    format!(
        "{:x}",
        Sha256::digest(migration.replace("\r\n", "\n").as_bytes())
    )
}

async fn open_pool(url: &str) -> Result<PgPool, MemoryError> {
    let options = PgConnectOptions::from_str(url)
        .map_err(|_| MemoryError::Unavailable)?
        .disable_statement_logging()
        .application_name("fiko_coder_memory")
        .options([("statement_timeout", "3000"), ("lock_timeout", "1500")]);
    tokio::time::timeout(
        Duration::from_secs(3),
        PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(3))
            .connect_with(options),
    )
    .await
    .map_err(|_| MemoryError::Unavailable)?
    .map_err(database_error)
}

async fn schema_exists(connection: &mut PgConnection) -> Result<bool, MemoryError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
        .bind("fiko_memory")
        .fetch_one(connection)
        .await
        .map_err(database_error)
}

fn validate_versions(versions: &[(i32, String)]) -> Result<usize, MemoryError> {
    if versions.is_empty() || versions.len() > MIGRATIONS.len() {
        return Err(MemoryError::SchemaConflict);
    }
    for (index, (version, actual_checksum)) in versions.iter().enumerate() {
        if *version != index as i32 + 1 || *actual_checksum != checksum(MIGRATIONS[index]) {
            return Err(MemoryError::SchemaConflict);
        }
    }
    Ok(versions.len())
}

async fn schema_versions(connection: &mut PgConnection) -> Result<usize, MemoryError> {
    if !schema_exists(connection).await? {
        return Err(MemoryError::SchemaMissing);
    }
    let rows = sqlx::query(
        "SELECT version, checksum FROM fiko_memory.schema_version ORDER BY version LIMIT 3",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|_| MemoryError::SchemaConflict)?;
    let versions: Result<Vec<(i32, String)>, MemoryError> = rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get("version")
                    .map_err(|_| MemoryError::SchemaConflict)?,
                row.try_get("checksum")
                    .map_err(|_| MemoryError::SchemaConflict)?,
            ))
        })
        .collect();
    validate_versions(&versions?)
}

async fn validate_tables(connection: &mut PgConnection) -> Result<(), MemoryError> {
    let complete: bool = sqlx::query_scalar(
        "SELECT to_regclass('fiko_memory.projects') IS NOT NULL \
         AND to_regclass('fiko_memory.project_files') IS NOT NULL \
         AND to_regclass('fiko_memory.task_history') IS NOT NULL \
         AND to_regclass('fiko_memory.task_files') IS NOT NULL",
    )
    .fetch_one(connection)
    .await
    .map_err(|_| MemoryError::SchemaConflict)?;
    if complete {
        Ok(())
    } else {
        Err(MemoryError::SchemaConflict)
    }
}

async fn validate_schema(connection: &mut PgConnection) -> Result<(), MemoryError> {
    let applied = schema_versions(connection).await?;
    validate_tables(connection).await?;
    if applied != MIGRATIONS.len() {
        return Err(MemoryError::SchemaMissing);
    }
    Ok(())
}

fn valid_intent(intent: &str) -> bool {
    [
        "Analyze", "Explain", "Generate", "Modify", "Test", "Unknown",
    ]
    .contains(&intent)
}

fn validate_paths(paths: &[String]) -> Result<(), MemoryError> {
    if paths.len() > MAX_TASK_FILES {
        return Err(MemoryError::InvalidData);
    }
    let mut unique = BTreeSet::new();
    for path in paths {
        if safe_path(path)? != *path || !unique.insert(path) {
            return Err(MemoryError::InvalidData);
        }
    }
    Ok(())
}

fn validate_topics(topics: &[String]) -> Result<(), MemoryError> {
    if topics.len() > 16
        || topics.iter().any(|topic| {
            topic.is_empty()
                || topic.len() > 64
                || !topic.chars().all(|c| c.is_alphanumeric() || c == '_')
        })
    {
        return Err(MemoryError::InvalidData);
    }
    Ok(())
}

fn commands_json(commands: &[CommandMemory]) -> Result<Value, MemoryError> {
    if commands.len() > 16 {
        return Err(MemoryError::InvalidData);
    }
    let mut values = Vec::with_capacity(commands.len());
    for command in commands {
        if !["cargo check", "cargo test", "cargo build"].contains(&command.command.as_str())
            || ![
                "succeeded",
                "failed",
                "execution_error",
                "timed_out",
                "interrupted",
            ]
            .contains(&command.status.as_str())
            || (command.status == "succeeded" && command.exit_code != Some(0))
            || (command.status == "failed" && !command.exit_code.is_some_and(|code| code != 0))
            || (["execution_error", "timed_out", "interrupted"].contains(&command.status.as_str())
                && command.exit_code.is_some())
        {
            return Err(MemoryError::InvalidData);
        }
        values.push(json!({"command": command.command, "status": command.status, "exit_code": command.exit_code}));
    }
    Ok(Value::Array(values))
}

fn validate_finish(finish: &TaskFinish) -> Result<Value, MemoryError> {
    if !["Completed", "Failed", "Blocked", "Rejected", "Interrupted"]
        .contains(&finish.phase.as_str())
        || finish.files.len() > MAX_TASK_FILES
    {
        return Err(MemoryError::InvalidData);
    }
    let valid_outcome = match finish.outcome.as_str() {
        "verified" => {
            finish.phase == "Completed"
                && finish.applied == Some(true)
                && finish.rolled_back == Some(false)
        }
        "not_applied" => finish.applied == Some(false) && finish.rolled_back == Some(false),
        "rolled_back" => finish.applied == Some(true) && finish.rolled_back == Some(true),
        "uncertain" => finish.applied.is_none(),
        _ => false,
    };
    if !valid_outcome {
        return Err(MemoryError::InvalidData);
    }
    validate_paths(&finish.target_paths)?;
    if let Some(paths) = &finish.modified_paths {
        validate_paths(paths)?;
    }
    let mut unique = BTreeSet::new();
    for file in &finish.files {
        validate_file(file)?;
        if !unique.insert(&file.path) {
            return Err(MemoryError::InvalidData);
        }
    }
    commands_json(&finish.commands)
}

async fn project_lock(connection: &mut PgConnection, project_id: i64) -> Result<(), MemoryError> {
    if project_id <= 0 || project_id > i64::MAX - PROJECT_LOCK_BASE {
        return Err(MemoryError::InvalidData);
    }
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(PROJECT_LOCK_BASE + project_id)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM fiko_memory.projects WHERE id = $1)")
            .bind(project_id)
            .fetch_one(connection)
            .await
            .map_err(database_error)?;
    if exists {
        Ok(())
    } else {
        Err(MemoryError::InvalidData)
    }
}

fn file_from_row(row: &sqlx::postgres::PgRow) -> Result<FileMemory, MemoryError> {
    let file = FileMemory {
        path: row
            .try_get("relative_path")
            .map_err(|_| MemoryError::InvalidData)?,
        sha256: row
            .try_get("sha256")
            .map_err(|_| MemoryError::InvalidData)?,
        bytes: row
            .try_get("byte_len")
            .map_err(|_| MemoryError::InvalidData)?,
        present: row
            .try_get("present")
            .map_err(|_| MemoryError::InvalidData)?,
        symbols: row
            .try_get("symbols")
            .map_err(|_| MemoryError::InvalidData)?,
        related_paths: row
            .try_get("related_paths")
            .map_err(|_| MemoryError::InvalidData)?,
    };
    validate_file(&file)?;
    Ok(file)
}

async fn task_files(
    connection: &mut PgConnection,
    project_id: i64,
    task_id: i64,
) -> Result<Vec<TaskFileMemory>, MemoryError> {
    let rows = sqlx::query(
        "SELECT f.relative_path, tf.role, tf.sha256, tf.byte_len, tf.present, tf.symbols, tf.related_paths \
         FROM fiko_memory.task_files tf JOIN fiko_memory.project_files f \
         ON f.project_id = tf.project_id AND f.id = tf.file_id \
         WHERE tf.project_id = $1 AND tf.task_id = $2 ORDER BY f.relative_path LIMIT 33",
    )
    .bind(project_id)
    .bind(task_id)
    .fetch_all(connection)
    .await
    .map_err(database_error)?;
    if rows.len() > MAX_TASK_FILES {
        return Err(MemoryError::InvalidData);
    }
    rows.iter()
        .map(|row| {
            let role = match row
                .try_get::<String, _>("role")
                .map_err(|_| MemoryError::InvalidData)?
                .as_str()
            {
                "observed" => MemoryFileRole::Observed,
                "target" => MemoryFileRole::Target,
                _ => return Err(MemoryError::InvalidData),
            };
            Ok(TaskFileMemory {
                snapshot: file_from_row(row)?,
                role,
            })
        })
        .collect()
}

impl PgMemoryStore {
    pub async fn connect(url: &str) -> Result<Self, MemoryError> {
        let pool = open_pool(url).await?;
        let result = {
            let mut connection = pool.acquire().await.map_err(database_error)?;
            validate_schema(&mut connection).await
        };
        if let Err(error) = result {
            pool.close().await;
            return Err(error);
        }
        Ok(Self { pool })
    }

    /// Sadece kullanicinin acik migration komutu bu metodu cagirir.
    pub async fn migrate(url: &str) -> Result<(), MemoryError> {
        let pool = open_pool(url).await?;
        let result = async {
            let mut transaction = pool.begin().await.map_err(database_error)?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(SCHEMA_LOCK)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            let applied = if schema_exists(&mut transaction).await? {
                let applied = schema_versions(&mut transaction).await?;
                validate_tables(&mut transaction).await?;
                applied
            } else {
                sqlx::query("CREATE SCHEMA fiko_memory")
                    .execute(&mut *transaction)
                    .await
                    .map_err(database_error)?;
                0
            };
            for (index, migration) in MIGRATIONS.iter().enumerate().skip(applied) {
                sqlx::raw_sql(migration)
                    .execute(&mut *transaction)
                    .await
                    .map_err(database_error)?;
                sqlx::query(
                    "INSERT INTO fiko_memory.schema_version(version, checksum) VALUES ($1, $2)",
                )
                .bind(index as i32 + 1)
                .bind(checksum(migration))
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            }
            validate_schema(&mut transaction).await?;
            transaction.commit().await.map_err(database_error)
        }
        .await;
        pool.close().await;
        result
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn health(&self) -> Result<(), MemoryError> {
        let mut connection = self.pool.acquire().await.map_err(database_error)?;
        validate_schema(&mut connection).await
    }

    pub async fn register_project(&self, project: &ProjectIdentity) -> Result<i64, MemoryError> {
        if project.root_key.len() != 64
            || !project
                .root_key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || project.root_display.is_empty()
            || project.root_display.len() > 2048
            || project.root_display.chars().any(char::is_control)
            || !["cargo", "unsupported"].contains(&project.kind.as_str())
        {
            return Err(MemoryError::InvalidData);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(SCHEMA_LOCK)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM fiko_memory.projects WHERE root_key = $1")
                .bind(&project.root_key)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(database_error)?;
        let id = if let Some(id) = existing {
            sqlx::query("UPDATE fiko_memory.projects SET root_display=$2,kind=$3,updated_at=now() WHERE id=$1")
                .bind(id).bind(&project.root_display).bind(&project.kind)
                .execute(&mut *transaction).await.map_err(database_error)?;
            id
        } else {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM fiko_memory.projects")
                .fetch_one(&mut *transaction)
                .await
                .map_err(database_error)?;
            if count >= MAX_PROJECTS {
                return Err(MemoryError::Capacity);
            }
            sqlx::query_scalar("INSERT INTO fiko_memory.projects(root_key, root_display, kind) VALUES ($1,$2,$3) RETURNING id")
                .bind(&project.root_key).bind(&project.root_display).bind(&project.kind)
                .fetch_one(&mut *transaction).await.map_err(database_error)?
        };
        transaction.commit().await.map_err(database_error)?;
        Ok(id)
    }

    pub async fn begin_task(&self, project_id: i64, start: &TaskStart) -> Result<i64, MemoryError> {
        if !valid_intent(&start.intent)
            || start.summary.len() > 512
            || start.summary.chars().any(char::is_control)
        {
            return Err(MemoryError::InvalidData);
        }
        validate_topics(&start.topics)?;
        validate_paths(&start.target_paths)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        project_lock(&mut transaction, project_id).await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM fiko_memory.task_history WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        if count >= MAX_TASKS {
            return Err(MemoryError::Capacity);
        }
        let id = sqlx::query_scalar(
            "INSERT INTO fiko_memory.task_history(project_id,intent,summary,topics,target_paths,phase,outcome,applied,rolled_back) \
             VALUES ($1,$2,$3,$4,$5,'Running','not_applied',false,false) RETURNING id",
        ).bind(project_id).bind(&start.intent).bind(&start.summary).bind(&start.topics).bind(&start.target_paths)
            .fetch_one(&mut *transaction).await.map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(id)
    }

    pub async fn finish_task(
        &self,
        project_id: i64,
        task_id: i64,
        finish: &TaskFinish,
    ) -> Result<(), MemoryError> {
        let commands = validate_finish(finish)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        project_lock(&mut transaction, project_id).await?;
        let row = sqlx::query("SELECT phase,outcome,applied,rolled_back,commands,target_paths,modified_paths FROM fiko_memory.task_history WHERE project_id=$1 AND id=$2 FOR UPDATE")
            .bind(project_id).bind(task_id).fetch_optional(&mut *transaction).await.map_err(database_error)?.ok_or(MemoryError::InvalidData)?;
        let phase: String = row.try_get("phase").map_err(|_| MemoryError::InvalidData)?;
        if phase != "Running" {
            let mut expected: Vec<TaskFileMemory> = finish
                .files
                .iter()
                .cloned()
                .map(|snapshot| {
                    let role = if finish.target_paths.contains(&snapshot.path) {
                        MemoryFileRole::Target
                    } else {
                        MemoryFileRole::Observed
                    };
                    TaskFileMemory { snapshot, role }
                })
                .collect();
            expected.sort_by(|a, b| a.snapshot.path.cmp(&b.snapshot.path));
            let equal = phase == finish.phase
                && row
                    .try_get::<String, _>("outcome")
                    .map_err(|_| MemoryError::InvalidData)?
                    == finish.outcome
                && row
                    .try_get::<Option<bool>, _>("applied")
                    .map_err(|_| MemoryError::InvalidData)?
                    == finish.applied
                && row
                    .try_get::<Option<bool>, _>("rolled_back")
                    .map_err(|_| MemoryError::InvalidData)?
                    == finish.rolled_back
                && row
                    .try_get::<Value, _>("commands")
                    .map_err(|_| MemoryError::InvalidData)?
                    == commands
                && row
                    .try_get::<Vec<String>, _>("target_paths")
                    .map_err(|_| MemoryError::InvalidData)?
                    == finish.target_paths
                && row
                    .try_get::<Option<Vec<String>>, _>("modified_paths")
                    .map_err(|_| MemoryError::InvalidData)?
                    == finish.modified_paths
                && task_files(&mut transaction, project_id, task_id).await? == expected;
            return if equal {
                transaction.commit().await.map_err(database_error)
            } else {
                Err(MemoryError::InvalidData)
            };
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM fiko_memory.project_files WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        let paths: Vec<&str> = finish.files.iter().map(|file| file.path.as_str()).collect();
        let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM fiko_memory.project_files WHERE project_id = $1 AND relative_path = ANY($2)")
            .bind(project_id).bind(&paths).fetch_one(&mut *transaction).await.map_err(database_error)?;
        if count + finish.files.len() as i64 - existing > MAX_FILES {
            return Err(MemoryError::Capacity);
        }
        for file in &finish.files {
            let file_id: i64 = sqlx::query_scalar(
                "INSERT INTO fiko_memory.project_files(project_id,relative_path,sha256,byte_len,present,symbols,related_paths) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(project_id,relative_path) DO UPDATE \
                 SET sha256=EXCLUDED.sha256,byte_len=EXCLUDED.byte_len,present=EXCLUDED.present,symbols=EXCLUDED.symbols,related_paths=EXCLUDED.related_paths,updated_at=now() RETURNING id",
            ).bind(project_id).bind(&file.path).bind(&file.sha256).bind(file.bytes).bind(file.present).bind(&file.symbols).bind(&file.related_paths)
                .fetch_one(&mut *transaction).await.map_err(database_error)?;
            let role = if finish.target_paths.contains(&file.path) {
                MemoryFileRole::Target
            } else {
                MemoryFileRole::Observed
            };
            sqlx::query("INSERT INTO fiko_memory.task_files(project_id,task_id,file_id,role,sha256,byte_len,present,symbols,related_paths) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(project_id).bind(task_id).bind(file_id).bind(role.label()).bind(&file.sha256).bind(file.bytes).bind(file.present).bind(&file.symbols).bind(&file.related_paths)
                .execute(&mut *transaction).await.map_err(database_error)?;
        }
        sqlx::query("UPDATE fiko_memory.task_history SET phase=$3,outcome=$4,applied=$5,rolled_back=$6,commands=$7,target_paths=$8,modified_paths=$9,finished_at=now() WHERE project_id=$1 AND id=$2 AND phase='Running'")
            .bind(project_id).bind(task_id).bind(&finish.phase).bind(&finish.outcome).bind(finish.applied).bind(finish.rolled_back).bind(commands).bind(&finish.target_paths).bind(&finish.modified_paths)
            .execute(&mut *transaction).await.map_err(database_error)?;
        transaction.commit().await.map_err(database_error)
    }

    pub async fn known_paths(
        &self,
        project_id: i64,
        limit: usize,
    ) -> Result<Vec<String>, MemoryError> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT relative_path FROM fiko_memory.project_files WHERE project_id=$1 AND present=true ORDER BY updated_at DESC,id DESC LIMIT $2")
            .bind(project_id).bind(limit.min(MAX_FILES as usize) as i64).fetch_all(&self.pool).await.map_err(database_error)?;
        validate_paths_in_result(&rows)?;
        Ok(rows)
    }

    pub async fn retrieve(
        &self,
        project_id: i64,
        query: &MemoryQuery,
    ) -> Result<Vec<MemoryCandidate>, MemoryError> {
        if !valid_intent(&query.intent) {
            return Err(MemoryError::InvalidData);
        }
        validate_topics(&query.topics)?;
        validate_paths(&query.paths)?;
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let words = query.topics.join(" ");
        let mut connection = self.pool.acquire().await.map_err(database_error)?;
        let rows = sqlx::query(
            "SELECT id,intent,summary,topics,phase,outcome,applied,rolled_back,modified_paths FROM fiko_memory.task_history t \
             WHERE project_id=$1 AND phase <> 'Running' AND \
             (target_paths && $2::text[] OR topics && $3::text[] \
              OR EXISTS (SELECT 1 FROM fiko_memory.task_files tf JOIN fiko_memory.project_files f \
                 ON f.project_id=tf.project_id AND f.id=tf.file_id \
                 WHERE tf.project_id=$1 AND tf.task_id=t.id \
                 AND (f.relative_path=ANY($2::text[]) OR regexp_replace(f.relative_path,'^.*/','')=ANY($2::text[]))) \
              OR ($4 <> '' AND to_tsvector('simple',summary) @@ plainto_tsquery('simple',$4))) \
             ORDER BY (target_paths && $2::text[]) DESC,(topics && $3::text[]) DESC,id DESC LIMIT $5",
        ).bind(project_id).bind(&query.paths).bind(&query.topics).bind(words).bind(query.limit.min(16) as i64)
            .fetch_all(&mut *connection).await.map_err(database_error)?;
        let mut candidates = Vec::with_capacity(rows.len());
        for row in rows {
            let task_id = row.try_get("id").map_err(|_| MemoryError::InvalidData)?;
            let modified_paths: Option<Vec<String>> = row
                .try_get("modified_paths")
                .map_err(|_| MemoryError::InvalidData)?;
            if let Some(paths) = &modified_paths {
                validate_paths(paths)?;
            }
            candidates.push(MemoryCandidate {
                project_id,
                task_id,
                intent: row
                    .try_get("intent")
                    .map_err(|_| MemoryError::InvalidData)?,
                phase: row.try_get("phase").map_err(|_| MemoryError::InvalidData)?,
                outcome: row
                    .try_get("outcome")
                    .map_err(|_| MemoryError::InvalidData)?,
                applied: row
                    .try_get("applied")
                    .map_err(|_| MemoryError::InvalidData)?,
                rolled_back: row
                    .try_get("rolled_back")
                    .map_err(|_| MemoryError::InvalidData)?,
                modified_paths,
                summary: row
                    .try_get("summary")
                    .map_err(|_| MemoryError::InvalidData)?,
                topics: row
                    .try_get("topics")
                    .map_err(|_| MemoryError::InvalidData)?,
                files: task_files(&mut connection, project_id, task_id).await?,
            });
        }
        Ok(candidates)
    }
}

fn validate_paths_in_result(paths: &[String]) -> Result<(), MemoryError> {
    for path in paths {
        if safe_path(path)? != *path {
            return Err(MemoryError::InvalidData);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_status_is_process_metadata_not_output_text() {
        assert!(
            commands_json(&[CommandMemory {
                command: "cargo test".into(),
                status: "succeeded".into(),
                exit_code: Some(0)
            }])
            .is_ok()
        );
        assert!(
            commands_json(&[CommandMemory {
                command: "cargo test".into(),
                status: "succeeded".into(),
                exit_code: Some(1)
            }])
            .is_err()
        );
        assert!(
            commands_json(&[CommandMemory {
                command: "cargo test; echo secret".into(),
                status: "failed".into(),
                exit_code: Some(1)
            }])
            .is_err()
        );
    }

    #[test]
    fn uncertain_or_rolled_back_is_never_verified_success() {
        let mut finish = TaskFinish {
            phase: "Failed".into(),
            outcome: "uncertain".into(),
            applied: None,
            rolled_back: None,
            commands: vec![],
            files: vec![],
            target_paths: vec![],
            modified_paths: None,
        };
        assert!(validate_finish(&finish).is_ok());
        finish.outcome = "verified".into();
        assert!(validate_finish(&finish).is_err());
        finish.outcome = "rolled_back".into();
        finish.applied = Some(true);
        finish.rolled_back = Some(true);
        assert!(validate_finish(&finish).is_ok());
    }

    #[test]
    fn path_and_metadata_limits_are_checked_before_sql() {
        assert!(validate_paths(&["../secret.rs".into()]).is_err());
        assert!(validate_paths(&["src/lib.rs".into(), "src/lib.rs".into()]).is_err());
        assert!(validate_topics(&["ignore previous instructions".into()]).is_err());
    }

    #[test]
    fn migration_identity_is_content_hash() {
        for migration in MIGRATIONS {
            assert_eq!(checksum(migration).len(), 64);
            assert!(!migration.contains("DROP "));
            assert!(!migration.contains("TRUNCATE "));
        }
    }

    #[test]
    fn migration_versions_must_be_known_checksum_prefix() {
        let first = (1, checksum(MIGRATIONS[0]));
        let second = (2, checksum(MIGRATIONS[1]));
        assert_eq!(validate_versions(std::slice::from_ref(&first)), Ok(1));
        assert_eq!(validate_versions(&[first.clone(), second.clone()]), Ok(2));
        assert_eq!(validate_versions(&[]), Err(MemoryError::SchemaConflict));
        assert_eq!(
            validate_versions(&[second]),
            Err(MemoryError::SchemaConflict)
        );
        assert_eq!(
            validate_versions(&[first.clone(), (2, "0".repeat(64))]),
            Err(MemoryError::SchemaConflict)
        );
        assert_eq!(
            validate_versions(&[first, (3, checksum(MIGRATIONS[1]))]),
            Err(MemoryError::SchemaConflict)
        );
    }

    #[test]
    fn modified_paths_are_not_inferred_from_declared_targets() {
        let mut finish = TaskFinish {
            phase: "Completed".into(),
            outcome: "not_applied".into(),
            applied: Some(false),
            rolled_back: Some(false),
            commands: vec![],
            files: vec![],
            target_paths: vec!["src/lib.rs".into()],
            modified_paths: Some(vec![]),
        };
        assert!(validate_finish(&finish).is_ok());
        assert_eq!(finish.modified_paths, Some(vec![]));
        finish.modified_paths = None;
        assert!(validate_finish(&finish).is_ok());
        finish.modified_paths = Some(vec!["src/lib.rs".into(), "src/lib.rs".into()]);
        assert!(validate_finish(&finish).is_err());
        finish.modified_paths = Some(vec!["../outside.rs".into()]);
        assert!(validate_finish(&finish).is_err());
    }
}
