//! Email transports: SMTP (lettre, rustls) and SendGrid (reqwest, v3
//! mail/send). Mirrors v1's provider semantics: `none | smtp | sendgrid`,
//! multipart/alternative with a real text/plain part beside the HTML (UX-027;
//! v1 sent a fixed English stub there), TLS
//! mandatory on SMTP whenever credentials are configured, 15 s per-op
//! timeout, SendGrid success on 200/201/202. Never raises — every path
//! returns a [`SendOutcome`].

use std::time::Duration;

use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::json;

/// Per-op timeout for both transports (v1: smtplib 15 s per-op).
const OP_TIMEOUT: Duration = Duration::from_secs(15);

/// The whole SMTP conversation, not just the connect. HN-2 (night test 2026-09-26): lettre's
/// async transport applies `OP_TIMEOUT` to the TCP connect only, so a relay that accepted the
/// connection and never spoke held the notifier for as long as the relay pleased.
const SEND_DEADLINE: Duration = Duration::from_secs(60);
/// SendGrid v3 send endpoint (tests hit a local stand-in instead).
const SENDGRID_URL: &str = "https://api.sendgrid.com/v3/mail/send";

/// Result of one send attempt — never an Err; failures are described.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub success: bool,
    pub message: String,
}

impl SendOutcome {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }

    fn fail(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
        }
    }
}

/// The configured email provider (v1 global config `provider`).
#[derive(Debug, Clone)]
pub enum EmailTransport {
    /// No provider configured — every send fails softly.
    None,
    Smtp {
        host: String,
        port: u16,
        username: Option<String>,
        password: Option<String>,
        /// STARTTLS on the submission port.
        use_tls: bool,
        /// Implicit TLS (wrapper, typically port 465).
        use_ssl: bool,
    },
    Sendgrid {
        api_key: String,
    },
}

impl EmailTransport {
    /// Sends one email to `to`, `text` and `html` being the two
    /// renderings of one [`super::payload::EmailBody`]. Multi-recipient sends go out as a
    /// single message with all recipients in `To:` (v1 behavior).
    pub async fn send(
        &self,
        from: &str,
        from_name: &str,
        to: &[String],
        subject: &str,
        text: &str,
        html: &str,
    ) -> SendOutcome {
        if to.is_empty() {
            return SendOutcome::fail("no recipients");
        }
        match self {
            EmailTransport::None => SendOutcome::fail("no email provider configured"),
            EmailTransport::Smtp {
                host,
                port,
                username,
                password,
                use_tls,
                use_ssl,
            } => {
                send_smtp(
                    host, *port, username, password, *use_tls, *use_ssl, from, from_name, to,
                    subject, text, html,
                )
                .await
            }
            EmailTransport::Sendgrid { api_key } => {
                send_sendgrid(
                    SENDGRID_URL,
                    api_key,
                    from,
                    from_name,
                    to,
                    subject,
                    text,
                    html,
                )
                .await
            }
        }
    }
}

/// Builds the multipart/alternative message (text/plain + HTML).
fn build_message(
    from: &str,
    from_name: &str,
    to: &[String],
    subject: &str,
    text: &str,
    html: &str,
) -> Result<Message, String> {
    let from_addr: Address = from
        .parse()
        .map_err(|err| format!("invalid from address '{from}': {err}"))?;
    let from_mailbox = Mailbox::new(
        (!from_name.is_empty()).then(|| from_name.to_string()),
        from_addr,
    );

    // HN-15 (Hub test 2026-09-26): the recipients go in Bcc, so they do not see each other,
    // with the sender in To; and every message has its own Message-ID, which lettre does not
    // add by itself. The envelope carries the recipients only (R-1, review of !102): with the
    // sender in it too, the sender got a copy of every alarm, and a relay refusing the
    // sender's own RCPT made the whole message fail.
    let domain = from.rsplit_once('@').map_or("proximos.local", |(_, d)| d);
    let mut builder = Message::builder()
        .from(from_mailbox.clone())
        .to(from_mailbox.clone())
        .subject(subject)
        .message_id(Some(format!("<{}@{domain}>", uuid::Uuid::new_v4())));
    let mut rcpts: Vec<Address> = Vec::new();
    for recipient in to {
        match recipient.parse::<Address>() {
            Ok(addr) => {
                builder = builder.bcc(Mailbox::new(None, addr.clone()));
                rcpts.push(addr);
            }
            Err(err) => tracing::warn!(%recipient, %err, "skipping malformed email recipient"),
        }
    }
    if rcpts.is_empty() {
        return Err("no valid recipients".to_string());
    }
    let envelope = lettre::address::Envelope::new(Some(from_mailbox.email), rcpts)
        .map_err(|err| format!("envelope: {err}"))?;

    builder
        .envelope(envelope)
        .multipart(MultiPart::alternative_plain_html(
            text.to_string(),
            html.to_string(),
        ))
        .map_err(|err| format!("message build failed: {err}"))
}

