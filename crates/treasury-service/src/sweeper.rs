//! Deciding when to sweep a deposit address, and recording that it happened.
//!
//! The split matters: this service decides WHEN, `tron-signer` knows HOW. The threshold needs a
//! balance, an age and the sweep bookkeeping — all of which live here — and putting the decision
//! next to the keys would mean linking mnemonic-handling code into whatever else wanted to reason
//! about it.
//!
//! # A sweep must not move the ledger
//!
//! Sweeping transfers USDT between two addresses WE control. The reserve is unchanged; the money was
//! counted when it arrived, by `custody_deposit`. So this writes `swept_at` and nothing else.
//!
//! Appending a ledger event here would double-count every deposit — inflating `custody_reported`
//! against an unchanged liability, which reads as over-backing and, for anyone reconciling by hand,
//! as money appearing from nowhere. `get_reserve_balance` already handles the on-chain side by
//! summing unswept addresses plus the treasury, so the total it reports is identical before and
//! after a sweep. That invariance is the point.

use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::ledger::alert;
use crate::tron_verifier::TronClient;

/// Is this address worth sweeping yet?
///
/// Threshold rather than per-deposit, because a sweep is not free: the address must first hold TRX
/// for energy, and against a $1 minimum deposit sweeping on arrival can cost more than it moves.
///
/// The age escape valve is what stops that becoming "small deposits never move". Without it a
/// sub-threshold balance sits at its address indefinitely and the reserve fragments permanently
/// across addresses nobody revisits — the reserve SUM stays correct, but the funds become unusable
/// in practice and the fragmentation only grows.
pub fn should_sweep(
    balance_usdt: i64,
    threshold_usdt: i64,
    age_hours: i64,
    max_age_hours: i64,
    _min_usdt: i64,
) -> bool {
    if balance_usdt <= 0 {
        return false;
    }
    balance_usdt >= threshold_usdt || age_hours >= max_age_hours
}

/// What the signer reported for one address.
#[derive(Debug, PartialEq)]
pub enum SignerReply {
    Swept { tx_id: String },
    /// Address already empty — the sweep is complete by definition.
    NothingToSweep,
    /// The signer just sent the address TRX so it can pay its own transfer fee. NOT a failure: a
    /// fresh address holds no TRX, because receiving tokens does not create a balance, so this is
    /// the expected first answer for every address. The funding has to confirm before the sweep, so
    /// the address stays unswept for a later pass.
    Funded { tx_id: String, amount_sun: i64 },
    /// The wallet's TRX float has run out. Nothing here can fix it — an operator has to send TRX to
    /// `fee_address` — and until they do, no address can be swept.
    FeeAccountDry { fee_address: String, have_sun: i64, need_sun: i64 },
    Failed(String),
}

/// The signer boundary, as a trait so the worker is testable without a live service or real keys.
#[async_trait::async_trait]
pub trait SweepSigner: Send + Sync {
    /// Sweep the address at `index`. Deliberately takes ONLY an index: the destination is the
    /// signer's own config, so nothing here can redirect funds. Do not widen this signature.
    async fn sweep(&self, index: i64) -> SignerReply;
}

