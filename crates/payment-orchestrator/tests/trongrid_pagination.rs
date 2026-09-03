//! Task 12: TronGrid's pagination cursor (`meta.fingerprint`). Wiremock only — no database, since
//! `transfers_to` never touches Postgres. See custody.rs's module docs and its `MAX_PAGES` doc
//! comment for why a full page must be followed onward instead of silently truncated.

use payment_orchestrator::custody::{CustodyWatcher, ObservedTransfer, TronGridWatcher};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADDR: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
/// A different address than `ADDR`, used for filler rows: `rows_to_transfers`'s recipient check
/// drops every one of them, so a correct implementation's result contains only the row that
/// actually matters, not 200 unrelated entries.
const OTHER: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";

fn row_json(tx_id: &str, to: &str, contract: &str, value: &str, event_type: &str, block_timestamp: i64) -> serde_json::Value {
    json!({
        "transaction_id": tx_id,
        "to": to,
        "value": value,
        "type": event_type,
        "token_info": { "address": contract },
        "block_timestamp": block_timestamp,
    })
}

/// Against the pre-fix code this fails: `transfers_to` sends one request, sees a full page, warns,
/// and returns — the second page, and the one real transfer sitting on it, is never fetched.
#[tokio::test]
async fn the_second_page_is_fetched_when_the_first_is_full() {
    let server = MockServer::start().await;

    // 200 filler rows to OTHER, not ADDR — `rows_to_transfers`'s recipient filter drops every one,
    // so the only way this test's assertion below can pass is if page 2 was actually fetched.
    let filler: Vec<serde_json::Value> = (0..200)
        .map(|i| row_json(&format!("filler-{i}"), OTHER, USDT, "1000000", "Transfer", 1_700_000_000_000))
        .collect();

    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{ADDR}/transactions/trc20")))
        .and(query_param_is_missing("fingerprint"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": filler,
            "meta": { "fingerprint": "abc" },
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{ADDR}/transactions/trc20")))
        .and(query_param("fingerprint", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [row_json("tx-real", ADDR, USDT, "5000000", "Transfer", 1_700_000_001_000)],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let watcher = TronGridWatcher::new(server.uri(), "test-key".into(), USDT.into());
    let got = watcher.transfers_to(ADDR, None).await.unwrap();

    assert_eq!(
        got,
        vec![ObservedTransfer {
            tx_id: "tx-real".into(),
            amount_usdt: 5_000_000,
            to: ADDR.into(),
            contract: USDT.into(),
            block_timestamp: 1_700_000_001_000,
        }],
        "the real transfer sitting on page 2 must be returned"
    );
    // Both `.expect(1)`s above are verified automatically when `server` drops at the end of this
    // function — page 2 never being requested (the pre-fix behaviour) panics there too.
}

/// A page that never shortens and never stops offering a fingerprint — a misbehaving or
/// adversarial upstream. Must stop at the cap with a loud `Err`, not loop forever and not return a
/// silently partial `Ok`. Against the pre-fix code this fails as well: there is no loop or cap to
/// hit, so `transfers_to` returns `Ok` after exactly one request instead of `Err` after ten.
#[tokio::test]
async fn an_endless_fingerprint_stops_at_the_page_cap_with_an_error() {
    let server = MockServer::start().await;

    let full_page: Vec<serde_json::Value> = (0..200)
        .map(|i| row_json(&format!("row-{i}"), ADDR, USDT, "1000000", "Transfer", 1_700_000_000_000))
        .collect();

    // Matches every request regardless of whether it carries a `fingerprint`, and always answers
    // with another full page plus another fingerprint — there is never a short page or an absent
    // cursor to stop on.
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{ADDR}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": full_page,
            "meta": { "fingerprint": "never-ending" },
        })))
        .expect(10)
        .mount(&server)
        .await;

    let watcher = TronGridWatcher::new(server.uri(), "test-key".into(), USDT.into());
    let err = watcher.transfers_to(ADDR, None).await.unwrap_err();

    assert!(err.contains(ADDR), "the error must name the address: {err}");
    assert!(err.contains("10"), "the error must name the page cap: {err}");
    // `.expect(10)` above is verified when `server` drops: anything other than exactly MAX_PAGES
    // requests — an off-by-one in the loop bound, or no cap at all — panics there.
}
