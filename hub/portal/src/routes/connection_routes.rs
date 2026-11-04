//! `GET /connection/ca` — this Hub's CA certificate, unauthenticated
//! (plan/control/cluster.md §6, milestone C0).
//!
//! The CA is the Hub-local one `proximos-tls-cert.sh`
//! (`system/meta-proximos/recipes-core/tls/`) makes on the first boot; it
//! signs the `server.crt` nginx terminates 443 with. A Client Hub joining
//! this one fetches it here (cluster.md §8 step 3.3) and trusts it only after
//! its SHA-256 matches the fingerprint in the join bundle, so the route needs
//! no authentication and the transport needs no trust of its own — v1 served
//! the same file the same way (`api/connection_info.py`), but over plain HTTP.
//!
//! Public by design and exempt from the setup gate too (`http::setup_gate`):
//! the file is public key material, and its consumer is a program, which a
//! redirect to `/setup` would only hand an HTML page to hash.
//!
//! The file is read on every request, so a factory reset's new CA is served
//! without a portal restart. What is served is checked to be certificates and
//! nothing else: a private key copied to the wrong name must never leave the
//! machine through an unauthenticated route.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::http::AppState;

/// The route, named once: `http::setup_gate` exempts it by this constant.
pub const CA_ROUTE: &str = "/connection/ca";

/// Where the CA lives. A constant, not configuration: the Landlock table
/// (`main.rs` `TLS_DIR`) already grants `/data/tls` read-only, and
/// plan/security/landlock.md §2 bans paths assembled from configuration.
pub const CA_CERT_PATH: &str = "/data/tls/ca.crt";

/// A CA certificate is well under 2 KiB; anything this large is not one.
const MAX_CA_BYTES: u64 = 64 * 1024;

pub fn router() -> Router<AppState> {
    Router::new().route(CA_ROUTE, get(connection_ca))
}

async fn connection_ca(State(state): State<AppState>) -> Response {
    ca_response(state.ca_cert_path.to_path_buf()).await
}

/// The bytes `GET /connection/ca` would serve right now, or `None` when it
/// would not serve any (missing, unreadable, not certificates alone).
///
/// The join bundle's fingerprint is taken over exactly these bytes
/// (plan/control/cluster.md §6: the Client hashes the body as received), so
/// it goes through the one function the route serves from rather than a
/// second reading of the file.
pub(crate) async fn served_ca(path: PathBuf) -> Option<String> {
    match tokio::task::spawn_blocking(move || read_ca(&path)).await {
        Ok(Ok(pem)) => Some(pem),
        // The normal state of a dev box and of a Hub before its first-boot CA, and every admin
        // render of the Hubs pages asks (`add_hub_ctx`): not worth a warning each time. The
        // dialog says why Add hub is unavailable; the route answers 404 without a log line.
        Ok(Err(CaError::Missing)) => {
            tracing::debug!("no CA certificate to fingerprint");
            None
        }
        Ok(Err(err)) => {
            tracing::warn!(?err, "no CA certificate to fingerprint");
            None
        }
        Err(join) => {
            tracing::error!(error = %join, "CA read task failed");
            None
        }
    }
}

