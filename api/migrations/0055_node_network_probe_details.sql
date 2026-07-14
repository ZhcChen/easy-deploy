ALTER TABLE node_checks
ADD COLUMN public_ip TEXT NOT NULL DEFAULT '';

ALTER TABLE node_checks
ADD COLUMN private_ips TEXT NOT NULL DEFAULT '';

ALTER TABLE node_capabilities
ADD COLUMN public_ip TEXT NOT NULL DEFAULT '';

ALTER TABLE node_capabilities
ADD COLUMN private_ips TEXT NOT NULL DEFAULT '';
