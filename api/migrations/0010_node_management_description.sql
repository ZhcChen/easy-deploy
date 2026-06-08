UPDATE admin_permissions
SET description = '新增、编辑、探测、启用和禁用部署目标机器。',
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE permission_key = 'nodes.manage';
