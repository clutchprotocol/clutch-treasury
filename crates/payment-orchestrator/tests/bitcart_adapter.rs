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
