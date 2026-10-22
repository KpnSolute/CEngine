-- Historical-migration provenance (SOP section 5), kept with the tenant in its installation schema.
-- Safe metadata only: counts, hashes and codes, never customer payloads.

CREATE TABLE "__SCHEMA__"."migration_runs" (
    id                   uuid        PRIMARY KEY,
    tenant_id            uuid        NOT NULL,
    installation_id      text        NOT NULL,
    source_system        text        NOT NULL,
    source_project_ref   text        NOT NULL,
    source_snapshot_at   timestamptz,
    source_snapshot_hash text,
    migration_version    text        NOT NULL,
    migration_code_commit text,
    started_at           timestamptz NOT NULL DEFAULT now(),
    completed_at         timestamptz,
    status               text        NOT NULL DEFAULT 'planned'
        CHECK (status IN ('planned', 'running', 'reconciling', 'passed', 'failed', 'cancelled')),
    source_record_count  bigint      NOT NULL DEFAULT 0,
    target_record_count  bigint      NOT NULL DEFAULT 0,
    rejected_record_count bigint     NOT NULL DEFAULT 0,
    initiated_by         text        NOT NULL,
    approved_by          text,
    notes_safe           text
);
CREATE INDEX "migration_runs_tenant_idx" ON "__SCHEMA__"."migration_runs" (tenant_id, started_at);

CREATE TABLE "__SCHEMA__"."migration_id_map" (
    tenant_id        uuid        NOT NULL,
    migration_run_id uuid        NOT NULL REFERENCES "__SCHEMA__"."migration_runs" (id),
    source_system    text        NOT NULL,
    source_table     text        NOT NULL,
    source_id        text        NOT NULL,
    target_entity    text        NOT NULL,
    target_id        uuid        NOT NULL,
    source_parent_id text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    -- One source identity maps to one target identity for the whole lineage, across runs.
    PRIMARY KEY (tenant_id, source_system, source_table, source_id)
);
CREATE INDEX "migration_id_map_target_idx" ON "__SCHEMA__"."migration_id_map" (tenant_id, target_entity, target_id);
CREATE INDEX "migration_id_map_run_idx" ON "__SCHEMA__"."migration_id_map" (migration_run_id);

CREATE TABLE "__SCHEMA__"."migration_reconciliation_results" (
    id               bigserial   PRIMARY KEY,
    tenant_id        uuid        NOT NULL,
    migration_run_id uuid        NOT NULL REFERENCES "__SCHEMA__"."migration_runs" (id),
    check_key        text        NOT NULL,
    scope_type       text        NOT NULL,
    scope_id         text        NOT NULL DEFAULT '',
    legacy_value     numeric,
    target_value     numeric,
    delta            numeric,
    tolerance        numeric     NOT NULL DEFAULT 0,
    status           text        NOT NULL CHECK (status IN ('pass', 'fail', 'skipped')),
    details_safe     text,
    checked_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX "migration_reconciliation_run_idx"
    ON "__SCHEMA__"."migration_reconciliation_results" (migration_run_id, check_key);

CREATE TABLE "__SCHEMA__"."migration_errors" (
    id                 bigserial   PRIMARY KEY,
    tenant_id          uuid        NOT NULL,
    migration_run_id   uuid        NOT NULL REFERENCES "__SCHEMA__"."migration_runs" (id),
    source_table       text        NOT NULL,
    source_id          text        NOT NULL,
    error_code         text        NOT NULL,
    safe_error_message text        NOT NULL,
    payload_hash       text,
    severity           text        NOT NULL CHECK (severity IN ('info', 'warning', 'error', 'fatal')),
    created_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX "migration_errors_run_idx" ON "__SCHEMA__"."migration_errors" (migration_run_id, severity);

CREATE TABLE "__SCHEMA__"."migration_artifacts" (
    id                    bigserial   PRIMARY KEY,
    tenant_id             uuid        NOT NULL,
    migration_run_id      uuid        NOT NULL REFERENCES "__SCHEMA__"."migration_runs" (id),
    artifact_kind         text        NOT NULL,
    path_or_object_ref    text        NOT NULL,
    sha256                text        NOT NULL,
    size_bytes            bigint      NOT NULL CHECK (size_bytes >= 0),
    created_at            timestamptz NOT NULL DEFAULT now(),
    expires_at            timestamptz,
    contains_customer_data boolean    NOT NULL
);
CREATE INDEX "migration_artifacts_run_idx" ON "__SCHEMA__"."migration_artifacts" (migration_run_id);
