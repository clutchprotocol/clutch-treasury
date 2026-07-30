//! wiremock coverage for BitcartAdapter: asserts the exact request we send (T3 brief's
//! "assert the field in the wiremock body test" for the expiration, and the decisive
//! watch-only property — we forward `pay_amount_usdt` verbatim, never compute it) and
//! feeds canned Bitcart responses to prove the status-mapping table end to end.

use payment_orchestrator::adapter::{BitcartAdapter, InvoiceState, PaymentAdapter};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn adapter(base_url: String) -> BitcartAdapter {
    BitcartAdapter {
        http: reqwest::Client::new(),
        base_url,
        token: "test-bitcart-token".to_string(),
        store_id: "store-123".to_string(),
        deposit_ttl_minutes: 30,
        invoice_currency: "USDT".to_string(),
    }
}

/// Asserts the exact POST body (price built by integer math, store_id, order_id,
/// notification_url) and the Bearer header — not just that SOME request landed.
#[tokio::test]
async fn create_invoice_sends_exact_body_and_bearer_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/invoices"))
        .and(header("authorization", "Bearer test-bitcart-token"))
        .and(body_json(json!({
            "price": "5.000173",
            "store_id": "store-123",
            "order_id": "order-abc",
            "notification_url": "https://orchestrator.example/webhooks/bitcart",
            "currency": "USDT",
            "expiration": 30,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "inv-1",
            "status": "pending",
            "payments": [{"payment_address": "TCustodyAddressXXXXXXXXXXXXXXXXXXX"}],
            "expiration_seconds": 1800,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let a = adapter(server.uri());
    let instructions = a
        .create_invoice("order-abc", 5_000_173, "https://orchestrator.example/webhooks/bitcart")
        .await
        .unwrap();

    assert_eq!(instructions.invoice_id, "inv-1");
    assert_eq!(instructions.pay_address, "TCustodyAddressXXXXXXXXXXXXXXXXXXX");
    // The decisive watch-only property: the adapter forwards the EXACT amount it was given
    // (T2's discriminator already made it unique) — it must never recompute or perturb it.
    assert_eq!(instructions.pay_amount_usdt, 5_000_173);
    assert!(instructions.expires_at > chrono::Utc::now());
}

/// Store-level/invoice expiration must be passed explicitly — Bitcart's own default window
/// must not diverge from our advertised pay-in window (deposit_ttl_minutes). Uses a TTL
/// distinct from the shared `adapter()` helper's (45 min, not 30) so the assertion actually
/// proves the configured value is threaded through to the request, not a coincidental match
/// against a hardcoded default.
#[tokio::test]
async fn create_invoice_asserts_expiration_field_in_request() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/invoices"))
        .and(body_json(json!({
            "price": "1.000000",
            "store_id": "store-123",
            "order_id": "order-ttl",
            "notification_url": "https://orchestrator.example/webhooks/bitcart",
            "currency": "USDT",
            "expiration": 45,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "inv-ttl",
            "status": "pending",
            "payments": [{"payment_address": "TCustodyAddressXXXXXXXXXXXXXXXXXXX"}],
            "expiration_seconds": 2700,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let a = BitcartAdapter {
        http: reqwest::Client::new(),
        base_url: server.uri(),
        token: "test-bitcart-token".to_string(),
        store_id: "store-123".to_string(),
        deposit_ttl_minutes: 45,
        invoice_currency: "USDT".to_string(),
    };
    a.create_invoice("order-ttl", 1_000_000, "https://orchestrator.example/webhooks/bitcart")
        .await
        .unwrap();
}

#[tokio::test]
async fn get_invoice_maps_plain_statuses() {
    let server = MockServer::start().await;
    for (status, expected) in [
        ("pending", InvoiceState::Pending),
        ("paid", InvoiceState::Paid),
        ("confirmed", InvoiceState::Confirmed),
        ("complete", InvoiceState::Confirmed),
        ("expired", InvoiceState::Expired),
        ("invalid", InvoiceState::Invalid),
        ("refunded", InvoiceState::Refunded),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/invoices/inv-{status}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": format!("inv-{status}"),
                "status": status,
                "payments": [],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let a = adapter(server.uri());
        let result = a.get_invoice(&format!("inv-{status}")).await.unwrap();
        assert_eq!(result.state, expected, "status {status} mapped wrong");
        assert_eq!(result.tron_tx_id, None);
    }
}

#[tokio::test]
async fn get_invoice_maps_exception_sub_statuses() {
    let server = MockServer::start().await;
    for (exc, expected) in [
        ("paid_partial", InvoiceState::PaidPartial),
        ("paid_over", InvoiceState::PaidOver),
        ("failed_confirm", InvoiceState::FailedConfirm),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/invoices/inv-{exc}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": format!("inv-{exc}"),
                "status": "paid",
                "exception_status": exc,
                "payments": [],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let a = adapter(server.uri());
        let result = a.get_invoice(&format!("inv-{exc}")).await.unwrap();
        assert_eq!(result.state, expected, "exception {exc} mapped wrong");
    }
}

/// The precedence rule end to end, against a real HTTP response, not just the pure
/// function: a `complete` + `paid_over` invoice must map to PaidOver, never Confirmed —
/// otherwise an overpayment is silently treated as an exact payment.
#[tokio::test]
async fn exception_sub_status_overrides_main_status_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-overpaid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-overpaid",
            "status": "complete",
            "exception_status": "paid_over",
            // Top-level `tx_hashes`, per DisplayInvoice at 0.10.3.0 — PaymentMethod has no
            // tx_hash column, so `payments[].tx_hash` (what this test used to assert) was a
            // field that does not exist.
            "tx_hashes": ["abc123tronhash"],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let a = adapter(server.uri());
    let result = a.get_invoice("inv-overpaid").await.unwrap();
    assert_eq!(result.state, InvoiceState::PaidOver, "complete+paid_over must be PaidOver, not Confirmed");
    assert_eq!(result.tron_tx_id, Some("abc123tronhash".to_string()));
}

/// Several hashes against one invoice means several transfers landed. With trx-only there is one
/// payment method, so picking any single hash would hand the verifier evidence that doesn't
/// account for the full amount — leave it None and let the exact-amount fallback (which alerts)
/// deal with it.
#[tokio::test]
async fn get_invoice_returns_no_tx_id_when_several_hashes_are_present() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-multi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-multi",
            "status": "complete",
            "tx_hashes": ["hash-one", "hash-two"],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-multi").await.unwrap();
    assert_eq!(result.state, InvoiceState::Confirmed);
    assert_eq!(result.tron_tx_id, None, "ambiguous evidence must not be presented as a single tx id");
}

/// The request-shape regression guard, stated as its own test because it is the one that bit:
/// `expiration` is MINUTES. Sending seconds turned a 30-minute window into 30 hours.
#[tokio::test]
async fn expiration_is_sent_in_minutes_not_seconds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/invoices"))
        .and(body_json(json!({
            "price": "1.000000",
            "store_id": "store-123",
            "order_id": "order-mins",
            "notification_url": "https://orchestrator.example/webhooks/bitcart",
            "currency": "USDT",
            // 30, not 1800. Bitcart's Invoice.expiration_seconds is `expiration * 60`.
            "expiration": 30,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "inv-mins",
            "status": "pending",
            "payments": [{"payment_address": "TCustodyAddressXXXXXXXXXXXXXXXXXXX"}],
            "expiration_seconds": 1800,
        })))
        .expect(1)
        .mount(&server)
        .await;

    adapter(server.uri())
        .create_invoice("order-mins", 1_000_000, "https://orchestrator.example/webhooks/bitcart")
        .await
        .unwrap();
}

/// No `payments[]` at all means no address to give the user. Fail closed rather than returning
/// instructions that would send their USDT nowhere.
#[tokio::test]
async fn create_invoice_fails_closed_when_no_payment_address_is_returned() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/invoices"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "inv-noaddr",
            "status": "pending",
            "payments": [],
            "expiration_seconds": 1800,
        })))
        .mount(&server)
        .await;

    let err = adapter(server.uri())
        .create_invoice("order-noaddr", 1_000_000, "https://orchestrator.example/webhooks/bitcart")
        .await
        .expect_err("no payment address must be an error, never silently accepted");
    assert!(err.contains("payment_address"), "the error must name what was missing, got: {err}");
}

