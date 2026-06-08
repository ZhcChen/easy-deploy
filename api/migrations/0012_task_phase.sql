ALTER TABLE operation_tasks
ADD COLUMN phase TEXT NOT NULL DEFAULT 'queued'
CHECK (phase IN ('queued', 'preflight', 'preparing_files', 'executing', 'healthchecking', 'completed', 'failed', 'canceled'));

UPDATE operation_tasks
SET phase = CASE status
    WHEN 'queued' THEN 'queued'
    WHEN 'running' THEN 'executing'
    WHEN 'success' THEN 'completed'
    WHEN 'failed' THEN 'failed'
    WHEN 'canceled' THEN 'canceled'
    ELSE phase
END;
