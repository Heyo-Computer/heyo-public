-- Operator isolation is not evidence that execution on the native host stopped.
-- Keep ci_host_work intact; prevent this runner from receiving more work.
CREATE TABLE IF NOT EXISTS ci_native_quarantine (
    runner_id TEXT PRIMARY KEY REFERENCES ci_native_runner(id),
    report_uri TEXT NOT NULL CHECK (report_uri LIKE 's3://%'),
    requested_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
