//! Prometheus metrics, read straight out of Postgres.
//!
//! Deliberately not an in-process counter registry. Everything worth paging on here is already
//! durable state in the database — the breaker, the last reconciliation, how many intents are
//! waiting on a human, how many deposit addresses are unswept — and a counter living in process
//! memory would reset on every deploy and quietly disagree with the ledger it is supposed to
//! describe. A scrape runs a handful of aggregates instead, so the number Prometheus sees is the
//! number an operator would get from `psql`.
//!
//! Served on its own listener, never on the service's API router. The orchestrator's router is
//! reachable from the public internet through nginx's `/payment/` route, and its sibling module
//! there follows the same rule for that reason; treasury-service publishes no port at all, but
//! keeping both the same means neither can acquire a public metrics endpoint by accident later.
//!
//! ponytail: a few small aggregate queries per scrape, uncached. At a 30s interval on tables this
//! size that is free. If `alerts` ever grows enough to make the count sting, add a partial index
//! or memoise the rendered body for a few seconds — do not reach for a counter registry.

use std::collections::HashMap;

use axum::{routing::get, Router};
use sqlx::PgPool;

/// Every status `mint_intents` can hold (migration 0001, widened by 0011). Emitted even at zero:
/// a series that only appears once something goes wrong is a series nobody has an alert on, and
/// `needs_manual > 0` is exactly the rule worth writing.
const MINT_STATUSES: [&str; 7] = [
    "created", "approved", "submitted", "credited", "failed", "rejected", "needs_manual",
];
const OUTBOX_STATUSES: [&str; 4] = ["pending", "submitted", "confirmed", "failed"];
const SEVERITIES: [&str; 3] = ["info", "warn", "p1"];
const RECONCILIATION_STATUSES: [&str; 4] = ["ok", "over_backed_drift", "mismatch", "error"];

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

/// Emits one sample per known label value, defaulting to zero. `GROUP BY` only returns rows that
/// exist, so without this a status with no rows would vanish from the scrape rather than read 0.
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

    // Scrape reachability is itself a signal: if this is missing, the service is down or its
    // database is unreachable, and every other number below is stale rather than zero.
    header(&mut out, "clutch_treasury_up", "1 when the service answered a scrape.", "gauge");
    out.push_str("clutch_treasury_up 1\n");

    let halted: Option<(bool,)> = sqlx::query_as("SELECT minting_halted FROM breaker_state")
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
    header(
        &mut out,
        "clutch_treasury_minting_halted",
        "1 when the breaker has halted minting. Requires a human to clear (resume-minting).",
        "gauge",
    );
    out.push_str(&format!(
        "clutch_treasury_minting_halted {}\n",
        i32::from(halted.map(|(h,)| h).unwrap_or(false))
    ));

    let alerts = counts_by(pool, "SELECT severity, COUNT(*)::BIGINT FROM alerts GROUP BY severity").await;
    header(
        &mut out,
        "clutch_treasury_alerts_total",
        "Alerts ever raised, by severity. The alerts table is append-only, so this only grows; alert on the rate, not the value.",
        "counter",
    );
    labelled(&mut out, "clutch_treasury_alerts_total", "severity", &SEVERITIES, &alerts);

    let intents = counts_by(pool, "SELECT status, COUNT(*)::BIGINT FROM mint_intents GROUP BY status").await;
    header(
        &mut out,
        "clutch_treasury_mint_intents",
        "Mint intents by status. `needs_manual` above zero means real money is waiting on a person.",
        "gauge",
    );
    labelled(&mut out, "clutch_treasury_mint_intents", "status", &MINT_STATUSES, &intents);

    let outbox = counts_by(pool, "SELECT status, COUNT(*)::BIGINT FROM chain_outbox GROUP BY status").await;
    header(&mut out, "clutch_treasury_chain_outbox", "Chain outbox rows by status.", "gauge");
    labelled(&mut out, "clutch_treasury_chain_outbox", "status", &OUTBOX_STATUSES, &outbox);

    let unswept: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT deposit_address)::BIGINT FROM mint_intents
         WHERE deposit_address IS NOT NULL AND swept_at IS NULL
           AND status IN ('approved', 'submitted', 'credited', 'needs_manual')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    header(
        &mut out,
        "clutch_treasury_unswept_deposit_addresses",
        "Distinct deposit addresses holding USDT the sweeper has not yet collected. Counted in the reserve; a number that only climbs means sweeping has stopped.",
        "gauge",
    );
    out.push_str(&format!("clutch_treasury_unswept_deposit_addresses {unswept}\n"));

    if let Ok(b) = crate::ledger::balances(pool).await {
        header(&mut out, "clutch_treasury_clt_liability", "CLT in circulation, base units.", "gauge");
        out.push_str(&format!("clutch_treasury_clt_liability {}\n", b.clt_liability));
        header(&mut out, "clutch_treasury_custody_usdt", "USDT held, micro-USDT.", "gauge");
        out.push_str(&format!("clutch_treasury_custody_usdt {}\n", b.custody_usdt));
    }

    let recon: Option<(String, f64)> = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM (now() - run_at))::float8
         FROM reconciliation_runs ORDER BY run_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    header(
        &mut out,
        "clutch_treasury_reconciliation_status",
        "The latest reconciliation run's outcome, one series per possible status, 1 on the current one.",
        "gauge",
    );
    for status in RECONCILIATION_STATUSES {
        let hit = recon.as_ref().is_some_and(|(s, _)| s == status);
        out.push_str(&format!(
            "clutch_treasury_reconciliation_status{{status=\"{status}\"}} {}\n",
            i32::from(hit)
        ));
    }
    header(
        &mut out,
        "clutch_treasury_reconciliation_age_seconds",
        "Seconds since the last reconciliation run. Stale is its own failure: the mint gate reads this.",
        "gauge",
    );
    if let Some((_, age)) = recon {
        out.push_str(&format!("clutch_treasury_reconciliation_age_seconds {age:.0}\n"));
    }

    out
}

/// Spawns the metrics listener. Bound wherever config says, published nowhere: Prometheus reaches
/// it over the compose network by service name.
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
                tracing::info!("treasury-service metrics on {addr}/metrics");
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!("metrics listener stopped: {e}");
                }
            }
            // Never fatal: losing metrics must not take down a service that moves money.
            Err(e) => tracing::error!("metrics listener could not bind {addr}: {e}"),
        }
    });
}