/// The response for the CA at `path`, split out so the tests can point it at
/// a file they own.
async fn ca_response(path: PathBuf) -> Response {
    let read = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || read_ca(&path)).await
    };
    match read {
        Ok(Ok(pem)) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-pem-file"),
                ),
                // A factory reset replaces it; the client checks the
                // fingerprint, but a cache must not answer with the old one.
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            pem,
        )
            .into_response(),
        Ok(Err(CaError::Missing)) => {
            (StatusCode::NOT_FOUND, "no CA certificate on this Hub\n").into_response()
        }
        Ok(Err(CaError::Unreadable(err))) => {
            tracing::warn!(path = %path.display(), error = %err, "CA certificate unreadable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the CA certificate cannot be read\n",
            )
                .into_response()
        }
        Ok(Err(CaError::NotACertificate)) => {
            tracing::error!(
                path = %path.display(),
                "refusing to serve the CA file: it is not PEM certificates alone"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "the CA file is not a certificate\n",
            )
                .into_response()
        }
        Err(join) => {
            tracing::error!(error = %join, "CA read task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug)]
enum CaError {
    Missing,
    Unreadable(std::io::Error),
    NotACertificate,
}

fn read_ca(path: &Path) -> Result<String, CaError> {
    let io = |err: std::io::Error| match err.kind() {
        ErrorKind::NotFound => CaError::Missing,
        _ => CaError::Unreadable(err),
    };
    let len = std::fs::metadata(path).map_err(io)?.len();
    if len == 0 {
        return Err(CaError::Missing);
    }
    if len > MAX_CA_BYTES {
        return Err(CaError::NotACertificate);
    }
    let text = std::fs::read_to_string(path).map_err(|err| match err.kind() {
        ErrorKind::InvalidData => CaError::NotACertificate,
        _ => io(err),
    })?;
    if is_certificates_only(&text) {
        Ok(text)
    } else {
        Err(CaError::NotACertificate)
    }
}

/// At least one PEM block, and every block a `CERTIFICATE`. Text outside the
/// blocks (openssl's `-text` preamble) is tolerated, as PEM readers do.
fn is_certificates_only(text: &str) -> bool {
    let mut blocks = 0;
    for line in text.lines() {
        let line = line.trim();
        if let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|rest| rest.strip_suffix("-----"))
        {
            if label != "CERTIFICATE" {
                return false;
            }
            blocks += 1;
        }
    }
    blocks > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // A syntactically complete PEM block; the route checks shape, never the DER.
    const CA_PEM: &str =
        "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIQ\n-----END CERTIFICATE-----\n";

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn serves_the_ca_as_pem_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.crt");
        std::fs::write(&path, CA_PEM).unwrap();

        let response = ca_response(path).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/x-pem-file"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        // Byte for byte: the Client hashes what it receives and compares it
        // with the bundle's fingerprint of the file on this Hub.
        assert_eq!(body(response).await, CA_PEM);
    }

    #[tokio::test]
    async fn a_replaced_ca_is_served_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.crt");
        std::fs::write(&path, CA_PEM).unwrap();
        assert_eq!(body(ca_response(path.clone()).await).await, CA_PEM);

        let after_reset = CA_PEM.replace("MIIBszCC", "MIIBtDCC");
        std::fs::write(&path, &after_reset).unwrap();
        assert_eq!(body(ca_response(path).await).await, after_reset);
    }

    #[tokio::test]
    async fn no_ca_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let response = ca_response(dir.path().join("ca.crt")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body(response).await.contains("no CA certificate"));

        // An empty file is what a crash mid-write would leave: still "none".
        let empty = dir.path().join("empty.crt");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(ca_response(empty).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_private_key_is_never_served() {
        let dir = tempfile::tempdir().unwrap();
        // Assembled at run time: the `lint:recipes` CI job fails on a PEM
        // private-key header anywhere in the tracked tree.
        let key_header = format!("-----BEGIN {} KEY-----", "PRIVATE");
        let key_footer = format!("-----END {} KEY-----", "PRIVATE");
        for (name, text) in [
            (
                "key-alone",
                format!("{key_header}\nMIGHAgEA\n{key_footer}\n"),
            ),
            (
                "cert-then-key",
                format!("{CA_PEM}{key_header}\nMIGHAgEA\n{key_footer}\n"),
            ),
            ("not-pem", "hello\n".to_string()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, &text).unwrap();
            let response = ca_response(path).await;
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{name}"
            );
            assert!(!body(response).await.contains("MIGHAgEA"), "{name}");
        }
    }

    #[tokio::test]
    async fn an_oversized_file_is_not_served() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.crt");
        std::fs::write(&path, CA_PEM.repeat(2000)).unwrap();
        assert_eq!(
            ca_response(path).await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// A CA that exists but cannot be read (on a Hub: `/data/tls` not
    /// traversable by `proximos`) is a 503, not "this Hub has no CA" — a
    /// directory at the path stands in for it, because root, which CI runs
    /// as, reads through any mode bits.
    #[tokio::test]
    async fn an_unreadable_ca_is_a_503_not_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.crt");
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            ca_response(path).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// Captures what the current thread logs at WARN and above.
    #[derive(Clone, Default)]
    struct Warnings(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Warnings {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Warnings {
        type Writer = Warnings;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Every admin render of the Hubs pages asks `served_ca`; a missing CA
    /// (a dev box, a Hub before its first boot) is not a warning each time.
    /// A CA that is there and unusable still is.
    #[tokio::test]
    async fn a_missing_ca_is_not_a_warning_on_every_render() {
        let warnings = Warnings::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(warnings.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let dir = tempfile::tempdir().unwrap();

        for _ in 0..3 {
            assert!(served_ca(dir.path().join("ca.crt")).await.is_none());
        }
        assert!(
            warnings.0.lock().unwrap().is_empty(),
            "{}",
            String::from_utf8_lossy(&warnings.0.lock().unwrap())
        );

        let not_a_cert = dir.path().join("not-a-cert");
        std::fs::write(&not_a_cert, "hello\n").unwrap();
        assert!(served_ca(not_a_cert).await.is_none());
        assert!(
            !warnings.0.lock().unwrap().is_empty(),
            "a bad CA still warns"
        );
    }

    #[test]
    fn certificate_bundles_pass_and_anything_else_does_not() {
        assert!(is_certificates_only(CA_PEM));
        assert!(is_certificates_only(&format!("{CA_PEM}{CA_PEM}")));
        assert!(is_certificates_only(&format!(
            "subject=O={}\n{CA_PEM}",
            proximos_brand::CERT_ORGANISATION
        )));
        assert!(!is_certificates_only(""));
        assert!(!is_certificates_only(
            "-----BEGIN PUBLIC KEY-----\n-----END PUBLIC KEY-----\n"
        ));
    }
}
