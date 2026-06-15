UPDATE apps
SET status = 'ready'
WHERE status = 'draft';

CREATE TRIGGER IF NOT EXISTS trg_apps_normalize_draft_status
AFTER INSERT ON apps
FOR EACH ROW
WHEN NEW.status = 'draft'
BEGIN
    UPDATE apps
    SET status = 'ready'
    WHERE id = NEW.id;
END;