/// One pass: for every unswept deposit address, decide, sweep, record. Returns the number of
/// `approved`/`submitted`/`credited` rows that currently have NO `derivation_index` — see the
/// warning below for why that count matters.
pub async fn sweep_once(pool: &PgPool, config: &AppConfig, client: &TronClient, signer: &dyn SweepSigner) -> usize {
    let rows: Vec<(Uuid, String, i64, f64)> = match sqlx::query_as(
        // `credited` and later only. Sweeping an address whose deposit has not yet been credited
        // would move the evidence out from under the verifier before it has finished with it.
        "SELECT id, deposit_address, derivation_index,
                (EXTRACT(EPOCH FROM (now() - created_at)) / 3600.0)::double precision
         FROM mint_intents
         WHERE deposit_address IS NOT NULL
           AND derivation_index IS NOT NULL
           AND swept_at IS NULL
           AND status IN ('credited', 'submitted')
         ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("sweeper: could not list unswept addresses: {e}");
            return 0;
        }
    };

    // One line per pass, always -- including when there is nothing to do.
    //
    // Without it this worker is completely silent while idle, which is byte-for-byte what a worker
    // that died at startup looks like. There is no way to tell them apart from outside, and the
    // difference is "no deposits to consolidate" versus "money is accumulating at addresses nothing
    // will ever sweep".
    //
    // The interval is an hour, so this costs 24 lines a day and buys a liveness signal that does
    // not depend on anything going wrong first.
    tracing::info!("sweeper: pass over {} unswept address(es)", rows.len());

    // A credited deposit with no derivation_index can never satisfy the query above (it requires
    // `derivation_index IS NOT NULL`), so it would sit unswept forever while this pass keeps logging
    // the same "N unswept address(es)" line a healthy pass would show. Checked independently of the
    // loop below, every pass, so a row stuck like this cannot hide behind an otherwise-quiet worker.
    let missing_index: Vec<String> = match sqlx::query_scalar(
        "SELECT deposit_address FROM mint_intents
         WHERE deposit_address IS NOT NULL
           AND derivation_index IS NULL
           AND swept_at IS NULL
           AND status IN ('approved', 'submitted', 'credited', 'needs_manual')",
    )
    .fetch_all(pool)
    .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("sweeper: could not check for deposits missing a derivation_index: {e}");
            Vec::new()
        }
    };
    let missing_count = missing_index.len();
    if missing_count > 0 {
        let mut distinct_addresses = missing_index;
        distinct_addresses.sort_unstable();
        distinct_addresses.dedup();
        // An 'approved' row here is not yet stuck — the sweep query above only ever selects
        // ('credited', 'submitted'), so 'approved' simply isn't sweep-eligible yet regardless of
        // this column. It is 'submitted'/'credited' rows missing the index that are truly stuck:
        // those statuses ARE what the sweep query selects on, so a missing index is the only thing
        // excluding them, and derivation_index never gets set after the row is created.
        let message = format!(
            "sweeper: {missing_count} deposit(s) have no derivation_index — an 'approved' row is not \
             yet eligible for sweeping, but any already 'submitted' or 'credited' can never be swept \
             without one: {distinct_addresses:?}"
        );
        tracing::warn!("{message}");
        // Beside the log line: a plain warn! is invisible to whatever watches the alerts table, and
        // this condition is exactly as actionable as every other sweeper alert below.
        alert(pool, "warn", "sweeper", &message).await;
    }

    for (id, address, index, age_hours) in rows {
        let balance = match client.get_custody_balance(&address, &config.usdt_contract).await {
            Ok(b) => b,
            Err(e) => {
                // Transient. Never mark swept on an unread balance: that would abandon real funds
                // at an address nothing looks at again.
                tracing::warn!("sweeper: balance read failed for {address}: {e}");
                continue;
            }
        };

        if !should_sweep(
            balance,
            config.sweep_threshold_usdt,
            age_hours as i64,
            config.sweep_max_age_hours,
            config.sweep_min_usdt,
        ) {
            continue;
        }

        match signer.sweep(index).await {
            SignerReply::Swept { tx_id } => {
                // swept_at ONLY. No ledger event — see the module docs: the reserve did not change,
                // and recording one here would double-count the deposit.
                if let Err(e) = mark_swept(pool, id).await {
                    // The funds moved but we failed to record it. Loud, because the next pass will
                    // find a now-empty address and resolve it as NothingToSweep — correct, but only
                    // by luck, and a human should know a write was lost.
                    alert(
                        pool,
                        "p1",
                        "sweeper",
                        &format!("swept {address} in {tx_id} but failed to record swept_at for intent {id}: {e}"),
                    )
                    .await;
                } else {
                    tracing::info!("swept {balance} micro-USDT from {address} (index {index}) in {tx_id}");
                }
            }

            // Already empty: nothing to move, and the address is done. Recorded so it stops being
            // polled and stops inflating the reserve walk.
            SignerReply::NothingToSweep => {
                if let Err(e) = mark_swept(pool, id).await {
                    tracing::error!("sweeper: failed to mark empty address {address} swept: {e}");
                }
            }

            // Expected, not exceptional — every fresh address starts here, because receiving
            // tokens does not create a TRX balance. Left unswept deliberately: the funding transfer
            // has to confirm before the sweep can spend it, so the next pass finishes the job.
            SignerReply::Funded { tx_id, amount_sun } => {
                tracing::info!(
                    "sweeper: funded {address} (index {index}) with {amount_sun} sun in {tx_id}; \
                     sweeping {balance} micro-USDT on a later pass"
                );
            }

            // The only outcome no retry resolves. Break rather than continue: every remaining
            // address in this pass gets the same answer, and alerting once per address would bury
            // the one fact that matters under a pass-sized burst of duplicates.
            SignerReply::FeeAccountDry { fee_address, have_sun, need_sun } => {
                alert(
                    pool,
                    "warn",
                    "sweeper",
                    &format!(
                        "TRX float exhausted: {fee_address} holds {have_sun} sun, needs {need_sun}. \
                         No deposit can be swept until it is topped up."
                    ),
                )
                .await;
                return missing_count;
            }

            SignerReply::Failed(e) => {
                alert(pool, "warn", "sweeper", &format!("sweep of {address} (index {index}) failed: {e}")).await;
            }
        }
    }

    missing_count
}

