//! Webhook delivery with the v1 retry policy (ws.md "Outbound
//! notifications" §Webhook): attempts 1..=10 (default 3) retried on
//! {408, 429, 500, 502, 503, 504} + transport errors, other 4xx/5xx not
//! retried, backoff `min(8, 2^(n-1))` seconds + jitter, http(s) URLs only,
//! operator headers cleaned (CR/LF stripped from values, malformed names
//! skipped). Never raises — always returns a [`WebhookOutcome`].
//!
//! No SSRF guard, no HMAC/signature, no idempotency key — v1 parity (the
//! URL is operator-configured; see mod docs).

use std::time::Duration;

use rand::Rng;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::{Method, Url};
use serde_json::Value;

/// Statuses that trigger a retry (v1 parity).
const RETRYABLE_STATUSES: [u16; 6] = [408, 429, 500, 502, 503, 504];
/// Backoff cap in seconds: `min(8, 2^(n-1))`.
const BACKOFF_CAP_SECS: u64 = 8;

/// Operator webhook configuration (global config in v1; the wiring
/// package owns loading/validating it — ranges are clamped here anyway).
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub url: String,
    /// HTTP method; anything unparseable falls back to POST (the default).
    pub method: String,
    /// Operator headers as (name, value) pairs, pre-decryption handled by
    /// the caller (see `seal.rs`).
    pub headers: Vec<(String, String)>,
    /// Per-attempt timeout, clamped to 1..=60 (default 10).
    pub timeout_s: u64,
    /// Total attempts, clamped to 1..=10 (default 3).
    pub max_attempts: u32,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: "POST".to_string(),
            headers: Vec::new(),
            timeout_s: 10,
            max_attempts: 3,
        }
    }
}

/// Delivery result — mirrors v1's `{success, status, attempts, message}`.
#[derive(Debug, Clone)]
pub struct WebhookOutcome {
    pub success: bool,
    /// Last HTTP status seen, `None` when no response ever arrived.
    pub status: Option<u16>,
    pub attempts: u32,
    pub message: String,
}

impl WebhookOutcome {
    fn no_send(message: impl Into<String>) -> Self {
        Self {
            success: false,
            status: None,
            attempts: 0,
            message: message.into(),
        }
    }
}

/// A webhook URL as the history shows it: scheme, host and port only. HN-7 (night test
/// 2026-09-26): webhook URLs often carry their secret in the path or query (Slack, Teams,
/// Zapier), and the history is shown to managers, who may not see the URL field itself.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}/…", u.scheme()),
            (Some(host), None) => format!("{}://{host}/…", u.scheme()),
            _ => "(webhook)".to_string(),
        },
        Err(_) => "(webhook)".to_string(),
    }
}

/// Sends `payload` as JSON to the configured URL per the retry policy.
pub async fn send_webhook(cfg: &WebhookConfig, payload: &Value) -> WebhookOutcome {
    let url = match Url::parse(&cfg.url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url,
        Ok(url) => {
            return WebhookOutcome::no_send(format!(
                "webhook URL must be http(s), got scheme '{}'",
                url.scheme()
            ))
        }
        Err(err) => return WebhookOutcome::no_send(format!("invalid webhook URL: {err}")),
    };

    let body = match serde_json::to_vec(payload) {
        Ok(body) => body,
        Err(err) => return WebhookOutcome::no_send(format!("payload serialization: {err}")),
    };

    let timeout = Duration::from_secs(cfg.timeout_s.clamp(1, 60));
    let max_attempts = cfg.max_attempts.clamp(1, 10);
    let method = Method::from_bytes(cfg.method.trim().to_ascii_uppercase().as_bytes())
        .unwrap_or(Method::POST);
    let mut headers = clean_headers(&cfg.headers);
    // HN-19: one id across every attempt of this delivery, so a receiver that got an attempt
    // the portal gave up on can drop the retry (delivery is at-least-once).
    if let Ok(id) = reqwest::header::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()) {
        headers.insert("x-proximos-delivery-id", id);
    }

    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(err) => return WebhookOutcome::no_send(format!("HTTP client build failed: {err}")),
    };

    let mut last_message = String::new();
    let mut last_status: Option<u16> = None;

    for attempt in 1..=max_attempts {
        let request = client
            .request(method.clone(), url.clone())
            .headers(headers.clone())
            .body(body.clone());

        let retryable = match request.send().await {
            Ok(response) => {
                let status = response.status();
                last_status = Some(status.as_u16());
                if status.is_success() {
                    return WebhookOutcome {
                        success: true,
                        status: last_status,
                        attempts: attempt,
                        message: format!("delivered (HTTP {})", status.as_u16()),
                    };
                }
                last_message = format!("HTTP {}", status.as_u16());
                RETRYABLE_STATUSES.contains(&status.as_u16())
            }
            Err(err) => {
                // Transport errors (connect/timeout/etc.) are always
                // retryable; a status inside the error (redirect loop
                // etc.) is preserved when present.
                last_status = err.status().map(|status| status.as_u16());
                // HN-11: say why, not only which URL.
                last_message = if err.is_timeout() {
                    "request failed: timed out".to_string()
                } else if err.is_connect() {
                    "request failed: could not connect".to_string()
                } else {
                    format!("request failed: {}", err.without_url())
                };
                true
            }
        };

        if !retryable || attempt == max_attempts {
            return WebhookOutcome {
                success: false,
                status: last_status,
                attempts: attempt,
                message: last_message,
            };
        }
        tokio::time::sleep(backoff_delay(attempt)).await;
    }

    // 1..=max_attempts with max_attempts >= 1 always returns inside the
    // loop; keep a sane value anyway rather than panicking.
    WebhookOutcome {
        success: false,
        status: last_status,
        attempts: max_attempts,
        message: last_message,
    }
}