#[tokio::test]
async fn get_invoice_unmapped_status_becomes_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-weird"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-weird",
            "status": "some_new_bitcart_status",
            "payments": [],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let a = adapter(server.uri());
    let result = a.get_invoice("inv-weird").await.unwrap();
    assert_eq!(result.state, InvoiceState::Unknown("some_new_bitcart_status".to_string()));
}

/// tron_tx_id must come back None, not error, when Bitcart hasn't recorded a payment yet —
/// the adapter tolerates the field's absence rather than treating it as a hard failure.
#[tokio::test]
async fn get_invoice_tolerates_missing_tx_hash() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-no-tx"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-no-tx",
            "status": "pending",
            "payments": [],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let a = adapter(server.uri());
    let result = a.get_invoice("inv-no-tx").await.unwrap();
    assert_eq!(result.tron_tx_id, None);
}

/// Regression guard for a bug only a LIVE Bitcart run exposed: a healthy 0.10.3.0 invoice reports
/// `exception_status: "none"` — a literal string, not null and not "". The model types it
/// `str | None`, which is why this code originally only treated ""/absent as "no exception".
///
/// The consequence was total rather than subtle: every ordinary invoice mapped to
/// `Unknown("none")`, and T4 routes `Unknown` to `needs_manual` + a P1 alert. Every deposit would
/// have been parked for a human and nothing would ever have been credited.
#[tokio::test]
async fn literal_none_exception_status_is_not_an_exception() {
    let server = MockServer::start().await;
    for (status, expected) in [
        ("pending", InvoiceState::Pending),
        ("paid", InvoiceState::Paid),
        ("complete", InvoiceState::Confirmed),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/invoices/none-{status}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": format!("none-{status}"),
                "status": status,
                "exception_status": "none",
                "payments": [],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = adapter(server.uri()).get_invoice(&format!("none-{status}")).await.unwrap();
        assert_eq!(
            result.state, expected,
            "exception_status \"none\" must fall through to the main status, not become Unknown"
        );
    }
}

