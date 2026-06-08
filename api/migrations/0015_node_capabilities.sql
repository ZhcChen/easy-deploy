CREATE TABLE IF NOT EXISTS node_capabilities (
    node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    check_status TEXT NOT NULL CHECK (check_status IN ('passed', 'failed', 'unknown')) DEFAULT 'unknown',
    message TEXT NOT NULL DEFAULT '',
    docker_available INTEGER NOT NULL DEFAULT 0 CHECK (docker_available IN (0, 1)),
    compose_available INTEGER NOT NULL DEFAULT 0 CHECK (compose_available IN (0, 1)),
    systemd_available INTEGER NOT NULL DEFAULT 0 CHECK (systemd_available IN (0, 1)),
    docker_version TEXT NOT NULL DEFAULT '',
    compose_version TEXT NOT NULL DEFAULT '',
    os_info TEXT NOT NULL DEFAULT '',
    disk_info TEXT NOT NULL DEFAULT '',
    systemd_version TEXT NOT NULL DEFAULT '',
    checked_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO node_capabilities(
    node_id,
    check_status,
    message,
    docker_available,
    compose_available,
    systemd_available,
    docker_version,
    compose_version,
    os_info,
    disk_info,
    systemd_version,
    checked_at
)
SELECT
    n.id,
    COALESCE(c.check_status, 'unknown'),
    COALESCE(c.message, ''),
    CASE
        WHEN c.check_status = 'passed' AND c.docker_version <> '' THEN 1
        ELSE 0
    END,
    CASE
        WHEN c.check_status = 'passed' AND c.compose_version <> '' THEN 1
        ELSE 0
    END,
    CASE
        WHEN c.check_status = 'passed'
         AND c.systemd_version <> ''
         AND c.systemd_version NOT LIKE '%:%'
        THEN 1
        ELSE 0
    END,
    COALESCE(c.docker_version, ''),
    COALESCE(c.compose_version, ''),
    COALESCE(c.os_info, ''),
    COALESCE(c.disk_info, ''),
    COALESCE(c.systemd_version, ''),
    c.checked_at
FROM nodes n
LEFT JOIN node_checks c ON c.id = (
    SELECT latest.id
    FROM node_checks latest
    WHERE latest.node_id = n.id
    ORDER BY latest.id DESC
    LIMIT 1
)
ON CONFLICT(node_id) DO UPDATE SET
    check_status = excluded.check_status,
    message = excluded.message,
    docker_available = excluded.docker_available,
    compose_available = excluded.compose_available,
    systemd_available = excluded.systemd_available,
    docker_version = excluded.docker_version,
    compose_version = excluded.compose_version,
    os_info = excluded.os_info,
    disk_info = excluded.disk_info,
    systemd_version = excluded.systemd_version,
    checked_at = excluded.checked_at,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');
