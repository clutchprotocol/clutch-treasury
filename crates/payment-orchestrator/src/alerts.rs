//! Same shape as treasury-service's `ledger::alert` — log + a DB row, nothing more (this
//! crate has its own database, so it needs its own table rather than sharing treasury's).
//! T4's needs_manual paths (PaidPartial, FailedConfirm, Unknown, PaidOver's surplus notice)
//! all go through this.

use sqlx::PgPool;

pub async fn alert(pool: &PgPool, severity: &str, source: &str, message: &str) {
    tracing::error!(source, severity, "{}", message);
    let _ = sqlx::query("INSERT INTO alerts (severity, source, message) VALUES ($1, $2, $3)")
        .bind(severity)
        .bind(source)
        .bind(message)
        .execute(pool)
        .await;
}
