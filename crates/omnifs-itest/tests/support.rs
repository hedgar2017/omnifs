//! The shared helpers of a provider test, over the in-tree test provider.

#![cfg(not(target_os = "wasi"))]

use omnifs_engine::NsError;
use omnifs_itest::support::{Answer, Request, answering, decoded, lookup, names, read, served};
use omnifs_itest::{RuntimeHarness, TEST_PROVIDER_CONFIG};
use omnifs_wit::host::types::{CalloutResult, HttpResponse};

#[tokio::test(flavor = "multi_thread")]
async fn an_operation_takes_the_answers_of_its_callouts_and_gives_their_requests() {
    let harness = RuntimeHarness::new(TEST_PROVIDER_CONFIG).unwrap();
    let (body, requests) = answering(
        &harness,
        read(&harness, "/test/hello/remote-a"),
        [Answer::Fetch(CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: b"answered".to_vec(),
        }))],
    )
    .await;
    assert_eq!(body.unwrap(), "answered");
    let [Request::Fetch(request)] = requests.as_slice() else {
        panic!("one fetch, got {requests:?}");
    };
    assert_eq!(request.url, "https://httpbin.org/get");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_served_operation_makes_no_callout() {
    let harness = RuntimeHarness::new(TEST_PROVIDER_CONFIG).unwrap();
    let hello = served(&harness, names(&harness, "/test/hello"))
        .await
        .unwrap();
    assert!(hello.is_sorted(), "{hello:?}");
    assert!(hello.iter().any(|name| name == "message"), "{hello:?}");
    assert_eq!(
        served(&harness, read(&harness, "/test/hello/message"))
            .await
            .unwrap(),
        "Hello, world!"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_stops_at_its_first_missing_segment() {
    let harness = RuntimeHarness::new(TEST_PROVIDER_CONFIG).unwrap();
    assert!(!lookup(&harness, "/test/hello").await.unwrap().is_missing());
    assert!(
        lookup(&harness, "/test/hello/absent/below")
            .await
            .unwrap()
            .is_missing()
    );
    assert!(matches!(
        read(&harness, "/test/hello/absent").await,
        Err(NsError::NotFound)
    ));
}

#[test]
fn a_part_of_a_query_decodes_its_escapes_and_its_spaces() {
    assert_eq!(decoded("a+b%20%E2%82%AC%2B"), "a b €+");
}
