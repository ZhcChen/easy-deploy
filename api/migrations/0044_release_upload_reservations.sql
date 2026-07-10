ALTER TABLE app_release_uploads
ADD COLUMN reservation_active INTEGER NOT NULL DEFAULT 1
CHECK (reservation_active IN (0, 1));

ALTER TABLE app_release_uploads
ADD COLUMN verification_started_at TEXT;

ALTER TABLE app_release_uploads
ADD COLUMN object_cleanup_at TEXT;

ALTER TABLE app_release_uploads
ADD COLUMN cleanup_error TEXT NOT NULL DEFAULT '';

UPDATE app_release_uploads
SET reservation_active = 0
WHERE status <> 'pending';

UPDATE app_release_uploads
SET object_cleanup_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE status IN ('expired', 'canceled')
  AND object_cleanup_at IS NULL;

UPDATE app_release_uploads
SET status = 'canceled',
    reservation_active = 0,
    object_cleanup_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    cleanup_error = 'migration_reservation_deduplicated',
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE status = 'pending'
  AND EXISTS (
      SELECT 1
      FROM app_release_uploads newer
      WHERE newer.app_id = app_release_uploads.app_id
        AND newer.release_version = app_release_uploads.release_version
        AND newer.status = 'pending'
        AND (
            newer.created_at > app_release_uploads.created_at
            OR (
                newer.created_at = app_release_uploads.created_at
                AND newer.id > app_release_uploads.id
            )
        )
  );

CREATE UNIQUE INDEX IF NOT EXISTS idx_app_release_uploads_active_version
ON app_release_uploads(app_id, release_version)
WHERE reservation_active = 1;

CREATE INDEX IF NOT EXISTS idx_app_release_uploads_pending_verification
ON app_release_uploads(status, verification_started_at, expires_at);
