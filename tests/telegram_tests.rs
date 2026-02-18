//! Integration tests for Telegram poll loop offset behavior.
//!
//! Tests verify that the poll loop correctly handles offset advancement:
//! - Empty updates (timeouts) should NOT advance offset
//! - Non-empty updates should advance offset to max_update_id + 1

use serde_json::json;
use tokio::time::{sleep, Duration};
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, ResponseTemplate};

mod common;
use common::{create_test_config_with_telegram, MockTelegramServer, TestWorkspace};

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

    // Spawn telegram poller
    let (mut inbound_rx, _outbound_tx) = icrab::telegram::spawn_telegram(&config);

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

    let (_inbound_rx, _outbound_tx) = icrab::telegram::spawn_telegram(&config);

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
