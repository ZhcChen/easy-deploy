ALTER TABLE app_releases
ADD COLUMN storage_integrity TEXT NOT NULL DEFAULT 'legacy'
CHECK (storage_integrity IN ('legacy', 'local', 'unique_key', 'version_pinned'));

UPDATE app_releases
SET storage_integrity = 'local'
WHERE storage_provider = 'local';

UPDATE app_releases
SET storage_integrity = 'version_pinned'
WHERE storage_provider = 'aliyun_oss'
  AND trim(storage_object_version_id) <> ''
  AND trim(storage_object_version_id) <> 'null';

UPDATE app_releases
SET storage_integrity = 'unique_key'
WHERE storage_provider = 'aliyun_oss'
  AND trim(storage_object_version_id) = ''
  AND EXISTS (
      SELECT 1
      FROM app_release_uploads uploads
      WHERE uploads.app_id = app_releases.app_id
        AND uploads.release_version = app_releases.version
        AND uploads.object_key = app_releases.storage_object_key
        AND uploads.status = 'completed'
        AND uploads.metadata LIKE '%"upload_precondition":"x-oss-forbid-overwrite=true"%'
  );