/// Backoff before attempt `n + 1`: `min(8, 2^(n-1))` seconds plus up to
/// one second of jitter.
fn backoff_delay(attempt: u32) -> Duration {
    let base = backoff_secs(attempt);
    let jitter_ms = rand::thread_rng().gen_range(0..1000u64);
    Duration::from_millis(base * 1000 + jitter_ms)
}

/// The deterministic part of the backoff: `min(8, 2^(n-1))` seconds.
fn backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt.saturating_sub(1))
        .min(BACKOFF_CAP_SECS)
}

/// Builds the outgoing header map: `Content-Type: application/json` plus
/// the cleaned operator headers. Values get CR/LF stripped; header names
/// that still don't parse are skipped (v1 parity: skip, don't fail the
/// whole send). Operator headers may override Content-Type.
fn clean_headers(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    for (name, value) in headers {
        let cleaned_value: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        let parsed_name = match HeaderName::from_bytes(name.trim().as_bytes()) {
            Ok(parsed) => parsed,
            Err(_) => {
                tracing::warn!(header = %name, "skipping malformed webhook header name");
                continue;
            }
        };
        match HeaderValue::from_str(&cleaned_value) {
            Ok(parsed_value) => {
                map.insert(parsed_name, parsed_value);
            }
            Err(_) => {
                tracing::warn!(header = %name, "skipping webhook header with unrepresentable value");
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::HeaderMap as AxumHeaderMap;
    use axum::routing::any;
    use axum::Router;
    use parking_lot::Mutex;
    use serde_json::json;

    use super::*;

    /// HN-7: the history shows where a webhook went, not its secret path or query.
    #[test]
    fn a_webhook_url_is_shown_without_its_path() {
        assert_eq!(
            redact_url("https://hooks.slack.com/services/T0/B0/secret?x=1"),
            "https://hooks.slack.com/…"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:9199/hook"),
            "http://127.0.0.1:9199/…"
        );
        assert_eq!(redact_url("not a url"), "(webhook)");
    }

    /// Scripted test endpoint: pops the next status from `script` per hit
    /// (repeating the last one when exhausted) and records each request's
    /// headers + body.
    struct Recorder {
        script: Vec<u16>,
        hits: Vec<(AxumHeaderMap, Value)>,
    }

    async fn spawn_endpoint(script: Vec<u16>) -> (String, Arc<Mutex<Recorder>>) {
        let recorder = Arc::new(Mutex::new(Recorder {
            script,
            hits: Vec::new(),
        }));
        let state = Arc::clone(&recorder);

        let app = Router::new()
            .route(
                "/hook",
                any(
                    |State(recorder): State<Arc<Mutex<Recorder>>>,
                     headers: AxumHeaderMap,
                     body: String| async move {
                        let mut guard = recorder.lock();
                        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        let idx = guard.hits.len().min(guard.script.len() - 1);
                        let status = guard.script[idx];
                        guard.hits.push((headers, parsed));
                        axum::http::StatusCode::from_u16(status).expect("valid status")
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (format!("http://{addr}/hook"), recorder)
    }

    fn cfg(url: String, max_attempts: u32) -> WebhookConfig {
        WebhookConfig {
            url,
            max_attempts,
            ..WebhookConfig::default()
        }
    }

    #[tokio::test]
    async fn delivers_first_try() {
        let (url, recorder) = spawn_endpoint(vec![200]).await;
        let outcome = send_webhook(&cfg(url, 3), &json!({"event": "alarm.test"})).await;
        assert!(outcome.success);
        assert_eq!(outcome.status, Some(200));
        assert_eq!(outcome.attempts, 1);

        let guard = recorder.lock();
        assert_eq!(guard.hits.len(), 1);
        assert_eq!(guard.hits[0].1["event"], "alarm.test");
        assert_eq!(
            guard.hits[0].0.get("content-type").map(|v| v.as_bytes()),
            Some(&b"application/json"[..])
        );
    }

    #[tokio::test]
    async fn retries_retryable_statuses_until_success() {
        let (url, recorder) = spawn_endpoint(vec![500, 503, 200]).await;
        let outcome = send_webhook(&cfg(url, 3), &json!({"n": 1})).await;
        assert!(outcome.success, "{}", outcome.message);
        assert_eq!(outcome.attempts, 3);
        assert_eq!(outcome.status, Some(200));
        assert_eq!(recorder.lock().hits.len(), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let (url, recorder) = spawn_endpoint(vec![503]).await;
        let outcome = send_webhook(&cfg(url, 2), &json!({"n": 1})).await;
        assert!(!outcome.success);
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.status, Some(503));
        assert_eq!(outcome.message, "HTTP 503");
        assert_eq!(recorder.lock().hits.len(), 2);
    }

    #[tokio::test]
    async fn non_retryable_4xx_fails_immediately() {
        let (url, recorder) = spawn_endpoint(vec![404]).await;
        let outcome = send_webhook(&cfg(url, 5), &json!({"n": 1})).await;
        assert!(!outcome.success);
        assert_eq!(outcome.attempts, 1, "404 must not be retried");
        assert_eq!(outcome.status, Some(404));
        assert_eq!(recorder.lock().hits.len(), 1);
    }

    #[tokio::test]
    async fn retryable_408_and_429_are_retried() {
        for status in [408u16, 429] {
            let (url, recorder) = spawn_endpoint(vec![status, 200]).await;
            let outcome = send_webhook(&cfg(url, 3), &json!({"n": 1})).await;
            assert!(outcome.success, "status {status}: {}", outcome.message);
            assert_eq!(outcome.attempts, 2, "status {status}");
            assert_eq!(recorder.lock().hits.len(), 2);
        }
    }

    #[tokio::test]
    async fn transport_error_is_retried_and_never_raises() {
        // Bind-then-drop leaves a port nothing listens on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let outcome = send_webhook(&cfg(format!("http://{addr}/hook"), 2), &json!({"n": 1})).await;
        assert!(!outcome.success);
        assert_eq!(outcome.attempts, 2, "transport errors are retryable");
        assert_eq!(outcome.status, None);
        assert!(outcome.message.contains("request failed"));
    }

    #[tokio::test]
    async fn rejects_non_http_schemes_without_sending() {
        for url in ["ftp://example.com/hook", "file:///etc/passwd", "not a url"] {
            let outcome = send_webhook(&cfg(url.to_string(), 3), &json!({})).await;
            assert!(!outcome.success, "{url}");
            assert_eq!(outcome.attempts, 0, "{url}: nothing must be sent");
            assert_eq!(outcome.status, None);
        }
    }

    #[tokio::test]
    async fn header_cleaning_strips_crlf_and_skips_malformed_names() {
        let (url, recorder) = spawn_endpoint(vec![200]).await;
        let mut config = cfg(url, 1);
        config.headers = vec![
            (
                "X-Token".to_string(),
                "secret\r\nX-Injected: evil".to_string(),
            ),
            ("Bad Name".to_string(), "value".to_string()), // space → invalid, skipped
            ("X-Plain".to_string(), "ok".to_string()),
        ];
        let outcome = send_webhook(&config, &json!({})).await;
        assert!(outcome.success, "{}", outcome.message);

        let guard = recorder.lock();
        let headers = &guard.hits[0].0;
        assert_eq!(
            headers.get("x-token").map(|v| v.as_bytes()),
            Some(&b"secretX-Injected: evil"[..]),
            "CR/LF stripped, remainder kept as a plain value"
        );
        assert!(headers.get("x-injected").is_none(), "no header injection");
        assert!(headers.get("bad name").is_none());
        assert_eq!(
            headers.get("x-plain").map(|v| v.as_bytes()),
            Some(&b"ok"[..])
        );
    }

    #[tokio::test]
    async fn method_from_config_with_post_fallback() {
        let (url, recorder) = spawn_endpoint(vec![200, 200]).await;

        let mut config = cfg(url.clone(), 1);
        config.method = "put".to_string();
        assert!(send_webhook(&config, &json!({})).await.success);

        config.method = "n o t".to_string(); // unparseable → POST
        assert!(send_webhook(&config, &json!({})).await.success);

        // Axum's `any` route accepted both; nothing more to assert about
        // the method from this side beyond "both were delivered".
        assert_eq!(recorder.lock().hits.len(), 2);
    }

    #[test]
    fn backoff_table() {
        assert_eq!(backoff_secs(1), 1);
        assert_eq!(backoff_secs(2), 2);
        assert_eq!(backoff_secs(3), 4);
        assert_eq!(backoff_secs(4), 8);
        assert_eq!(backoff_secs(5), 8, "capped at 8 s");
        assert_eq!(backoff_secs(9), 8);

        for attempt in 1..=9 {
            let d = backoff_delay(attempt);
            let base = Duration::from_secs(backoff_secs(attempt));
            assert!(
                d >= base && d < base + Duration::from_secs(1),
                "jitter < 1 s"
            );
        }
    }
}
