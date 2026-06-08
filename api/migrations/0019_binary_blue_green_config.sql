ALTER TABLE app_binary_configs
ADD COLUMN release_strategy TEXT NOT NULL DEFAULT 'restart'
CHECK (release_strategy IN ('restart', 'blue_green'));

ALTER TABLE app_binary_configs
ADD COLUMN active_slot TEXT NOT NULL DEFAULT 'blue'
CHECK (active_slot IN ('blue', 'green'));

ALTER TABLE app_binary_configs
ADD COLUMN base_port INTEGER NOT NULL DEFAULT 0;

ALTER TABLE app_binary_configs
ADD COLUMN standby_port INTEGER NOT NULL DEFAULT 0;
