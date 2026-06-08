ALTER TABLE apps
ADD COLUMN deploy_strategy TEXT NOT NULL DEFAULT 'rolling_stop_on_failure'
CHECK (deploy_strategy IN ('rolling_stop_on_failure', 'rolling_continue'));
