ALTER TABLE app_config_drafts
ADD COLUMN secret_ciphertext TEXT NOT NULL DEFAULT '';

ALTER TABLE app_config_drafts
ADD COLUMN secret_fingerprints TEXT NOT NULL DEFAULT '{}';

ALTER TABLE app_config_drafts
ADD COLUMN encryption_key_id TEXT NOT NULL DEFAULT '';