/// The invoice must be denominated in the payment TOKEN, never the store's fiat default.
///
/// Also live-found. Against a real instance, omitting `currency` under a USD store turned a price
/// of `5.000173` into a stored `5.00` converted at rate `0.33` — rounding away the 6-decimal
/// discriminator (the only thing telling two payers apart on the shared custody address) and
/// applying exactly the conversion arithmetic the integer par peg exists to eliminate. Denominated
/// in the token, the same request comes back `5.000173` at rate `1.000000`.
#[tokio::test]
async fn invoice_is_denominated_in_the_token_so_the_discriminator_survives() {
    let server = MockServer::start().await;
    // wiremock only matches a body carrying the token currency, so if the adapter ever stopped
    // sending it (or sent fiat) the request 404s and create_invoice errors.
    Mock::given(method("POST"))
        .and(path("/invoices"))
        .and(body_json(json!({
            "price": "5.000173",
            "store_id": "store-123",
            "order_id": "order-cur",
            "notification_url": "https://orchestrator.example/webhooks/bitcart",
            "currency": "USDT",
            "expiration": 30,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": "inv-cur",
            "status": "pending",
            "payments": [{"payment_address": "TCustodyAddressXXXXXXXXXXXXXXXXXXX"}],
            "expiration_seconds": 1800,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let got = adapter(server.uri())
        .create_invoice("order-cur", 5_000_173, "https://orchestrator.example/webhooks/bitcart")
        .await
        .expect("must send the token currency");
    assert_eq!(got.pay_amount_usdt, 5_000_173, "the discriminated amount must pass through intact");
}

/// LIVE-FOUND, and the most dangerous of the lot: a real 0.10.3.0 instance reported
/// `exception_status: "paid_over"` for a payment of 2.0 against a 5.000203 invoice. `PaidOver`
/// credits the full INTENDED amount, so believing that label mints CLT no deposit backs.
///
/// The adapter now cross-checks the label against `price` vs `sent_amount` and downgrades to
/// PaidPartial — which routes to needs_manual instead of crediting.
#[tokio::test]
async fn a_fully_paid_label_with_a_short_sent_amount_downgrades_to_partial() {
    let server = MockServer::start().await;
    // Exactly the shape observed live: complete + paid_over, but sent < price.
    Mock::given(method("GET"))
        .and(path("/invoices/inv-liar"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-liar",
            "status": "complete",
            "exception_status": "paid_over",
            "price": "5.000203",
            "sent_amount": 2.0,
            "tx_hashes": ["short-payment-hash"],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-liar").await.unwrap();
    assert_eq!(
        result.state,
        InvoiceState::PaidPartial,
        "a short payment must never be treated as fully paid, whatever the label says"
    );
}

/// The guard only ever downgrades: a genuine overpayment (sent >= price) keeps PaidOver, which
/// credits the intended amount and alerts on the surplus.
#[tokio::test]
async fn a_genuine_overpayment_is_still_paid_over() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-genuine-over"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-genuine-over",
            "status": "complete",
            "exception_status": "paid_over",
            "price": "5.000202",
            "sent_amount": 6.5,
            "tx_hashes": ["real-over-hash"],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-genuine-over").await.unwrap();
    assert_eq!(result.state, InvoiceState::PaidOver, "sent >= price must remain PaidOver");
}

/// An exact payment stays Confirmed — the guard must not downgrade the normal path. Uses the exact
/// values a live instance returned for an exactly-paid invoice.
#[tokio::test]
async fn an_exact_payment_remains_confirmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-exact"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-exact",
            "status": "complete",
            "exception_status": "none",
            "price": "5.000201",
            "sent_amount": 5.000201,
            "tx_hashes": ["livehash-exact"],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-exact").await.unwrap();
    assert_eq!(result.state, InvoiceState::Confirmed);
    assert_eq!(result.tron_tx_id, Some("livehash-exact".to_string()));
}

/// Absent numbers must leave the label alone. The guard exists to add safety from real data, not
/// to manufacture a verdict when the data isn't there.
#[tokio::test]
async fn missing_amounts_leave_the_reported_state_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-noamounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-noamounts",
            "status": "complete",
            "exception_status": "none",
            "tx_hashes": [],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-noamounts").await.unwrap();
    assert_eq!(result.state, InvoiceState::Confirmed);
}

/// The invoice `price` is DISPLAY-formatted; `payments[].amount` is authoritative.
///
/// Live evidence: a 5.000921 request came back with price `"5.00"` (2dp, Bitcart's currency table)
/// while `payments[0].amount` was the exact `"5.000921"`. A short-payment check against `price`
/// would therefore accept a payment 921 micro-USDT light — and the missing part IS the
/// discriminator, the only thing identifying which payer this was.
#[tokio::test]
async fn short_payment_is_measured_against_payment_amount_not_the_display_price() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-displayprice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-displayprice",
            "status": "complete",
            "exception_status": "none",
            // Exactly the live shape: rounded display price, exact per-method amount.
            "price": "5.00",
            "payments": [{"payment_address": "TCustody", "amount": "5.000921"}],
            "sent_amount": 5.0,
            "tx_hashes": ["short-by-the-discriminator"],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-displayprice").await.unwrap();
    assert_eq!(
        result.state,
        InvoiceState::PaidPartial,
        "5.000000 against a 5.000921 requirement is short — comparing to the 5.00 display price \
         would have wrongly accepted it"
    );
}

/// And the exact-payment case at full precision still confirms.
#[tokio::test]
async fn exact_payment_at_full_precision_confirms() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/invoices/inv-exactfull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "inv-exactfull",
            "status": "complete",
            "exception_status": "none",
            "price": "5.00",
            "payments": [{"payment_address": "TCustody", "amount": "5.000921"}],
            "sent_amount": 5.000921,
            "tx_hashes": ["exact-hash"],
        })))
        .mount(&server)
        .await;

    let result = adapter(server.uri()).get_invoice("inv-exactfull").await.unwrap();
    assert_eq!(result.state, InvoiceState::Confirmed);
}