#[allow(clippy::too_many_arguments)] // flat transport params, called from one place
async fn send_smtp(
    host: &str,
    port: u16,
    username: &Option<String>,
    password: &Option<String>,
    use_tls: bool,
    use_ssl: bool,
    from: &str,
    from_name: &str,
    to: &[String],
    subject: &str,
    text: &str,
    html: &str,
) -> SendOutcome {
    let has_credentials = username.is_some() || password.is_some();
    // v1 parity: TLS/SSL is mandatory when credentials are set — never
    // authenticate over plaintext.
    if has_credentials && !use_tls && !use_ssl {
        return SendOutcome::fail(
            "SMTP credentials are set but neither TLS nor SSL is enabled; refusing to \
             authenticate over plaintext",
        );
    }

    let message = match build_message(from, from_name, to, subject, text, html) {
        Ok(message) => message,
        Err(err) => return SendOutcome::fail(err),
    };

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(port)
        .timeout(Some(OP_TIMEOUT));

    if use_ssl || use_tls {
        let params = match TlsParameters::new(host.to_string()) {
            Ok(params) => params,
            Err(err) => return SendOutcome::fail(format!("TLS setup failed: {err}")),
        };
        builder = builder.tls(if use_ssl {
            Tls::Wrapper(params)
        } else {
            Tls::Required(params)
        });
    } else {
        builder = builder.tls(Tls::None);
    }

    if let (Some(user), Some(pass)) = (username, password) {
        builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
    }

    match tokio::time::timeout(SEND_DEADLINE, builder.build().send(message)).await {
        Ok(Ok(_)) => SendOutcome::ok("sent via SMTP"),
        Ok(Err(err)) => SendOutcome::fail(format!("SMTP send failed: {err}")),
        Err(_) => SendOutcome::fail(format!(
            "SMTP send timed out after {} s",
            SEND_DEADLINE.as_secs()
        )),
    }
}

