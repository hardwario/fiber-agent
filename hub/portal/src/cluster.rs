//! The Server half of a Client Hub, as the portal sees it (plan/control/cluster.md §7–§9,
//! plan/portal/devices.hub.md §4, §5, §12 J; milestone C5).
//!
//! - [`Registry`]: the `cluster_clients` rows, cached in memory so that `known_hub()` — which
//!   every `/hubs/{hub}/…` handler calls synchronously — can answer "store ∪ registry" without a
//!   database round trip. Every write goes to the database first and then refreshes the cache
//!   from the row the database holds, so the two never disagree for longer than one statement.
//! - The input rules of §8.3 for a Client's hostname and gateway EUI.
//! - The join bundle (§8): `{"v":1,"h":…,"p":…,"f":…}`, its CA fingerprint and its QR code.
//! - [`AddAnswer`]: `cluster-client-add`'s `result`, with the password opened out of its
//!   `password_sealed` into a zeroizing string the moment it arrives, using the reply secret this
//!   request generated ([`fiber_protocol::sealed_reply`]).
//! - [`watch_liveness`]: `pending` → `complete` when the Client's status reaches this broker.
//!
//! **The password.** The agent never publishes it in clear (cluster.md §7, amended 2026-10-02).
//! Every Add hub or Retry generates an ephemeral X25519 reply secret and signs its public half
//! into the request as `reply_key`. The agent seals the password to it, and [`AddAnswer::take`]
//! opens the seal here, in memory. From then on the password is a [`Secret`]: no `Debug`, no
//! `Display`, no `Serialize`, zeroized on drop. It is never written to the registry, an audit
//! row, a log line or an SSE frame. Only the one-time reveal returns it, from memory (the route
//! tests in `routes::cluster_routes` hold all of this).

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use parking_lot::RwLock;
use portal_storage::{
    ClusterClient, ClusterInsert, NewClusterClient, StorageError, CLUSTER_STATE_PENDING,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use fiber_protocol::payloads::system_info::LorawanGatewayInfo;
use fiber_protocol::sealed_reply::{ReplySecret, Sealed};

use crate::db::Db;
use crate::state::{ChangeEvent, StateStore};

/// The signed verbs C2 added (`fiber_protocol` registry, `cluster.manage`, admin).
pub const ADD_VERB: &str = "cluster-client-add";
pub const REMOVE_VERB: &str = "cluster-client-remove";
/// #164: grant a registered Client the gateway it reports, keeping its password.
pub const GATEWAY_VERB: &str = "cluster-client-gateway";

/// The account a Client derives for itself (cluster.md §8 step 1).
pub fn peer_username(hostname: &str) -> String {
    format!("peer-{hostname}")
}

/// The 8883 listener a Client joins (cluster.md §5).
pub const BROKER_PORT: u16 = 8883;

// ---------------------------------------------------------------------------------------------
// Input rules (devices.hub.md §8.3)
// ---------------------------------------------------------------------------------------------

/// A Client Hub's hostname: decision #17's `^proximos-[0-9a-f]{8}$` — the prefix is the
/// build's brand (`fiber-` in a FIBER build, decision #50) — case-insensitive and stored
/// lowercase (#21). The rule the agent applies (`cluster_clients::validate_hostname`), applied
/// here first so a typo is an error at the field and never a signed request.
pub fn validate_hostname(raw: &str) -> Option<String> {
    let h = raw.trim().to_ascii_lowercase();
    proximos_brand::hostname_serial(&h)?;
    Some(h)
}

/// A gateway EUI as an operator types it — any case, `:`/`-`/whitespace separators — in the
/// canonical 16 lowercase hex (control/radio/plan.md §9.5). `fiber-protocol` has no
/// canonicaliser, so the portal brings its own (§12 J); `None` = not eight bytes of hex.
pub fn canonical_eui(raw: &str) -> Option<String> {
    let hex: String = raw
        .chars()
        .filter(|c| !(c.is_whitespace() || *c == ':' || *c == '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    (hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

// ---------------------------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------------------------

/// The `cluster_clients` table with an in-memory copy. Cheap to clone.
#[derive(Clone)]
pub struct Registry {
    db: Db,
    rows: Arc<RwLock<BTreeMap<String, ClusterClient>>>,
}

impl Registry {
    /// Reads every row once, at startup. A read failure is logged and leaves the registry
    /// empty: the portal still serves the Server, and the next write re-reads its row.
    pub fn load(db: Db) -> Self {
        let rows = match db.blocking(|s| s.list_cluster_clients()) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(%err, "cluster_clients could not be read; registry starts empty");
                Vec::new()
            }
        };
        Self {
            db,
            rows: Arc::new(RwLock::new(
                rows.into_iter().map(|r| (r.hostname.clone(), r)).collect(),
            )),
        }
    }

    /// The row for `hostname` (any case).
    pub fn get(&self, hostname: &str) -> Option<ClusterClient> {
        self.rows
            .read()
            .get(&hostname.trim().to_ascii_lowercase())
            .cloned()
    }

    /// Every row, newest registration first — the Hubs list's order under the Server
    /// (devices.hub.md §2.2).
    pub fn list(&self) -> Vec<ClusterClient> {
        let mut rows: Vec<ClusterClient> = self.rows.read().values().cloned().collect();
        rows.sort_by(|a, b| {
            b.registered_at
                .cmp(&a.registered_at)
                .then_with(|| a.hostname.cmp(&b.hostname))
        });
        rows
    }

    /// The Client a gateway EUI is registered to.
    pub fn eui_owner(&self, eui: &str) -> Option<String> {
        self.rows
            .read()
            .values()
            .find(|r| r.gateway_eui.as_deref() == Some(eui))
            .map(|r| r.hostname.clone())
    }

    /// Re-reads one row into the cache: present → replaced, absent → removed.
    fn cache(&self, hostname: &str, row: Option<ClusterClient>) -> Option<ClusterClient> {
        let mut rows = self.rows.write();
        match &row {
            Some(r) => {
                rows.insert(hostname.to_string(), r.clone());
            }
            None => {
                rows.remove(hostname);
            }
        }
        row
    }

    pub async fn insert(&self, new: NewClusterClient) -> Result<ClusterInsert, StorageError> {
        let hostname = new.hostname.clone();
        let result = self
            .db
            .write(move |s| s.insert_cluster_client(&new))
            .await?;
        if let ClusterInsert::Inserted(row) = &result {
            self.cache(&hostname, Some((**row).clone()));
        }
        Ok(result)
    }

    /// `gateway_eui` replaces the row's EUI when set (#163).
    pub async fn begin_attempt(
        &self,
        hostname: &str,
        reason: &str,
        gateway_eui: Option<&str>,
    ) -> Result<Option<ClusterClient>, StorageError> {
        let (h, r, e) = (
            hostname.to_string(),
            reason.to_string(),
            gateway_eui.map(str::to_string),
        );
        let row = self
            .db
            .write(move |s| s.begin_cluster_attempt(&h, &r, e.as_deref()))
            .await?;
        Ok(self.cache(hostname, row))
    }

    /// Undoes [`Self::begin_attempt`] for a Retry that never reached the agent.
    pub async fn restore(
        &self,
        previous: &ClusterClient,
    ) -> Result<Option<ClusterClient>, StorageError> {
        let p = previous.clone();
        let row = self
            .db
            .write(move |s| s.restore_cluster_attempt(&p))
            .await?;
        Ok(self.cache(&previous.hostname, row))
    }

    pub async fn record_result(
        &self,
        hostname: &str,
        peer_username: &str,
        error: Option<&str>,
    ) -> Result<Option<ClusterClient>, StorageError> {
        let (h, u, e) = (
            hostname.to_string(),
            peer_username.to_string(),
            error.map(str::to_string),
        );
        let row = self
            .db
            .write(move |s| s.record_cluster_result(&h, &u, e.as_deref()))
            .await?;
        Ok(self.cache(hostname, row))
    }

    /// `pending` → `complete`; `true` = it moved.
    pub async fn mark_complete(&self, hostname: &str) -> Result<bool, StorageError> {
        let h = hostname.to_string();
        let moved = self
            .db
            .write(move |s| s.mark_cluster_client_complete(&h))
            .await?;
        if moved {
            let h = hostname.to_string();
            let row = self.db.run(move |s| s.get_cluster_client(&h)).await?;
            self.cache(hostname, row);
        }
        Ok(moved)
    }

    pub async fn record_removal_error(
        &self,
        hostname: &str,
        error: &str,
    ) -> Result<Option<ClusterClient>, StorageError> {
        let (h, e) = (hostname.to_string(), error.to_string());
        let row = self
            .db
            .write(move |s| s.record_cluster_removal_error(&h, &e))
            .await?;
        Ok(self.cache(hostname, row))
    }

    /// The gateway an admin granted (#164); `false` = another Client holds that EUI.
    pub async fn set_gateway_eui(&self, hostname: &str, eui: &str) -> Result<bool, StorageError> {
        let (h, e) = (hostname.to_string(), eui.to_string());
        let set = self
            .db
            .write(move |s| s.set_cluster_gateway_eui(&h, &e))
            .await?;
        let h = hostname.to_string();
        let row = self.db.run(move |s| s.get_cluster_client(&h)).await?;
        self.cache(hostname, row);
        Ok(set)
    }

    pub async fn delete(&self, hostname: &str) -> Result<bool, StorageError> {
        let h = hostname.to_string();
        let deleted = self.db.write(move |s| s.delete_cluster_client(&h)).await?;
        self.cache(hostname, None);
        Ok(deleted)
    }
}

// ---------------------------------------------------------------------------------------------
// Liveness (cluster.md §9)
// ---------------------------------------------------------------------------------------------

/// Moves a `pending` Client to `complete` the first time its agent's `status` reaches this
/// Server's broker — the store's `DeviceStatus { online: true }` for its hostname, which the
/// retained status produces at every (re)subscribe as well. Runs for the life of the process.
///
/// A missed event cannot strand a row: the store is reconciled once at start and again after
/// the broadcast lags, so a Client that is online in the store is completed either way.
pub async fn watch_liveness(store: Arc<StateStore>, registry: Registry) {
    let mut rx = store.subscribe();
    reconcile(&store, &registry).await;
    loop {
        match rx.recv().await {
            Ok(ChangeEvent::DeviceStatus {
                hostname,
                online: true,
            }) => complete_if_pending(&registry, &hostname).await,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                reconcile(&store, &registry).await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn reconcile(store: &StateStore, registry: &Registry) {
    let snapshot = store.snapshot();
    for row in registry.list() {
        if row.registration_state == CLUSTER_STATE_PENDING
            && snapshot
                .devices
                .get(&row.hostname)
                .is_some_and(|d| d.online)
        {
            complete_if_pending(registry, &row.hostname).await;
        }
    }
}

async fn complete_if_pending(registry: &Registry, hostname: &str) {
    let Some(row) = registry.get(hostname) else {
        return;
    };
    if row.registration_state != CLUSTER_STATE_PENDING {
        return;
    }
    match registry.mark_complete(&row.hostname).await {
        Ok(true) => tracing::info!(
            hostname = %row.hostname,
            "Client Hub reached this broker: registration complete"
        ),
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(%err, hostname = %row.hostname, "could not mark a Client complete")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The agent's answer
// ---------------------------------------------------------------------------------------------

/// A minted broker password. No `Debug`, `Display` or `Serialize`; zeroized on drop.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Only base64url, exactly the 22 characters the agent mints (cluster.md §7): anything
    /// else is not rendered into a page, a QR code or a JSON string.
    fn parse(raw: String) -> Option<Self> {
        let raw = Zeroizing::new(raw);
        (raw.len() == 22
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| Secret(raw))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// One effect's outcome as the agent reports it: `ok`, `skipped` or `failed`. Anything else
/// (or absent) reads as `failed` — the portal never infers success.
pub fn effect(result: &Value, key: &str) -> &'static str {
    match result.get(key).and_then(Value::as_str) {
        Some("ok") => "ok",
        Some("skipped") => "skipped",
        _ => "failed",
    }
}

/// Why an answer that should have carried a password does not hand one over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealProblem {
    /// `password_sealed` is there, but it does not open with this request's key, request id and
    /// Client. It may be tampered, or meant for another request.
    DoesNotOpen,
    /// The answer carries a clear-text `password`. It has already been on the loopback bus for
    /// every subscriber to read, so it is not used (an agent from before the seal).
    InClear,
}

/// `cluster-client-add`'s / `-remove`'s `result` (C2), password taken out.
pub struct AddAnswer {
    pub username: Option<String>,
    pub gateway_eui: Option<String>,
    pub account: &'static str,
    pub gateway: &'static str,
    /// The agent's own summary of the effects. Never contains the password (C2's tests).
    pub message: Option<String>,
    pub password: Option<Secret>,
    /// Set when there was a password to hand over and it could not be used.
    pub seal_problem: Option<SealProblem>,
}

impl AddAnswer {
    /// Takes the `result` apart. `reply` is the secret this request signed the public half of,
    /// and `request_id` and `client` are what the agent bound the seal to. It is consumed: one key
    /// opens one answer. A `-remove` answer has no seal, and `reply` is then simply dropped.
    pub fn take(
        mut result: Value,
        reply: Option<ReplySecret>,
        request_id: Option<&str>,
        client: &str,
    ) -> Self {
        let map = result.as_object_mut();
        let in_clear = map
            .as_ref()
            .is_some_and(|m| m.get("password").is_some_and(|p| !p.is_null()));
        let sealed = map.and_then(|m| {
            // A clear password is taken out unread, so nothing that prints `result` shows it.
            m.remove("password");
            m.remove("password_sealed")
        });
        let mut seal_problem = in_clear.then_some(SealProblem::InClear);
        let password = match (sealed, reply, request_id) {
            (Some(sealed), Some(reply), Some(request_id)) if !sealed.is_null() => {
                match serde_json::from_value::<Sealed>(sealed)
                    .ok()
                    .and_then(|s| reply.open(&s, request_id, client).ok())
                    .and_then(|p| Secret::parse(p.as_str().to_string()))
                {
                    Some(p) => Some(p),
                    None => {
                        seal_problem = Some(SealProblem::DoesNotOpen);
                        None
                    }
                }
            }
            (Some(sealed), _, _) if !sealed.is_null() => {
                seal_problem = Some(SealProblem::DoesNotOpen);
                None
            }
            _ => None,
        };
        let text = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_string);
        AddAnswer {
            username: text("username"),
            gateway_eui: text("gateway_eui"),
            account: effect(&result, "account"),
            gateway: effect(&result, "gateway"),
            message: text("message"),
            password,
            seal_problem,
        }
    }

    /// Both effects good — the `pending` row of cluster.md §7 — and a password to hand out.
    pub fn complete(&self) -> bool {
        self.account == "ok"
            && self.gateway != "failed"
            && self.password.is_some()
            && self.seal_problem.is_none()
    }
}

// ---------------------------------------------------------------------------------------------
// The join bundle (cluster.md §8 step 1)
// ---------------------------------------------------------------------------------------------

/// SHA-256 of the CA certificate **exactly as `GET /connection/ca` serves it**, base64url
/// without padding: 43 characters. The Client hashes the body as received (cluster.md §6), so
/// this must be computed over the same bytes — the caller passes what
/// `connection_routes::read_ca` returned, the function that route serves from.
pub fn ca_fingerprint(served: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(served))
}

/// The four fields in the order cluster.md §8 writes them.
#[derive(Serialize)]
struct BundleWire<'a> {
    v: u8,
    h: &'a str,
    p: &'a str,
    f: &'a str,
}

/// `{"v":1,"h":"<server>.local","p":"<password>","f":"<fingerprint>"}`, compact.
pub fn join_bundle(
    server_hostname: &str,
    password: &Secret,
    fingerprint: &str,
) -> Zeroizing<String> {
    let host = format!("{}.local", server_hostname.trim().to_ascii_lowercase());
    Zeroizing::new(
        serde_json::to_string(&BundleWire {
            v: 1,
            h: &host,
            p: password.expose(),
            f: fingerprint,
        })
        // Four plain strings and a number: serialisation cannot fail.
        .unwrap_or_default(),
    )
}

/// The bundle as an inline SVG QR code, rendered here so no image file or client-side
/// library is involved. `None` only if the text cannot fit a QR code, which ~110 bytes always
/// can.
pub fn qr_svg(text: &str) -> Option<Zeroizing<String>> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M).ok()?;
    // Wrapped the moment it exists: the drawing encodes the password, and the spec promises
    // the bundle's buffers are overwritten on release (devices.hub.md §4, amended 2026-10-02).
    let svg = Zeroizing::new(
        code.render::<svg::Color<'_>>()
            .min_dimensions(240, 240)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    );
    Some(without_xml_declaration(svg))
}

/// The renderer opens with an XML declaration, which has no place inside HTML. Cut in place,
/// so no unwiped copy of the drawing is left behind: the bytes move within the one buffer, and
/// `Zeroizing` wipes its whole capacity when it is dropped.
fn without_xml_declaration(mut svg: Zeroizing<String>) -> Zeroizing<String> {
    let start = svg.find("<svg").unwrap_or(0);
    svg.drain(..start);
    svg
}

// ---------------------------------------------------------------------------------------------
// The one-time reveal (devices.hub.md §4 step 4, §8.4; decision of 2026-10-02)
// ---------------------------------------------------------------------------------------------

/// How long an unrevealed join bundle is held: long enough to reach for the phone, short enough
/// that a forgotten tab does not keep a live credential in this process.
pub const REVEAL_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// What the Add-hub screen shows instead of the password: an opaque, one-time token. The join
/// bundle stays in this process's memory, never on disk and never in a page, until the token is
/// redeemed by the same admin in the same session, once.
///
/// - **One fetch.** [`RevealVault::take`] removes the entry whatever it answers, so a second
///   fetch finds nothing.
/// - **Bound.** The entry records the SHA-256 of the session cookie (the cookie itself is not
///   kept), the user, and the Server and Client it was made for. A token presented from any
///   other session is refused and burnt: whoever has it should not have it.
/// - **Short-lived.** After [`REVEAL_TTL`] it is gone, swept by a timer even if nobody comes back.
/// - **One per Client.** A new bundle for a Client, which a Retry makes after rotating the
///   password, replaces any outstanding one. So does a removal.
///
/// The bundle and its QR code are `Zeroizing`, wiped when the entry is dropped.
#[derive(Clone)]
pub struct RevealVault {
    entries: Arc<parking_lot::Mutex<BTreeMap<String, RevealEntry>>>,
    ttl: std::time::Duration,
}

struct RevealEntry {
    session: [u8; 32],
    user_id: i64,
    server: String,
    client: String,
    bundle: Zeroizing<String>,
    qr_svg: Option<Zeroizing<String>>,
    expires: std::time::Instant,
}

/// A redeemed token: the bundle text and its QR code. No `Debug`, no `Serialize`.
pub struct Revealed {
    pub bundle: Zeroizing<String>,
    pub qr_svg: Option<Zeroizing<String>>,
}

/// Why a token did not reveal anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealRefusal {
    /// Already fetched, expired, replaced by a Retry, or never issued.
    Gone,
    /// Issued to another session or user. The entry is burnt.
    OtherSession,
}

