-- Independent of a host upgrade or the CI process serving the request.
CREATE TABLE IF NOT EXISTS ci_runner_drain (
    runner_hd_id TEXT PRIMARY KEY,
    operation_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
