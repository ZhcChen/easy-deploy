DELETE FROM admin_role_permissions
WHERE permission_id IN (
    SELECT id
    FROM admin_permissions
    WHERE permission_key IN ('nodes.create', 'nodes.update', 'nodes.delete')
);

DELETE FROM admin_permissions
WHERE permission_key IN ('nodes.create', 'nodes.update', 'nodes.delete');
