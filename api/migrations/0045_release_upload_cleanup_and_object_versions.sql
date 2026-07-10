ALTER TABLE app_releases
ADD COLUMN storage_object_version_id TEXT NOT NULL DEFAULT '';

ALTER TABLE app_release_uploads
ADD COLUMN object_version_id TEXT NOT NULL DEFAULT '';

ALTER TABLE app_release_uploads
ADD COLUMN cleanup_started_at TEXT;

ALTER TABLE app_release_uploads
ADD COLUMN cleanup_completed_at TEXT;

ALTER TABLE app_release_uploads
ADD COLUMN cleanup_attempts INTEGER NOT NULL DEFAULT 0
CHECK (cleanup_attempts >= 0);

CREATE INDEX IF NOT EXISTS idx_app_release_uploads_pending_cleanup
ON app_release_uploads(object_cleanup_at, cleanup_completed_at, cleanup_started_at);