/// SendGrid v3 `POST /v3/mail/send`; success is 200/201/202 (v1 parity).
/// `base_url` is a parameter so tests can point at a local server.
#[allow(clippy::too_many_arguments)] // flat transport params, like send_smtp
async fn send_sendgrid(
    base_url: &str,
    api_key: &str,
    from: &str,
    from_name: &str,
    to: &[String],
    subject: &str,
    text: &str,
    html: &str,
) -> SendOutcome {
    let client = match reqwest::Client::builder().timeout(OP_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => return SendOutcome::fail(format!("HTTP client build failed: {err}")),
    };

    let body = json!({
        // R-4: as for SMTP, the recipients in bcc and the sender in to, so they do not see
        // each other.
        "personalizations": [{
            "to": [{"email": from}],
            "bcc": to.iter().map(|email| json!({"email": email})).collect::<Vec<_>>(),
        }],
        "from": {"email": from, "name": from_name},
        "subject": subject,
        // SendGrid requires text/plain before text/html.
        "content": [
            {"type": "text/plain", "value": text},
            {"type": "text/html", "value": html},
        ],
    });

    match client
        .post(base_url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            // v1 parity: the SendGrid SDK treated 200/201/202 as success.
            if matches!(status, 200..=202) {
                SendOutcome::ok(format!("sent via SendGrid (HTTP {status})"))
            } else {
                let detail = response.text().await.unwrap_or_default();
                SendOutcome::fail(format!(
                    "SendGrid rejected the send: HTTP {status} {}",
                    detail.chars().take(200).collect::<String>()
                ))
            }
        }
        Err(err) => SendOutcome::fail(format!("SendGrid request failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};
    use parking_lot::Mutex;

    use super::*;

    /// HN-15: recipients in Bcc (they do not see each other), the sender in To, and a
    /// Message-ID on every message.
    #[test]
    fn recipients_are_blind_and_the_message_has_an_id() {
        let message = build_message(
            "portal@example.test",
            "ProXimos",
            &["a@example.test".to_string(), "b@example.test".to_string()],
            "subject",
            "x",
            "<p>x</p>",
        )
        .expect("message");
        let raw = String::from_utf8(message.formatted()).expect("utf8");
        let head = raw.split("\r\n\r\n").next().unwrap_or_default();
        let lines: Vec<&str> = head.lines().collect();
        assert!(
            lines.iter().any(|l| l.starts_with("Message-ID: <")),
            "{head}"
        );
        assert!(
            lines.contains(&"To: ProXimos <portal@example.test>"),
            "{head}"
        );
        // Header lines only: the Message-ID's random tail can end in "a", and
        // "…a@example.test" would match a plain substring check (R-2).
        assert!(
            !lines.iter().any(|l| l.starts_with("Bcc:")),
            "Bcc stays out: {head}"
        );
        let envelope: Vec<String> = message
            .envelope()
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(
            envelope,
            vec!["a@example.test".to_string(), "b@example.test".to_string()],
            "the envelope has the recipients only, not the sender (R-1)"
        );
    }

    #[tokio::test]
    async fn none_provider_fails_softly() {
        let outcome = EmailTransport::None
            .send("a@b.c", "Name", &["x@y.z".into()], "s", "h", "<p>h</p>")
            .await;
        assert!(!outcome.success);
        assert!(outcome.message.contains("no email provider"));
    }

    #[tokio::test]
    async fn empty_recipients_fails_softly() {
        let outcome = EmailTransport::Sendgrid {
            api_key: "k".into(),
        }
        .send("a@b.c", "Name", &[], "s", "h", "<p>h</p>")
        .await;
        assert!(!outcome.success);
        assert_eq!(outcome.message, "no recipients");
    }

    #[tokio::test]
    async fn smtp_refuses_plaintext_auth() {
        let transport = EmailTransport::Smtp {
            host: "mail.example.invalid".into(),
            port: 25,
            username: Some("user".into()),
            password: Some("pass".into()),
            use_tls: false,
            use_ssl: false,
        };
        let outcome = transport
            .send("a@b.c", "Name", &["x@y.z".into()], "s", "h", "<p>h</p>")
            .await;
        assert!(!outcome.success);
        assert!(outcome
            .message
            .contains("refusing to authenticate over plaintext"));
    }

    #[tokio::test]
    async fn smtp_invalid_from_fails_softly() {
        let transport = EmailTransport::Smtp {
            host: "mail.example.invalid".into(),
            port: 25,
            username: None,
            password: None,
            use_tls: false,
            use_ssl: false,
        };
        let outcome = transport
            .send(
                "not-an-address",
                "Name",
                &["x@y.z".into()],
                "s",
                "h",
                "<p>h</p>",
            )
            .await;
        assert!(!outcome.success);
        assert!(outcome.message.contains("invalid from address"));
    }

    #[test]
    fn message_skips_malformed_recipients_but_needs_one_valid() {
        let ok = build_message(
            "a@b.c",
            "Name",
            &["good@y.z".into(), "bad recipient".into()],
            "s",
            "h",
            "<p>h</p>",
        );
        assert!(ok.is_ok());

        let none = build_message(
            "a@b.c",
            "Name",
            &["bad recipient".into()],
            "s",
            "h",
            "<p>h</p>",
        );
        assert_eq!(
            none.expect_err("no valid recipients"),
            "no valid recipients"
        );
    }

    /// UX-027: the SMTP message's text/plain part carries the given text, and
    /// the English stub is gone.
    #[test]
    fn smtp_message_carries_the_text_part() {
        let message = build_message(
            "a@b.c",
            "Name",
            &["good@y.z".into()],
            "s",
            "Device: Cold Store A",
            "<p>h</p>",
        )
        .expect("message");
        let raw = String::from_utf8(message.formatted()).expect("utf-8");
        assert!(raw.contains("Content-Type: text/plain"), "{raw}");
        assert!(raw.contains("Device: Cold Store A"), "{raw}");
        assert!(!raw.contains("HTML-capable"), "{raw}");
    }

    /// Captured SendGrid request: bearer token + JSON body.
    #[derive(Default)]
    struct Captured {
        auth: Option<String>,
        body: Option<serde_json::Value>,
    }

    /// Local SendGrid stand-in returning `status`; captures the request.
    async fn spawn_sendgrid_stub(status: u16) -> (String, Arc<Mutex<Captured>>) {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let status_code = Arc::new(AtomicU16::new(status));
        let state = (Arc::clone(&captured), status_code);

        let app = Router::new()
            .route(
                "/v3/mail/send",
                post(
                    |State((captured, status)): State<(Arc<Mutex<Captured>>, Arc<AtomicU16>)>,
                     headers: HeaderMap,
                     Json(body): Json<serde_json::Value>| async move {
                        let mut guard = captured.lock();
                        guard.auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        guard.body = Some(body);
                        axum::http::StatusCode::from_u16(status.load(Ordering::SeqCst))
                            .expect("valid status")
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
        (format!("http://{addr}/v3/mail/send"), captured)
    }

    #[tokio::test]
    async fn sendgrid_success_on_202_with_expected_body() {
        let (url, captured) = spawn_sendgrid_stub(202).await;
        let outcome = send_sendgrid(
            &url,
            "sg-key",
            "alerts@example.com",
            "ProXimos Monitor",
            &["ops@example.com".into(), "night@example.com".into()],
            "[CRITICAL] test",
            "[KRITICKÉ] Alarm: Sklad B\nTeplota: 12.1 °C",
            "<p>hot</p>",
        )
        .await;
        assert!(outcome.success, "{}", outcome.message);

        let guard = captured.lock();
        assert_eq!(guard.auth.as_deref(), Some("Bearer sg-key"));
        let body = guard.body.as_ref().expect("body captured");
        // R-4: the recipients in bcc, the sender in to, as for SMTP.
        assert_eq!(
            body["personalizations"][0]["bcc"],
            serde_json::json!([
                {"email": "ops@example.com"},
                {"email": "night@example.com"}
            ])
        );
        assert_eq!(
            body["personalizations"][0]["to"],
            serde_json::json!([{"email": "alerts@example.com"}])
        );
        assert_eq!(body["from"]["email"], "alerts@example.com");
        assert_eq!(body["from"]["name"], "ProXimos Monitor");
        assert_eq!(body["subject"], "[CRITICAL] test");
        assert_eq!(body["content"][0]["type"], "text/plain");
        // UX-027: the text part is the e-mail itself, not v1's English stub.
        assert_eq!(
            body["content"][0]["value"],
            "[KRITICKÉ] Alarm: Sklad B\nTeplota: 12.1 °C"
        );
        assert_eq!(body["content"][1]["type"], "text/html");
        assert_eq!(body["content"][1]["value"], "<p>hot</p>");
    }

    #[tokio::test]
    async fn sendgrid_failure_on_401() {
        let (url, _captured) = spawn_sendgrid_stub(401).await;
        let outcome = send_sendgrid(
            &url,
            "bad-key",
            "alerts@example.com",
            "",
            &["ops@example.com".into()],
            "s",
            "h",
            "<p>h</p>",
        )
        .await;
        assert!(!outcome.success);
        assert!(outcome.message.contains("HTTP 401"), "{}", outcome.message);
    }
}
