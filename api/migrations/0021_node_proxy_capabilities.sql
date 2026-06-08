ALTER TABLE node_capabilities
ADD COLUMN caddy_available INTEGER NOT NULL DEFAULT 0
CHECK (caddy_available IN (0, 1));

ALTER TABLE node_capabilities
ADD COLUMN nginx_available INTEGER NOT NULL DEFAULT 0
CHECK (nginx_available IN (0, 1));

ALTER TABLE node_capabilities
ADD COLUMN caddy_version TEXT NOT NULL DEFAULT '';

ALTER TABLE node_capabilities
ADD COLUMN nginx_version TEXT NOT NULL DEFAULT '';