impl RevealRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            RevealRefusal::Gone => "gone",
            RevealRefusal::OtherSession => "other_session",
        }
    }
}

/// The SHA-256 of a session cookie's value: what a reveal is bound to, without keeping the
/// session token itself in a second place.
pub fn session_fingerprint(session_token: &str) -> [u8; 32] {
    Sha256::digest(session_token.as_bytes()).into()
}

impl Default for RevealVault {
    fn default() -> Self {
        Self::new(REVEAL_TTL)
    }
}

impl RevealVault {
    pub fn new(ttl: std::time::Duration) -> Self {
        Self {
            entries: Arc::default(),
            ttl,
        }
    }

    /// Holds `bundle` for one fetch and returns the token that fetches it: 32 random bytes,
    /// base64url. Any bundle still held for `client` is dropped first.
    pub fn put(
        &self,
        session: [u8; 32],
        user_id: i64,
        server: &str,
        client: &str,
        bundle: Zeroizing<String>,
        qr_svg: Option<Zeroizing<String>>,
    ) -> String {
        use rand::RngCore as _;
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let now = std::time::Instant::now();
        {
            let mut entries = self.entries.lock();
            entries.retain(|_, e| e.client != client && e.expires > now);
            entries.insert(
                token.clone(),
                RevealEntry {
                    session,
                    user_id,
                    server: server.to_ascii_lowercase(),
                    client: client.to_string(),
                    bundle,
                    qr_svg,
                    expires: now + self.ttl,
                },
            );
        }
        // The sweep: an entry nobody comes back for does not outlive its TTL in memory.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (entries, token, ttl) = (self.entries.clone(), token.clone(), self.ttl);
            runtime.spawn(async move {
                tokio::time::sleep(ttl).await;
                entries.lock().remove(&token);
            });
        }
        token
    }

    /// Drops whatever is held for `client`: a Retry rotated its password, or it was removed.
    pub fn forget_client(&self, client: &str) {
        self.entries.lock().retain(|_, e| e.client != client);
    }

    /// Redeems `token`, once. The entry is removed whatever the answer.
    pub fn take(
        &self,
        token: &str,
        session: [u8; 32],
        user_id: i64,
        server: &str,
        client: &str,
    ) -> Result<Revealed, RevealRefusal> {
        let entry = self
            .entries
            .lock()
            .remove(token)
            .ok_or(RevealRefusal::Gone)?;
        if entry.expires <= std::time::Instant::now() {
            return Err(RevealRefusal::Gone);
        }
        if entry.session != session || entry.user_id != user_id {
            return Err(RevealRefusal::OtherSession);
        }
        if entry.server != server.to_ascii_lowercase() || entry.client != client {
            return Err(RevealRefusal::Gone);
        }
        Ok(Revealed {
            bundle: entry.bundle,
            qr_svg: entry.qr_svg,
        })
    }

    /// How many bundles are held. For tests.
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.entries.lock().len()
    }
}

