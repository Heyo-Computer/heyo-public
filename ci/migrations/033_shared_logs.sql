-- Keep large log bodies out of status rows. Appends serialize on ci_step.
CREATE TABLE IF NOT EXISTS ci_step_log (
    step_id TEXT NOT NULL REFERENCES ci_step(id) ON DELETE CASCADE,
    byte_offset BIGINT NOT NULL CHECK (byte_offset >= 0),
    bytes BYTEA NOT NULL,
    PRIMARY KEY (step_id, byte_offset)
);
