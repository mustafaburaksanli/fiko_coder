-- Yalnizca acik --memory-migrate islemi tarafindan yeni fiko_memory semasinda calisir.
CREATE TABLE fiko_memory.schema_version (
    version integer PRIMARY KEY CHECK (version > 0),
    checksum text NOT NULL CHECK (checksum ~ '^[0-9a-f]{64}$'),
    applied_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE fiko_memory.projects (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    root_key text NOT NULL UNIQUE CHECK (root_key ~ '^[0-9a-f]{64}$'),
    root_display text NOT NULL CHECK (octet_length(root_display) BETWEEN 1 AND 2048),
    kind text NOT NULL CHECK (kind IN ('cargo', 'unsupported')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE fiko_memory.project_files (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id bigint NOT NULL REFERENCES fiko_memory.projects(id),
    relative_path text NOT NULL CHECK (octet_length(relative_path) BETWEEN 1 AND 1024),
    sha256 text CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    byte_len bigint NOT NULL CHECK (byte_len BETWEEN 0 AND 8388608),
    present boolean NOT NULL,
    symbols text[] NOT NULL DEFAULT '{}' CHECK (cardinality(symbols) <= 8 AND octet_length(array_to_string(symbols, '')) <= 512),
    related_paths text[] NOT NULL DEFAULT '{}' CHECK (cardinality(related_paths) <= 8 AND octet_length(array_to_string(related_paths, '')) <= 8192),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, relative_path),
    UNIQUE (project_id, id),
    CHECK ((present AND sha256 IS NOT NULL) OR (NOT present AND sha256 IS NULL AND byte_len = 0 AND cardinality(symbols) = 0 AND cardinality(related_paths) = 0))
);

CREATE TABLE fiko_memory.task_history (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project_id bigint NOT NULL REFERENCES fiko_memory.projects(id),
    intent text NOT NULL CHECK (intent IN ('Analyze', 'Explain', 'Generate', 'Modify', 'Test', 'Unknown')),
    summary text NOT NULL CHECK (octet_length(summary) <= 512),
    topics text[] NOT NULL DEFAULT '{}' CHECK (cardinality(topics) <= 16 AND octet_length(array_to_string(topics, '')) <= 1024),
    target_paths text[] NOT NULL DEFAULT '{}' CHECK (cardinality(target_paths) <= 32 AND octet_length(array_to_string(target_paths, '')) <= 32768),
    phase text NOT NULL CHECK (phase IN ('Running', 'Completed', 'Failed', 'Blocked', 'Rejected', 'Interrupted')),
    outcome text NOT NULL CHECK (outcome IN ('not_applied', 'verified', 'rolled_back', 'uncertain')),
    applied boolean,
    rolled_back boolean,
    commands jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(commands) = 'array' AND jsonb_array_length(commands) <= 16 AND octet_length(commands::text) <= 8192),
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE (project_id, id),
    CHECK ((phase = 'Running' AND finished_at IS NULL) OR (phase <> 'Running' AND finished_at IS NOT NULL)),
    CHECK (outcome <> 'verified' OR (phase = 'Completed' AND applied IS TRUE AND rolled_back IS FALSE)),
    CHECK (outcome <> 'rolled_back' OR (applied IS TRUE AND rolled_back IS TRUE)),
    CHECK (outcome <> 'not_applied' OR applied IS FALSE),
    CHECK (outcome <> 'uncertain' OR applied IS NULL)
);

-- Snapshot alanlari eski bir gorevin hash'ini sonraki gozlemle degistirmez.
CREATE TABLE fiko_memory.task_files (
    project_id bigint NOT NULL,
    task_id bigint NOT NULL,
    file_id bigint NOT NULL,
    role text NOT NULL CHECK (role IN ('target', 'observed')),
    sha256 text CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    byte_len bigint NOT NULL CHECK (byte_len BETWEEN 0 AND 8388608),
    present boolean NOT NULL,
    symbols text[] NOT NULL DEFAULT '{}' CHECK (cardinality(symbols) <= 8 AND octet_length(array_to_string(symbols, '')) <= 512),
    related_paths text[] NOT NULL DEFAULT '{}' CHECK (cardinality(related_paths) <= 8 AND octet_length(array_to_string(related_paths, '')) <= 8192),
    PRIMARY KEY (project_id, task_id, file_id),
    FOREIGN KEY (project_id, task_id) REFERENCES fiko_memory.task_history(project_id, id),
    FOREIGN KEY (project_id, file_id) REFERENCES fiko_memory.project_files(project_id, id),
    CHECK ((present AND sha256 IS NOT NULL) OR (NOT present AND sha256 IS NULL AND byte_len = 0 AND cardinality(symbols) = 0 AND cardinality(related_paths) = 0))
);

CREATE INDEX task_history_project_recent ON fiko_memory.task_history(project_id, id DESC);
CREATE INDEX task_history_summary_search ON fiko_memory.task_history USING gin(to_tsvector('simple', summary));
CREATE INDEX task_history_topics ON fiko_memory.task_history USING gin(topics);
CREATE INDEX task_history_targets ON fiko_memory.task_history USING gin(target_paths);
