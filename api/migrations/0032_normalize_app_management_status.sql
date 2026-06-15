UPDATE apps
SET status = 'ready'
WHERE status != 'disabled';

CREATE TRIGGER IF NOT EXISTS trg_apps_normalize_management_status_on_insert
AFTER INSERT ON apps
FOR EACH ROW
WHEN NEW.status NOT IN ('ready', 'disabled')
BEGIN
    UPDATE apps
    SET status = 'ready'
    WHERE id = NEW.id;
END;

CREATE TRIGGER IF NOT EXISTS trg_apps_normalize_management_status_on_update
AFTER UPDATE OF status ON apps
FOR EACH ROW
WHEN NEW.status NOT IN ('ready', 'disabled')
BEGIN
    UPDATE apps
    SET status = 'ready'
    WHERE id = NEW.id;
END;
