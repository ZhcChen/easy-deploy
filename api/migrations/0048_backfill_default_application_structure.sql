-- Add migration script here.
INSERT INTO app_environments(
    app_id,
    environment_key,
    name,
    status,
    runtime_status,
    last_deployment_status,
    created_at,
    updated_at
)
SELECT
    apps.id,
    apps.environment,
    CASE apps.environment WHEN 'production' THEN '正式环境' ELSE '测试环境' END,
    CASE WHEN apps.status = 'draft' THEN 'configuring' WHEN apps.status = 'disabled' THEN 'disabled' ELSE 'ready' END,
    CASE
        WHEN EXISTS (SELECT 1 FROM app_runtime_states s WHERE s.app_id = apps.id AND s.runtime_status = 'deploying') THEN 'unknown'
        WHEN EXISTS (SELECT 1 FROM app_runtime_states s WHERE s.app_id = apps.id AND s.runtime_status = 'unhealthy') THEN 'partial_unhealthy'
        WHEN EXISTS (SELECT 1 FROM app_runtime_states s WHERE s.app_id = apps.id AND s.runtime_status = 'healthy') THEN 'running'
        WHEN EXISTS (SELECT 1 FROM app_runtime_states s WHERE s.app_id = apps.id AND s.runtime_status = 'stopped') THEN 'stopped'
        ELSE 'unknown'
    END,
    CASE
        WHEN apps.status = 'deploying' THEN 'all_failed'
        WHEN apps.status = 'failed' THEN 'all_failed'
        WHEN apps.status = 'running' THEN 'success'
        ELSE 'waiting'
    END,
    apps.created_at,
    apps.updated_at
FROM apps
WHERE NOT EXISTS (
    SELECT 1 FROM app_environments environments WHERE environments.app_id = apps.id
);

INSERT INTO app_environment_targets(environment_id, node_id, target_role, created_at)
SELECT environments.id, targets.node_id, targets.target_role, targets.created_at
FROM app_targets targets
JOIN app_environments environments ON environments.app_id = targets.app_id
WHERE NOT EXISTS (
    SELECT 1
    FROM app_environment_targets existing
    WHERE existing.environment_id = environments.id
      AND existing.node_id = targets.node_id
);

INSERT INTO deployment_units(
    app_id,
    unit_key,
    name,
    description,
    required,
    lifecycle_status,
    work_dir,
    created_at,
    updated_at
)
SELECT
    apps.id,
    'default',
    apps.name,
    '兼容迁移生成的默认部署单元',
    1,
    CASE WHEN apps.status = 'disabled' THEN 'disabled' ELSE 'active' END,
    apps.work_dir,
    apps.created_at,
    apps.updated_at
FROM apps
WHERE NOT EXISTS (
    SELECT 1 FROM deployment_units units WHERE units.app_id = apps.id
);

INSERT INTO deployment_pipeline_stages(
    app_id,
    stage_no,
    stage_key,
    name,
    stage_kind,
    created_at,
    updated_at
)
SELECT apps.id, 1, 'default', '默认部署阶段', 'units', apps.created_at, apps.updated_at
FROM apps
WHERE NOT EXISTS (
    SELECT 1 FROM deployment_pipeline_stages stages WHERE stages.app_id = apps.id
);

INSERT INTO deployment_pipeline_stage_units(stage_id, unit_id, unit_order, removal_order)
SELECT stages.id, units.id, 1, 1
FROM deployment_pipeline_stages stages
JOIN deployment_units units ON units.app_id = stages.app_id AND units.unit_key = 'default'
WHERE stages.stage_key = 'default'
  AND NOT EXISTS (
      SELECT 1
      FROM deployment_pipeline_stage_units existing
      WHERE existing.stage_id = stages.id AND existing.unit_id = units.id
  );

INSERT INTO app_config_revisions(
    app_id,
    revision_no,
    config_json,
    public_config_json,
    secret_fingerprints,
    config_hash,
    script_hash,
    published_by,
    created_at
)
SELECT
    apps.id,
    100,
    json_object(
        'legacy_snapshot_id', snapshots.id,
        'compose_content', COALESCE(snapshots.compose_content, ''),
        'env_content', CASE WHEN trim(COALESCE(snapshots.env_content, '')) = '' THEN '' ELSE '[legacy value retained in app_config_snapshots]' END,
        'metadata', COALESCE(snapshots.metadata, '')
    ),
    json_object(
        'legacy_snapshot_id', snapshots.id,
        'compose_content', COALESCE(snapshots.compose_content, ''),
        'env_content', CASE WHEN trim(COALESCE(snapshots.env_content, '')) = '' THEN '' ELSE '[legacy value retained in app_config_snapshots]' END,
        'metadata', COALESCE(snapshots.metadata, '')
    ),
    '{}',
    CASE WHEN trim(COALESCE(snapshots.config_hash, '')) = '' THEN 'legacy-app-' || apps.id ELSE snapshots.config_hash END,
    '',
    'migration-0048',
    COALESCE(snapshots.created_at, apps.created_at)
FROM apps
LEFT JOIN app_config_snapshots snapshots ON snapshots.id = (
    SELECT latest.id
    FROM app_config_snapshots latest
    WHERE latest.app_id = apps.id
    ORDER BY latest.revision_no DESC, latest.id DESC
    LIMIT 1
)
WHERE NOT EXISTS (
    SELECT 1 FROM app_config_revisions revisions WHERE revisions.app_id = apps.id
);

UPDATE app_environments
SET current_config_revision_id = (
    SELECT revisions.id
    FROM app_config_revisions revisions
    WHERE revisions.app_id = app_environments.app_id
    ORDER BY revisions.revision_no DESC
    LIMIT 1
)
WHERE current_config_revision_id IS NULL;

INSERT INTO version_counters(scope_kind, scope_id, next_value)
SELECT 'config_revision', apps.id, 101
FROM apps
WHERE true
ON CONFLICT(scope_kind, scope_id) DO NOTHING;