async fn mark_swept(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    // Guarded on IS NULL so a concurrent or repeated pass cannot overwrite the original timestamp.
    sqlx::query("UPDATE mint_intents SET swept_at = now() WHERE id = $1 AND swept_at IS NULL")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::should_sweep;

    const THRESHOLD: i64 = 100_000_000; // $100
    const MAX_AGE: i64 = 168; // a week
    const MIN: i64 = 1_000_000; // $1 — the dust floor under the age valve
    const NO_MIN: i64 = 0; // the old behaviour: no floor at all

    #[test]
    fn sweeps_once_the_balance_reaches_the_threshold() {
        assert!(should_sweep(THRESHOLD, THRESHOLD, 0, MAX_AGE, NO_MIN));
        assert!(should_sweep(THRESHOLD + 1, THRESHOLD, 0, MAX_AGE, NO_MIN));
    }

    #[test]
    fn leaves_a_small_fresh_balance_alone() {
        assert!(!should_sweep(THRESHOLD - 1, THRESHOLD, 0, MAX_AGE, NO_MIN));
    }

    /// The escape valve. Without it anything under the threshold sits at its address forever and the
    /// reserve fragments permanently across addresses nobody revisits.
    #[test]
    fn sweeps_a_small_balance_once_it_is_old_enough() {
        assert!(should_sweep(MIN, THRESHOLD, MAX_AGE, MAX_AGE, MIN));
        assert!(should_sweep(MIN, THRESHOLD, MAX_AGE + 100, MAX_AGE, MIN));
    }

    /// An empty address is never worth a transaction, however old — a sweep would spend TRX to move
    /// nothing.
    #[test]
    fn never_sweeps_an_empty_address_however_old() {
        assert!(!should_sweep(0, THRESHOLD, MAX_AGE * 10, MAX_AGE, NO_MIN));
        assert!(!should_sweep(-1, THRESHOLD, MAX_AGE * 10, MAX_AGE, NO_MIN), "a negative balance is nonsense, not a sweep");
    }

    #[test]
    fn a_zero_threshold_sweeps_any_positive_balance() {
        assert!(should_sweep(1, 0, 0, MAX_AGE, NO_MIN));
    }

    /// The age valve used to fire on ANY positive balance. That spends TRX to recover less USDT
    /// than the transfer costs — a guaranteed loss, repeated per dust address. Leaving it put is
    /// not a loss: an unswept address is still counted in the reserve.
    #[test]
    fn the_age_valve_never_sweeps_less_than_a_sweep_costs() {
        assert!(!should_sweep(1, THRESHOLD, MAX_AGE * 10, MAX_AGE, MIN));
        assert!(!should_sweep(MIN - 1, THRESHOLD, MAX_AGE * 10, MAX_AGE, MIN));
    }

    /// The floor is a floor, not a second threshold: at or above it the age valve works as before.
    #[test]
    fn the_floor_still_lets_the_age_valve_fire() {
        assert!(should_sweep(MIN, THRESHOLD, MAX_AGE, MAX_AGE, MIN));
        assert!(should_sweep(MIN + 1, THRESHOLD, MAX_AGE, MAX_AGE, MIN));
    }

    /// A misconfigured floor above the threshold still wins. Spending more than the balance is
    /// wrong whichever rule asked for it, so the floor gates both paths.
    #[test]
    fn the_floor_outranks_the_threshold() {
        assert!(!should_sweep(THRESHOLD, THRESHOLD, 0, MAX_AGE, THRESHOLD + 1));
    }
}

