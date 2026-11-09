// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod test {
    use std::time::Duration;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::util::inflight::InflightOutput;
    use lore_revision::util::inflight::RequestRole;

    include!("helper.rs");

    #[test]
    fn first_request_returns_request_maker() {
        let inflight = InflightOutput::<&str, String>::default();
        assert!(matches!(
            inflight.request("a"),
            RequestRole::RequestMaker(_)
        ));
    }

    #[test]
    fn concurrent_request_returns_result_awaiter() {
        let inflight = InflightOutput::<&str, String>::default();
        let _guard = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker"),
        };
        assert!(matches!(
            inflight.request("a"),
            RequestRole::ResultAwaiter(_)
        ));
    }

    #[tokio::test]
    async fn broadcast_delivers_to_awaiter() {
        let inflight = InflightOutput::<&str, String>::default();

        let guard = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker"),
        };
        let mut receiver = match inflight.request("a") {
            RequestRole::ResultAwaiter(r) => r,
            _ => panic!("expected ResultAwaiter"),
        };

        guard.broadcast(&"hello".to_string());
        assert_eq!(receiver.recv().await.unwrap(), "hello");
    }

    #[test]
    fn broadcast_removes_entry_so_next_request_is_maker() {
        let inflight = InflightOutput::<&str, String>::default();

        let guard = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker"),
        };
        guard.broadcast(&"done".to_string());

        // The entry was removed — next request for the same key must be a fresh RequestMaker
        assert!(matches!(
            inflight.request("a"),
            RequestRole::RequestMaker(_)
        ));
    }

    #[test]
    fn drop_without_broadcast_removes_entry() {
        let inflight = InflightOutput::<&str, String>::default();

        let guard = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker"),
        };
        drop(guard);

        // Entry cleaned up on drop — next caller retries as RequestMaker
        assert!(matches!(
            inflight.request("a"),
            RequestRole::RequestMaker(_)
        ));
    }

    #[tokio::test]
    async fn sequential_request_after_broadcast_does_not_hang() {
        let inflight = InflightOutput::<&str, String>::default();

        // First request completes normally
        let guard = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker"),
        };
        guard.broadcast(&"first".to_string());

        // Second request for the same key should get a fresh RequestMaker (not hang)
        let result = tokio::time::timeout(Duration::from_millis(100), async {
            match inflight.request("a") {
                RequestRole::RequestMaker(g) => {
                    g.broadcast(&"second".to_string());
                    "second".to_string()
                }
                RequestRole::ResultAwaiter(mut r) => r.recv().await.unwrap(),
                RequestRole::Ready(result) => result,
            }
        })
        .await;

        assert_eq!(result.unwrap(), "second");
    }

    #[test]
    fn distinct_keys_are_independent() {
        let inflight = InflightOutput::<&str, String>::default();
        let _guard_a = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker for a"),
        };
        assert!(matches!(
            inflight.request("b"),
            RequestRole::RequestMaker(_)
        ));
    }

    #[tokio::test]
    async fn drop_without_broadcast_wakes_awaiter_with_error() {
        let inflight = InflightOutput::<&str, String>::default();

        let guard = match inflight.request("a") {
            RequestRole::RequestMaker(g) => g,
            _ => panic!("expected RequestMaker"),
        };
        let mut receiver = match inflight.request("a") {
            RequestRole::ResultAwaiter(r) => r,
            _ => panic!("expected ResultAwaiter"),
        };

        // Drop the guard without broadcasting — sender is dropped, receiver gets Closed
        drop(guard);
        assert!(receiver.recv().await.is_err());
    }

    /// A result broadcast and held reaches the callers waiting for it as sent, and every caller
    /// after them as held until the hold is dropped; the next caller then produces the result
    /// again.
    #[tokio::test]
    async fn a_held_value_is_handed_to_every_caller_after_the_sent_one_until_its_hold_drops() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let inflight = InflightOutput::<&str, String>::default();
                let guard = match inflight.request("a") {
                    RequestRole::RequestMaker(g) => g,
                    _ => panic!("expected RequestMaker"),
                };
                let mut receiver = match inflight.request("a") {
                    RequestRole::ResultAwaiter(r) => r,
                    _ => panic!("expected ResultAwaiter"),
                };

                let hold = guard.broadcast_and_hold(&"sent".to_string(), "held".to_string());

                assert_eq!(receiver.recv().await.unwrap(), "sent");
                for _ in 0..2 {
                    assert!(matches!(
                        inflight.request("a"),
                        RequestRole::Ready(result) if result == "held"
                    ));
                }
                assert!(matches!(
                    inflight.request("b"),
                    RequestRole::RequestMaker(_)
                ));
                drop(hold);
                assert!(matches!(
                    inflight.request("a"),
                    RequestRole::RequestMaker(_)
                ));
            })
            .await;
    }

    /// A key settles once nothing is in production or held for it: at once for a key with
    /// nothing in flight, and for one in flight once its result is sent and its hold dropped.
    #[tokio::test]
    async fn a_key_settles_once_its_result_is_sent_and_its_hold_dropped() {
        LORE_CONTEXT
            .scope(setup_test_execution(), async {
                let inflight = InflightOutput::<&str, String>::default();
                tokio::time::timeout(Duration::from_secs(5), inflight.settled(&"a"))
                    .await
                    .expect("a key with nothing in flight is settled");

                let guard = match inflight.request("a") {
                    RequestRole::RequestMaker(g) => g,
                    _ => panic!("expected RequestMaker"),
                };
                let mut settling = std::pin::pin!(inflight.settled(&"a"));
                assert!(
                    futures::poll!(settling.as_mut()).is_pending(),
                    "in production"
                );

                let hold = guard.broadcast_and_hold(&"sent".to_string(), "held".to_string());
                assert!(futures::poll!(settling.as_mut()).is_pending(), "held");

                drop(hold);
                tokio::time::timeout(Duration::from_secs(5), settling)
                    .await
                    .expect("the key settles once its hold drops");
            })
            .await;
    }
}
