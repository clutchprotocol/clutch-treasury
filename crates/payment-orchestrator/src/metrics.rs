//! Prometheus metrics, read straight out of Postgres. Same reasoning as treasury-service's
//! `metrics` module: the state worth paging on is already durable, so a scrape aggregates it
//! rather than keeping counters that reset on deploy.
//!
//! Served on a SEPARATE listener from the API router, and that matters more here than it does in
//! the treasury: this service's router is reachable from the public internet through nginx's
//! `/payment/` route, so a `/metrics` route added to it would publish deposit counts and poller
//! health to anyone who asked.
//!
//! ponytail: uncached aggregates per scrape. Cheap at a 30s interval; revisit only if the tables
//! grow enough to make it show up in query time.

use std::collections::HashMap;

use axum::{routing::get, Router};
use sqlx::PgPool;

const DEPOSIT_STATUSES: [&str; 7] = [
    "created", "confirmed", "mint_requested", "credited", "needs_manual", "failed", "expired",
];
const SEVERITIES: [&str; 3] = ["info", "warn", "p1"];

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

fn labelled(out: &mut String, name: &str, label: &str, known: &[&str], counts: &HashMap<String, i64>) {
    for value in known {
        let n = counts.get(*value).copied().unwrap_or(0);
        out.push_str(&format!("{name}{{{label}=\"{value}\"}} {n}\n"));
    }
}

async fn counts_by(pool: &PgPool, sql: &str) -> HashMap<String, i64> {
    sqlx::query_as::<_, (String, i64)>(sql)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

pub async fn render(pool: &PgPool) -> String {
    let mut out = String::new();

    header(&mut out, "clutch_orchestrator_up", "1 when the service answered a scrape.", "gauge");
    out.push_str("clutch_orchestrator_up 1\n");

    let alerts = counts_by(pool, "SELECT severity, COUNT(*)::BIGINT FROM alerts GROUP BY severity").await;
    header(
        &mut out,
        "clutch_orchestrator_alerts_total",
        "Alerts ever raised, by severity. Append-only, so alert on the rate rather than the value.",
        "counter",
    );
    labelled(&mut out, "clutch_orchestrator_alerts_total", "severity", &SEVERITIES, &alerts);

    let deposits = counts_by(pool, "SELECT status, COUNT(*)::BIGINT FROM deposit_intents GROUP BY status").await;
    header(
        &mut out,
        "clutch_orchestrator_deposit_intents",
        "Deposit intents by status. `needs_manual` above zero means a user's USDT arrived and their CLT did not.",
        "gauge",
    );
    labelled(&mut out, "clutch_orchestrator_deposit_intents", "status", &DEPOSIT_STATUSES, &deposits);

    let addresses: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM deposit_addresses")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    header(
        &mut out,
        "clutch_orchestrator_deposit_addresses",
        "Permanent deposit addresses issued. One per user, never reissued.",
        "gauge",
    );
    out.push_str(&format!("clutch_orchestrator_deposit_addresses {addresses}\n"));

    // The poller's own health, and the reason this module exists. `last_polled_at` only advances on
    // a SUCCESSFUL TronGrid read, so the oldest one climbing without bound is what a throttled or
    // broken watcher looks like from outside — and a watcher that has stopped reading is
    // indistinguishable, to a user, from nobody paying.
    let oldest: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (now() - MIN(last_polled_at)))::float8
         FROM deposit_addresses WHERE last_polled_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(None);
    header(
        &mut out,
        "clutch_orchestrator_oldest_poll_age_seconds",
        "Age of the least recently READ deposit address. Climbing without bound means the poller is not completing reads.",
        "gauge",
    );
    if let Some(age) = oldest {
        out.push_str(&format!("clutch_orchestrator_oldest_poll_age_seconds {age:.0}\n"));
    }

    let never: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM deposit_addresses WHERE last_polled_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    header(
        &mut out,
        "clutch_orchestrator_addresses_never_polled",
        "Issued addresses never once read successfully. Briefly non-zero after issuing one; persistently non-zero is a stuck rotation.",
        "gauge",
    );
    out.push_str(&format!("clutch_orchestrator_addresses_never_polled {never}\n"));

    out
}

/// Spawns the metrics listener. Published nowhere; Prometheus reaches it over the compose network.
pub fn serve(pool: PgPool, addr: String) {
    tokio::spawn(async move {
        let app = Router::new().route(
            "/metrics",
            get({
                let pool = pool.clone();
                move || {
                    let pool = pool.clone();
                    async move { render(&pool).await }
                }
            }),
        );
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                tracing::info!("payment-orchestrator metrics on {addr}/metrics");
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!("metrics listener stopped: {e}");
                }
            }
            // Never fatal: losing metrics must not take down the deposit path.
            Err(e) => tracing::error!("metrics listener could not bind {addr}: {e}"),
        }
    });
}
