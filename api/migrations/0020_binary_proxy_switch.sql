ALTER TABLE app_binary_configs
ADD COLUMN proxy_enabled INTEGER NOT NULL DEFAULT 0
CHECK (proxy_enabled IN (0, 1));

ALTER TABLE app_binary_configs
ADD COLUMN proxy_kind TEXT NOT NULL DEFAULT 'none'
CHECK (proxy_kind IN ('none', 'caddy', 'nginx'));

ALTER TABLE app_binary_configs
ADD COLUMN proxy_domain TEXT NOT NULL DEFAULT '';

ALTER TABLE app_binary_configs
ADD COLUMN proxy_config_path TEXT NOT NULL DEFAULT '';
