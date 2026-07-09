INSERT OR IGNORE INTO platform_settings(setting_key, setting_value, updated_by)
VALUES
    ('artifact_storage_provider', 'local', 'system'),
    ('aliyun_oss_region', 'oss-cn-hangzhou', 'system'),
    ('aliyun_oss_endpoint', 'https://oss-cn-hangzhou.aliyuncs.com', 'system'),
    ('aliyun_oss_bucket', '', 'system'),
    ('aliyun_oss_object_prefix', 'easy-deploy/releases', 'system'),
    ('aliyun_oss_access_key_id', '', 'system'),
    ('aliyun_oss_access_key_secret', '', 'system'),
    ('aliyun_oss_upload_url_ttl_seconds', '900', 'system'),
    ('aliyun_oss_download_url_ttl_seconds', '600', 'system');

ALTER TABLE app_releases
ADD COLUMN storage_provider TEXT NOT NULL DEFAULT 'local'
CHECK (storage_provider IN ('local', 'aliyun_oss'));

ALTER TABLE app_releases
ADD COLUMN storage_bucket TEXT NOT NULL DEFAULT '';

ALTER TABLE app_releases
ADD COLUMN storage_object_key TEXT NOT NULL DEFAULT '';

ALTER TABLE app_releases
ADD COLUMN storage_endpoint TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_app_releases_storage_object
ON app_releases(storage_provider, storage_bucket, storage_object_key);

CREATE TABLE IF NOT EXISTS app_release_uploads (
    id TEXT PRIMARY KEY,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    release_version TEXT NOT NULL,
    version_code INTEGER NOT NULL,
    file_name TEXT NOT NULL,
    object_key TEXT NOT NULL,
    bucket TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    checksum_sha256 TEXT NOT NULL DEFAULT '',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'completed', 'expired', 'canceled')),
    source TEXT NOT NULL DEFAULT '',
    published_at TEXT NOT NULL DEFAULT '',
    expires_at TEXT NOT NULL,
    metadata TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    completed_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_app_release_uploads_app_status
ON app_release_uploads(app_id, status, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_app_release_uploads_expires
ON app_release_uploads(status, expires_at);
