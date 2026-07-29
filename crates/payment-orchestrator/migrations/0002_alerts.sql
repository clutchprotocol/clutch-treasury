-- Same shape as treasury-service's own alerts table (crates/treasury-service/migrations/0001_init.sql)
-- — this is a separate database with its own pool, so it needs its own table, not a shared one.
-- T4's needs_manual paths (PaidPartial, FailedConfirm, Unknown, and the PaidOver surplus notice)
-- land here for a human to see; no silent overrides, same rule as the treasury side.
CREATE TABLE alerts (
    id BIGSERIAL PRIMARY KEY,
    severity TEXT NOT NULL CHECK (severity IN ('info','warn','p1')),
    source TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
