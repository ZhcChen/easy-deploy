ALTER TABLE apps
ADD COLUMN auto_queue_release INTEGER NOT NULL DEFAULT 1
CHECK (auto_queue_release IN (0, 1));

CREATE INDEX IF NOT EXISTS idx_apps_auto_queue_release
ON apps(auto_queue_release);

ALTER TABLE app_release_queue
ADD COLUMN scheduled_publish_at TEXT;

CREATE INDEX IF NOT EXISTS idx_app_release_queue_scheduled_publish
ON app_release_queue(scheduled_publish_at, status, id);