/// The real signer, over HTTP.
///
/// Sends ONLY the index — the destination lives in the signer's config, so nothing this service
/// says can redirect funds. That is the property the whole split exists for; do not add fields.
pub struct HttpSigner {
    pub http: reqwest::Client,
    pub base_url: String,
    pub token: String,
}

#[async_trait::async_trait]
impl SweepSigner for HttpSigner {
    async fn sweep(&self, index: i64) -> SignerReply {
        let resp = self
            .http
            .post(format!("{}/internal/sweep", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "index": index }))
            .send()
            .await;
        let body: serde_json::Value = match resp {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(v) => v,
                Err(e) => return SignerReply::Failed(format!("unreadable signer response: {e}")),
            },
            Ok(r) => return SignerReply::Failed(format!("signer returned {}", r.status())),
            Err(e) => return SignerReply::Failed(format!("signer unreachable: {e}")),
        };
        match body["status"].as_str() {
            Some("swept") => match body["tx_id"].as_str() {
                Some(tx) => SignerReply::Swept { tx_id: tx.to_string() },
                // Reported success without naming the transaction: refuse to record a sweep we
                // cannot point at on-chain.
                None => SignerReply::Failed("signer reported swept with no tx_id".into()),
            },
            Some("nothing_to_sweep") => SignerReply::NothingToSweep,
            Some("funded") => match body["tx_id"].as_str() {
                Some(tx) => SignerReply::Funded {
                    tx_id: tx.to_string(),
                    amount_sun: body["amount_sun"].as_i64().unwrap_or(0),
                },
                None => SignerReply::Failed("signer reported funded with no tx_id".into()),
            },
            Some("fee_account_dry") => SignerReply::FeeAccountDry {
                fee_address: body["fee_address"].as_str().unwrap_or("unknown").to_string(),
                have_sun: body["have_sun"].as_i64().unwrap_or(0),
                need_sun: body["need_sun"].as_i64().unwrap_or(0),
            },
            // An unknown status must never be treated as benign: it could mean a newer signer swept
            // in a way this version does not understand.
            other => SignerReply::Failed(format!("unrecognised signer status {other:?}")),
        }
    }
}

/// Spawned once from `main.rs`.
pub async fn run(pool: PgPool, config: AppConfig, interval_secs: u64) {
    let client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
    let signer = HttpSigner {
        http: reqwest::Client::new(),
        base_url: config.signer_url.clone(),
        token: config.signer_token.clone(),
    };
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    loop {
        ticker.tick().await;
        sweep_once(&pool, &config, &client, &signer).await;
    }
}
