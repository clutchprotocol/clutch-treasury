//! Test-support module (T4 brief): a scriptable `PaymentAdapter` so `db_webhook.rs` can drive
//! `apply_invoice_update`/the poller through every `InvoiceState` without a live Bitcart or
//! wiremock server — this crate never trusts anything but the refetched state, so the tests
//! that matter are "given this state comes back from `get_invoice`, what happens," and this
//! adapter is exactly that knob.
//!
//! `tests/support/mod.rs` (not `tests/support.rs` or a top-level `tests/fake_adapter.rs`) is
//! deliberate: cargo treats every direct `tests/*.rs` file as its own test binary, so a plain
//! `tests/fake_adapter.rs` would compile as an (empty, pointless) test crate of its own. The
//! `support/mod.rs` shape opts this file out of that and makes it `mod support;`-includable.

use async_trait::async_trait;
use payment_orchestrator::adapter::{InvoiceState, InvoiceStatus, PaymentAdapter, PaymentInstructions};
use std::collections::HashMap;
use std::sync::Mutex;

/// Scripted responses keyed by invoice_id. `get_invoice` on an id with no script panics loudly
/// rather than returning some default — a test that forgot to script an id should fail fast,
/// not silently observe `Pending` and pass anyway.
pub struct FakeAdapter {
    scripts: Mutex<HashMap<String, InvoiceStatus>>,
    calls: Mutex<HashMap<String, u32>>,
}

impl FakeAdapter {
    pub fn new() -> Self {
        Self { scripts: Mutex::new(HashMap::new()), calls: Mutex::new(HashMap::new()) }
    }

    /// Sets (or overwrites) what `get_invoice(invoice_id)` returns on every subsequent call —
    /// "overwrites" is what lets a test simulate a late confirm: script `Expired` first, poll,
    /// then re-script the SAME id to `Confirmed` and poll again.
    pub fn script(&self, invoice_id: &str, state: InvoiceState, tron_tx_id: Option<&str>) {
        self.scripts.lock().unwrap().insert(
            invoice_id.to_string(),
            InvoiceStatus { state, tron_tx_id: tron_tx_id.map(str::to_string) },
        );
    }

    /// How many times `get_invoice` was actually called for this id — lets a test assert the
    /// poller/webhook genuinely refetched rather than trusting a payload-only shortcut.
    pub fn call_count(&self, invoice_id: &str) -> u32 {
        *self.calls.lock().unwrap().get(invoice_id).unwrap_or(&0)
    }
}

#[async_trait]
impl PaymentAdapter for FakeAdapter {
    async fn create_invoice(
        &self,
        order_id: &str,
        pay_amount_usdt: i64,
        _notification_url: &str,
    ) -> Result<PaymentInstructions, String> {
        Ok(PaymentInstructions {
            invoice_id: format!("inv-{order_id}"),
            pay_address: "TCustodyAddressXXXXXXXXXXXXXXXXXXX".to_string(),
            pay_amount_usdt,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
        })
    }

    async fn get_invoice(&self, invoice_id: &str) -> Result<InvoiceStatus, String> {
        *self.calls.lock().unwrap().entry(invoice_id.to_string()).or_insert(0) += 1;
        self.scripts
            .lock()
            .unwrap()
            .get(invoice_id)
            .cloned()
            .ok_or_else(|| format!("FakeAdapter: no script set for invoice_id '{invoice_id}' — test bug"))
    }
}
