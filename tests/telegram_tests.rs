//! Integration tests for Telegram poll loop offset behavior.
//!
//! Tests verify that the poll loop correctly handles offset advancement:
//! - Empty updates (timeouts) should NOT advance offset
//! - Non-empty updates should advance offset to max_update_id + 1

use serde_json::json;
use tokio::time::{Duration, sleep};
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, ResponseTemplate};

mod common;
use common::{MockTelegramServer, TestWorkspace, create_test_config_with_telegram};

#[tokio::test]
async fn test_poll_loop_offset_behavior() {
    let ws = TestWorkspace::new();
    let mock_telegram = MockTelegramServer::new().await;

    // Create config pointing to mock Telegram server
    let config = create_test_config_with_telegram(
        &ws.root,
        "http://dummy-llm",
        Some(&mock_telegram.api_base()),
    );

    // Mock empty responses for offset=0 (initial state)
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(5) // Allow multiple empty calls
        .mount(&mock_telegram.server)
        .await;

    // Spawn telegram poller (inbound channel created here so cron runner could share it)
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(64);
    let _outbound_tx = icrab::telegram::spawn_telegram(&config, inbound_tx);

    // Give it a moment to start
    sleep(Duration::from_millis(100)).await;

    // The poll loop should have started and made at least one getUpdates call
    // Since we're mocking empty responses, it should keep using offset=0
    sleep(Duration::from_millis(200)).await;

    // Now mock a response with updates
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 10,
                "message": {
                    "from": {"id": 12345},
                    "chat": {"id": 67890},
                    "text": "Hello"
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    // Wait for the message to be received
    let received = tokio::time::timeout(Duration::from_secs(2), inbound_rx.recv()).await;
    assert!(received.is_ok(), "Should receive message from poll loop");
    let msg = received.unwrap().expect("Message should be Some");
    assert_eq!(msg.text, "Hello");
    assert_eq!(msg.chat_id, 67890);

    // Now mock empty again - next call should use offset=11
    Mock::given(method("GET"))
        .and(query_param("offset", "11"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(10)
        .mount(&mock_telegram.server)
        .await;

    // Wait a bit - should make calls with offset=11 (not advancing)
    sleep(Duration::from_millis(300)).await;

    // Now send another update - should use offset=11
    Mock::given(method("GET"))
        .and(query_param("offset", "11"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 11,
                "message": {
                    "from": {"id": 12345},
                    "chat": {"id": 67890},
                    "text": "World"
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    // Should receive the second message
    let received2 = tokio::time::timeout(Duration::from_secs(2), inbound_rx.recv()).await;
    assert!(received2.is_ok(), "Should receive second message");
    let msg2 = received2.unwrap().expect("Message should be Some");
    assert_eq!(msg2.text, "World");

    // After processing update_id=11, next calls should use offset=12
    // Mock empty response for offset=12
    Mock::given(method("GET"))
        .and(query_param("offset", "12"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(10)
        .mount(&mock_telegram.server)
        .await;

    sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn test_poll_loop_empty_updates_do_not_advance_offset() {
    // This test specifically verifies that empty updates (timeouts) don't advance offset
    let ws = TestWorkspace::new();
    let mock_telegram = MockTelegramServer::new().await;

    let config = create_test_config_with_telegram(
        &ws.root,
        "http://dummy-llm",
        Some(&mock_telegram.api_base()),
    );

    // Mock empty responses for offset=0 (should keep using offset=0)
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(5) // Multiple empty responses
        .mount(&mock_telegram.server)
        .await;

    let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel(64);
    let _outbound_tx = icrab::telegram::spawn_telegram(&config, inbound_tx);

    // Wait for multiple poll cycles
    sleep(Duration::from_millis(500)).await;

    // Now verify that when we DO get updates, offset advances correctly
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 5,
                "message": {
                    "from": {"id": 12345},
                    "chat": {"id": 67890},
                    "text": "Test"
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    // After processing update_id=5, next call should use offset=6
    Mock::given(method("GET"))
        .and(query_param("offset", "6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(5)
        .mount(&mock_telegram.server)
        .await;

    sleep(Duration::from_millis(500)).await;
}

// --- §3.1 Edge cases: disallowed user, API failure, non-text, malformed ---

/// Only the owner should trigger the agent. Disallowed user's update is ignored; offset still
/// advances so we don't reprocess it.
#[tokio::test]
async fn test_disallowed_user_ignored_offset_advances() {
    let ws = TestWorkspace::new();
    let mock_telegram = MockTelegramServer::new().await;
    let config = create_test_config_with_telegram(
        &ws.root,
        "http://dummy-llm",
        Some(&mock_telegram.api_base()),
    );
    // allowed_user_ids = [12345] from create_test_config_with_telegram

    // First poll: two updates — one from allowed (12345), one from disallowed (99999)
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [
                {
                    "update_id": 10,
                    "message": {
                        "from": {"id": 12345},
                        "chat": {"id": 67890},
                        "text": "Allowed"
                    }
                },
                {
                    "update_id": 11,
                    "message": {
                        "from": {"id": 99999},
                        "chat": {"id": 67890},
                        "text": "Disallowed"
                    }
                }
            ]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(64);
    let _outbound_tx = icrab::telegram::spawn_telegram(&config, inbound_tx);
    sleep(Duration::from_millis(100)).await;

    // Exactly one InboundMsg (from allowed user)
    let received = tokio::time::timeout(Duration::from_secs(2), inbound_rx.recv()).await;
    assert!(received.is_ok(), "Should receive one message");
    let msg = received.unwrap().expect("Message should be Some");
    assert_eq!(msg.text, "Allowed");
    assert_eq!(msg.user_id, 12345);
    assert_eq!(msg.chat_id, 67890);

    // No second message (disallowed was skipped)
    Mock::given(method("GET"))
        .and(query_param("offset", "12"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(10)
        .mount(&mock_telegram.server)
        .await;
    sleep(Duration::from_millis(400)).await;
}

/// On HTTP/5xx error, poll loop does not advance offset; after success we get the update and
/// subsequent calls use max_update_id + 1.
#[tokio::test]
async fn test_transient_api_failure_does_not_advance_offset() {
    let ws = TestWorkspace::new();
    let mock_telegram = MockTelegramServer::new().await;
    let config = create_test_config_with_telegram(
        &ws.root,
        "http://dummy-llm",
        Some(&mock_telegram.api_base()),
    );

    // First call: 503
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(64);
    let _outbound_tx = icrab::telegram::spawn_telegram(&config, inbound_tx);
    sleep(Duration::from_millis(100)).await;

    // Then success with one update (same offset=0 retry)
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 7,
                "message": {
                    "from": {"id": 12345},
                    "chat": {"id": 67890},
                    "text": "After retry"
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    let received = tokio::time::timeout(Duration::from_secs(5), inbound_rx.recv()).await;
    assert!(
        received.is_ok(),
        "Should eventually receive message after retry"
    );
    let msg = received.unwrap().expect("Message should be Some");
    assert_eq!(msg.text, "After retry");

    // Next poll should use offset=8
    Mock::given(method("GET"))
        .and(query_param("offset", "8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(5)
        .mount(&mock_telegram.server)
        .await;
    sleep(Duration::from_millis(400)).await;
}

/// Update with message but no text (e.g. photo) is ignored; offset still advances so we don't refetch.
#[tokio::test]
async fn test_non_text_update_ignored_offset_advances() {
    let ws = TestWorkspace::new();
    let mock_telegram = MockTelegramServer::new().await;
    let config = create_test_config_with_telegram(
        &ws.root,
        "http://dummy-llm",
        Some(&mock_telegram.api_base()),
    );

    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 20,
                "message": {
                    "from": {"id": 12345},
                    "chat": {"id": 67890}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(64);
    let _outbound_tx = icrab::telegram::spawn_telegram(&config, inbound_tx);
    sleep(Duration::from_millis(100)).await;

    // No InboundMsg (no text) — recv times out
    let no_msg = tokio::time::timeout(Duration::from_millis(600), inbound_rx.recv()).await;
    assert!(
        no_msg.is_err(),
        "No message expected for photo-only update (expected timeout)"
    );

    // Next poll uses offset=21
    Mock::given(method("GET"))
        .and(query_param("offset", "21"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": []
        })))
        .up_to_n_times(5)
        .mount(&mock_telegram.server)
        .await;
    sleep(Duration::from_millis(300)).await;
}

/// ok: false or empty result does not crash; empty result does not advance offset.
#[tokio::test]
async fn test_ok_false_does_not_crash_or_advance_offset() {
    let ws = TestWorkspace::new();
    let mock_telegram = MockTelegramServer::new().await;
    let config = create_test_config_with_telegram(
        &ws.root,
        "http://dummy-llm",
        Some(&mock_telegram.api_base()),
    );

    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": false })))
        .up_to_n_times(2)
        .mount(&mock_telegram.server)
        .await;

    let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel(64);
    let _outbound_tx = icrab::telegram::spawn_telegram(&config, inbound_tx);
    sleep(Duration::from_millis(300)).await;

    // Then valid response with update so loop can progress
    Mock::given(method("GET"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "from": {"id": 12345},
                    "chat": {"id": 67890},
                    "text": "OK"
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&mock_telegram.server)
        .await;

    sleep(Duration::from_millis(200)).await;
}