// ---------------------------------------------------------------------------------------------
// A Client's gateway that this Server does not let in (cluster.md §5, §7.1, amended 2026-10-13)
// ---------------------------------------------------------------------------------------------

/// A registered Client runs a LoRaWAN gateway that its registration does not cover.
///
/// The account's ACL block grants `<region>/gateway/<eui>/#` only for the EUI given to
/// `cluster-client-add` (§5), and the Server's ChirpStack knows only that EUI (§7). A Client
/// added while it had no concentrator (P2P, no card) has neither; when it later runs LoRaWAN,
/// its forwarder connects with the peer account and mosquitto drops every gateway publish
/// without a word. Nothing on either Hub fails, so the portal is the one place that can say so.
///
/// The registry row's `gateway_eui` is the portal's copy of what the ACL grants; the Client's
/// side is its retained `system/info.lorawan` on this broker. The Server never grants a gateway
/// on the Client's say-so — that stays the admin's signed Add hub / Retry (amendment
/// 2026-10-13) — so this is detection only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayGap {
    /// `"unregistered"`: the row has no gateway; `"mismatch"`: the Client reports another one.
    pub kind: &'static str,
    /// The EUI the Client reports, when it reports one. A Client runs no ChirpStack of its
    /// own, so its `system/info.lorawan.gateway_eui` is usually absent.
    pub reported_eui: Option<String>,
    /// The EUI the registration granted.
    pub registered_eui: Option<String>,
}

/// [`GatewayGap`] for one registry row, judged from the Client's last `system/info.lorawan`.
///
/// Silent unless the Client says its radio mode runs LoRaWAN (`lorawan`/`dual`): a P2P or `off`
/// Client has no forwarder, and an unknown mode is not evidence. Then:
/// - no registered EUI, and the Client runs concentratord or reports an EUI → `unregistered`;
/// - a registered EUI, and the Client reports a different one → `mismatch`.
///
/// A Client that reports no EUI and runs no concentratord (no card yet) is the "concentrator
/// missing" alarm's business, not this one's.
pub fn gateway_gap(
    registered_eui: Option<&str>,
    lorawan: Option<&LorawanGatewayInfo>,
) -> Option<GatewayGap> {
    let info = lorawan?;
    let runs_lorawan = info
        .mode
        .as_deref()
        .is_some_and(|m| matches!(m.trim().to_ascii_lowercase().as_str(), "lorawan" | "dual"));
    if !runs_lorawan {
        return None;
    }
    let reported = info.gateway_eui.as_deref().and_then(canonical_eui);
    let registered = registered_eui.and_then(canonical_eui);
    match (&registered, &reported) {
        (None, _) if reported.is_some() || info.concentratord_running == Some(true) => {
            Some(GatewayGap {
                kind: "unregistered",
                reported_eui: reported,
                registered_eui: None,
            })
        }
        (Some(r), Some(x)) if r != x => Some(GatewayGap {
            kind: "mismatch",
            reported_eui: reported,
            registered_eui: registered,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PW: &str = "AbCdEfGhIjKlMnOpQrSt-_";

    #[test]
    fn a_hostname_is_decision_17s_shape_in_lowercase() {
        assert_eq!(
            validate_hostname(" PROXIMOS-1A2B3C4D ").as_deref(),
            Some("proximos-1a2b3c4d")
        );
        for bad in [
            "",
            "proximos-1a2b3c4",
            "proximos-1a2b3c4d5",
            "fiber-1a2b3c4d",
            "proximos-1a2b3c4g",
            "proximos-1a2b 3c4d",
            "proximos_1a2b3c4d",
        ] {
            assert_eq!(validate_hostname(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_eui_is_canonicalised_from_what_an_operator_types() {
        for typed in [
            "70:B3:D5:49:00:AA:00:02",
            "70-b3-d5-49-00-aa-00-02",
            " 70b3d54900AA0002 ",
            "70 b3 d5 49 00 aa 00 02",
        ] {
            assert_eq!(
                canonical_eui(typed).as_deref(),
                Some("70b3d54900aa0002"),
                "{typed:?}"
            );
        }
        for bad in [
            "70b3d54900aa000",
            "70b3d54900aa00020",
            "70b3d54900aa000g",
            "",
        ] {
            assert_eq!(canonical_eui(bad), None, "{bad:?}");
        }
    }

    const CLIENT: &str = "proximos-1a2b3c4d";

    /// A request's reply secret, and the answer an agent would seal to it.
    fn sealed_answer(extra: Value) -> (ReplySecret, Value) {
        let secret = ReplySecret::generate();
        let sealed =
            fiber_protocol::sealed_reply::seal(PW, &secret.reply_key(), "req-1", CLIENT).unwrap();
        let mut doc = json!({
            "hostname": CLIENT, "username": "peer-proximos-1a2b3c4d",
            "gateway_eui": null, "account": "ok", "gateway": "skipped",
            "password_sealed": sealed,
            "message": "account peer-proximos-1a2b3c4d minted; gateway skipped",
        });
        for (k, v) in extra.as_object().unwrap() {
            doc[k] = v.clone();
        }
        (secret, doc)
    }

    #[test]
    fn the_answer_opens_the_sealed_password_with_this_requests_key() {
        let (secret, doc) = sealed_answer(json!({}));
        let answer = AddAnswer::take(doc, Some(secret), Some("req-1"), CLIENT);
        assert!(answer.complete());
        assert_eq!(answer.password.as_ref().map(Secret::expose), Some(PW));
        assert_eq!(answer.seal_problem, None);
        assert_eq!(answer.gateway, "skipped");
        assert_eq!(answer.gateway_eui, None);
    }

    /// A seal for another key, request or Client, a tampered one, or a password that came in
    /// clear: no password is handed out, and the registration is not complete.
    #[test]
    fn a_seal_that_does_not_open_or_a_clear_password_is_never_used() {
        let (_, doc) = sealed_answer(json!({}));
        let other = AddAnswer::take(doc, Some(ReplySecret::generate()), Some("req-1"), CLIENT);
        assert!(other.password.is_none());
        assert_eq!(other.seal_problem, Some(SealProblem::DoesNotOpen));
        assert!(!other.complete());

        let (secret, doc) = sealed_answer(json!({}));
        let wrong_request = AddAnswer::take(doc, Some(secret), Some("req-2"), CLIENT);
        assert_eq!(wrong_request.seal_problem, Some(SealProblem::DoesNotOpen));

        let (secret, doc) = sealed_answer(json!({}));
        let wrong_client = AddAnswer::take(doc, Some(secret), Some("req-1"), "proximos-00000000");
        assert_eq!(wrong_client.seal_problem, Some(SealProblem::DoesNotOpen));

        let (secret, mut doc) = sealed_answer(json!({}));
        let ct = doc["password_sealed"]["ct"].as_str().unwrap().to_string();
        let flipped = format!(
            "{}{}",
            if ct.starts_with('A') { "B" } else { "A" },
            &ct[1..]
        );
        doc["password_sealed"]["ct"] = json!(flipped);
        let tampered = AddAnswer::take(doc, Some(secret), Some("req-1"), CLIENT);
        assert_eq!(tampered.seal_problem, Some(SealProblem::DoesNotOpen));
        assert!(!tampered.complete());

        let clear = json!({"account": "ok", "gateway": "ok", "password": PW});
        let answer = AddAnswer::take(clear, Some(ReplySecret::generate()), Some("req-1"), CLIENT);
        assert!(answer.password.is_none(), "a clear password is never used");
        assert_eq!(answer.seal_problem, Some(SealProblem::InClear));
        assert!(!answer.complete());
    }

    #[test]
    fn a_partial_or_odd_answer_is_never_complete() {
        for (extra, why) in [
            (json!({"gateway": "failed"}), "gateway failed"),
            (
                json!({"account": "failed", "password_sealed": null}),
                "account failed",
            ),
            (json!({"password_sealed": null}), "no password"),
            (json!({"account": "yes"}), "unknown effect word"),
        ] {
            let (secret, doc) = sealed_answer(extra);
            assert!(
                !AddAnswer::take(doc, Some(secret), Some("req-1"), CLIENT).complete(),
                "{why}"
            );
        }
        // A password outside the agent's 22 base64url characters is not rendered, sealed or not.
        let secret = ReplySecret::generate();
        let odd = fiber_protocol::sealed_reply::seal(
            "<script>alert(1)</script>xx",
            &secret.reply_key(),
            "req-1",
            CLIENT,
        )
        .unwrap();
        let doc = json!({"account": "ok", "gateway": "ok", "password_sealed": odd});
        assert!(!AddAnswer::take(doc, Some(secret), Some("req-1"), CLIENT).complete());
    }

    fn vault_put(vault: &RevealVault, session: &str, client: &str) -> String {
        vault.put(
            session_fingerprint(session),
            1,
            "proximos-a1b2c3d4",
            client,
            Zeroizing::new(format!("bundle-for-{client}")),
            None,
        )
    }

    #[test]
    fn a_reveal_token_is_one_fetch_bound_to_its_session_and_client() {
        let vault = RevealVault::default();
        let fp = session_fingerprint("session-a");
        let token = vault_put(&vault, "session-a", CLIENT);
        assert_eq!(token.len(), 43, "32 random bytes");
        let revealed = vault
            .take(&token, fp, 1, "PROXIMOS-A1B2C3D4", CLIENT)
            .expect("the first fetch reveals");
        assert_eq!(revealed.bundle.as_str(), format!("bundle-for-{CLIENT}"));
        assert_eq!(
            vault.take(&token, fp, 1, "proximos-a1b2c3d4", CLIENT).err(),
            Some(RevealRefusal::Gone),
            "the second does not"
        );

        // Another session, or another user, is refused and burns the token.
        let token = vault_put(&vault, "session-a", CLIENT);
        assert_eq!(
            vault
                .take(
                    &token,
                    session_fingerprint("session-b"),
                    1,
                    "proximos-a1b2c3d4",
                    CLIENT
                )
                .err(),
            Some(RevealRefusal::OtherSession)
        );
        assert_eq!(
            vault.take(&token, fp, 1, "proximos-a1b2c3d4", CLIENT).err(),
            Some(RevealRefusal::Gone)
        );
        let token = vault_put(&vault, "session-a", CLIENT);
        assert_eq!(
            vault.take(&token, fp, 2, "proximos-a1b2c3d4", CLIENT).err(),
            Some(RevealRefusal::OtherSession)
        );

        // Another Client's path: nothing.
        let token = vault_put(&vault, "session-a", CLIENT);
        assert_eq!(
            vault
                .take(&token, fp, 1, "proximos-a1b2c3d4", "proximos-00000000")
                .err(),
            Some(RevealRefusal::Gone)
        );
        assert_eq!(vault.held(), 0);
    }

    #[test]
    fn a_new_bundle_for_a_client_replaces_the_outstanding_one() {
        let vault = RevealVault::default();
        let fp = session_fingerprint("s");
        let old = vault_put(&vault, "s", CLIENT);
        let other = vault_put(&vault, "s", "proximos-00000002");
        let new = vault_put(&vault, "s", CLIENT);
        assert_eq!(vault.held(), 2);
        assert!(vault
            .take(&old, fp, 1, "proximos-a1b2c3d4", CLIENT)
            .is_err());
        assert!(vault.take(&new, fp, 1, "proximos-a1b2c3d4", CLIENT).is_ok());
        vault.forget_client("proximos-00000002");
        assert!(vault
            .take(&other, fp, 1, "proximos-a1b2c3d4", "proximos-00000002")
            .is_err());
    }

    /// Past its TTL a token reveals nothing, and the sweep has already dropped the bundle.
    #[tokio::test]
    async fn a_reveal_token_expires_and_is_swept() {
        let vault = RevealVault::new(std::time::Duration::from_millis(50));
        let fp = session_fingerprint("s");
        let token = vault_put(&vault, "s", CLIENT);
        let late = vault_put(&vault, "s", "proximos-00000002");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(vault.held(), 0, "swept without anybody asking");
        assert_eq!(
            vault.take(&token, fp, 1, "proximos-a1b2c3d4", CLIENT).err(),
            Some(RevealRefusal::Gone)
        );
        assert!(vault
            .take(&late, fp, 1, "proximos-a1b2c3d4", "proximos-00000002")
            .is_err());
    }

    #[test]
    fn the_bundle_is_the_four_fields_in_order() {
        let secret = Secret::parse(PW.to_string()).unwrap();
        let f = ca_fingerprint(b"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n");
        assert_eq!(f.len(), 43, "SHA-256 in base64url without padding");
        assert!(!f.contains('='));
        let bundle = join_bundle("Proximos-A1B2C3D4", &secret, &f);
        assert_eq!(
            bundle.as_str(),
            format!(r#"{{"v":1,"h":"proximos-a1b2c3d4.local","p":"{PW}","f":"{f}"}}"#)
        );
        assert!(
            bundle.len() < 185,
            "one BLE write at the measured MTU (cluster.md §8)"
        );
    }

    #[test]
    fn the_qr_code_is_inline_svg() {
        let svg = qr_svg(r#"{"v":1}"#).unwrap();
        assert!(svg.starts_with("<svg"), "{}", &svg[..40]);
        assert!(!svg.contains("<?xml"));
    }

    /// The declaration is cut from the renderer's own, already wiping,
    /// buffer, not from a copy: the string handed back is the same
    /// allocation, so no second copy of the drawing is dropped unwiped.
    #[test]
    fn the_qr_svg_is_trimmed_in_its_wiping_buffer() {
        let rendered = Zeroizing::new(r#"<?xml version="1.0"?><svg>…</svg>"#.to_string());
        let buffer = rendered.as_ptr();
        let trimmed = without_xml_declaration(rendered);
        assert_eq!(trimmed.as_str(), "<svg>…</svg>");
        assert_eq!(trimmed.as_ptr(), buffer, "the same buffer, not a copy");
    }

    fn info(mode: &str, concentratord: Option<bool>, eui: Option<&str>) -> LorawanGatewayInfo {
        LorawanGatewayInfo {
            mode: Some(mode.into()),
            concentratord_running: concentratord,
            gateway_eui: eui.map(Into::into),
            ..Default::default()
        }
    }

    /// The 2026-10-13 HIL case: proximos-2200ad33 joined in P2P without a gateway, then ran
    /// LoRaWAN on a new RAK5146; the Server's broker dropped every gateway publish, silently.
    #[test]
    fn a_client_running_lorawan_without_a_registered_gateway_is_a_gap() {
        let gap = gateway_gap(None, Some(&info("dual", Some(true), None))).unwrap();
        assert_eq!(gap.kind, "unregistered");
        assert_eq!(gap.reported_eui, None);
        assert_eq!(gap.registered_eui, None);

        let gap =
            gateway_gap(None, Some(&info("lorawan", None, Some("0016C001F136DB1B")))).unwrap();
        assert_eq!(gap.kind, "unregistered");
        assert_eq!(gap.reported_eui.as_deref(), Some("0016c001f136db1b"));
    }

    #[test]
    fn a_client_reporting_another_gateway_than_registered_is_a_mismatch() {
        let gap = gateway_gap(
            Some("0016c001f1500001"),
            Some(&info("lorawan", Some(true), Some("0016c001f136db1b"))),
        )
        .unwrap();
        assert_eq!(gap.kind, "mismatch");
        assert_eq!(gap.registered_eui.as_deref(), Some("0016c001f1500001"));
        assert_eq!(gap.reported_eui.as_deref(), Some("0016c001f136db1b"));
    }

    #[test]
    fn no_gap_without_evidence_or_without_lorawan() {
        // Registered and matching (any case), or registered and the Client says nothing.
        let ok = info("lorawan", Some(true), Some("0016C001F136DB1B"));
        assert_eq!(gateway_gap(Some("0016c001f136db1b"), Some(&ok)), None);
        assert_eq!(
            gateway_gap(
                Some("0016c001f136db1b"),
                Some(&info("dual", Some(true), None))
            ),
            None
        );
        // P2P / off / unknown mode: no forwarder to let in.
        assert_eq!(
            gateway_gap(None, Some(&info("p2p", Some(true), None))),
            None
        );
        assert_eq!(
            gateway_gap(None, Some(&info("off", Some(true), None))),
            None
        );
        let unknown = LorawanGatewayInfo {
            concentratord_running: Some(true),
            ..Default::default()
        };
        assert_eq!(gateway_gap(None, Some(&unknown)), None);
        // LoRaWAN without a running concentrator and no EUI: no card yet, not this rule.
        assert_eq!(
            gateway_gap(None, Some(&info("lorawan", Some(false), None))),
            None
        );
        assert_eq!(gateway_gap(None, Some(&info("lorawan", None, None))), None);
        // Never reported.
        assert_eq!(gateway_gap(None, None), None);
    }
}
