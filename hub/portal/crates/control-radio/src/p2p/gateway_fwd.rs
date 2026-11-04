//! The Client Hub's gateway forwarder — `plan/control/radio/p2p_cluster_gateway.md` §3.
//!
//! On a Client Hub the P2P central does not run. Its Northbridge is a remote gateway of the
//! Server's central, and this forwarder bridges the link frames, undecoded, between the
//! `ttyAMA3` UART and the gateway plane ([`crate::p2p::plane`]) on the Server's broker:
//! every deframed `RSP` and event goes `up`, every `down` command is written to the UART with
//! the central's `seq` unchanged. It holds nothing: the session keys the central installs live
//! in the Northbridge's RAM only.
//!
//! The broker connection is the agent's (cluster.md §4.4): the Server's host and port and the
//! `peer-<hostname>` account from the rendered `mqtt.broker` override, TLS against the
//! Server's CA. `proximos-control` runs as `proximos` and cannot read
//! `/data/proximos/cluster/` (0700 root), so systemd hands the password and the CA over as
//! credentials ([`CREDENTIAL_PASSWORD`], [`CREDENTIAL_CA`]).

use std::time::Duration;

use rumqttc::{
    AsyncClient, Event, Incoming, LastWill, MqttOptions, Outgoing, QoS, TlsConfiguration,
    Transport as MqttTransport,
};
use serde::Deserialize;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::northbridge::{NorthbridgeStatus, UpdateSummary};
use crate::p2p::plane::{self, GatewayState, NorthbridgeLink, PlaneFrame, UpFrame};
use crate::p2p::uart::Northbridge;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

macro_rules! diag {
    ($($arg:tt)*) => { eprintln!("[p2p-gw {}] {}", now_ms(), format_args!($($arg)*)) };
}

/// The systemd credential carrying `/data/proximos/cluster/peer_password`.
pub const CREDENTIAL_PASSWORD: &str = "cluster-peer-password";
/// The systemd credential carrying `/data/proximos/cluster/ca.crt`.
pub const CREDENTIAL_CA: &str = "cluster-ca";

/// The rendered `mqtt.broker` override of a Client (cluster.md §4.4), the half this process may
/// read: where the Server is and which account to use. `password_file` and `tls.ca_cert_path`
/// name root-only files; their content arrives as credentials.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClientBroker {
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Deserialize)]
struct Doc {
    #[serde(default)]
    mqtt: Option<MqttSection>,
}

#[derive(Deserialize)]
struct MqttSection {
    #[serde(default)]
    broker: Option<ClientBroker>,
}

impl ClientBroker {
    /// The override from the rendered device config, `None` on a Server (no `mqtt.broker`).
    /// A config that does not parse is a Server here too: `dispatch::load_config` refuses it
    /// on its own, with the reason.
    pub fn from_device_config_yaml(s: &str) -> Option<Self> {
        serde_yaml::from_str::<Doc>(s)
            .ok()?
            .mqtt?
            .broker
            .filter(|b| !b.host.is_empty() && !b.username.is_empty())
    }
}

/// Everything a forwarder run needs.
pub struct ForwarderConfig {
    /// This Hub's hostname: the plane's `<gw>`.
    pub gw: String,
    pub broker: ClientBroker,
    /// The peer account's password, from [`CREDENTIAL_PASSWORD`].
    pub password: String,
    /// The Server's CA, PEM, from [`CREDENTIAL_CA`].
    pub ca_pem: Vec<u8>,
    /// This Hub's image version, for the state document.
    pub image: Option<String>,
}

impl std::fmt::Debug for ForwarderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwarderConfig")
            .field("gw", &self.gw)
            .field("broker", &self.broker)
            .field("password", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn cfg_err(m: impl Into<String>) -> Error {
    Error::new(
        Kind::NotBootstrapped,
        Transport::P2p,
        Component::MqttBroker,
        m.into(),
    )
}

/// The value the unit's `SetCredential=` fallback gives a credential whose file is missing, on a
/// Hub that is not a Client. systemd 259 rejects an empty `SetCredential=` value, so the
/// fallback is this word.
pub const CREDENTIAL_ABSENT: &str = "absent";

/// Read one systemd credential; `None` when it is missing, empty or [`CREDENTIAL_ABSENT`].
pub fn read_credential(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    credential_value(std::fs::read(std::path::Path::new(&dir).join(name)).ok()?)
}

fn credential_value(b: Vec<u8>) -> Option<Vec<u8>> {
    let t = b.trim_ascii();
    (!t.is_empty() && t != CREDENTIAL_ABSENT.as_bytes()).then_some(b)
}

impl ForwarderConfig {
    /// The forwarder's config from the rendered override and the two credentials. An error names
    /// what is missing, never a value.
    pub fn from_credentials(
        gw: String,
        broker: ClientBroker,
        image: Option<String>,
    ) -> Result<Self> {
        let password = read_credential(CREDENTIAL_PASSWORD)
            .ok_or_else(|| {
                cfg_err(format!(
                    "this Hub is a cluster Client but the credential {CREDENTIAL_PASSWORD} is \
                     missing or empty: the unit must load /data/proximos/cluster/peer_password"
                ))
            })
            .and_then(|b| {
                String::from_utf8(b)
                    .map(|s| s.trim().to_string())
                    .map_err(|_| cfg_err(format!("credential {CREDENTIAL_PASSWORD} is not UTF-8")))
            })?;
        let ca_pem = read_credential(CREDENTIAL_CA).ok_or_else(|| {
            cfg_err(format!(
                "this Hub is a cluster Client but the credential {CREDENTIAL_CA} is missing or \
                 empty: refusing to reach the Server without its CA"
            ))
        })?;
        Ok(ForwarderConfig {
            gw,
            broker,
            password,
            ca_pem,
            image,
        })
    }

    /// The broker session: client id `peer-<hostname>-p2p` beside the agent's own, the last
    /// will `offline` on the state topic, TLS against the Server's CA.
    pub fn mqtt_options(&self) -> MqttOptions {
        let mut o = MqttOptions::new(
            format!("{}-p2p", self.broker.username),
            self.broker.host.clone(),
            self.broker.port,
        );
        o.set_credentials(self.broker.username.clone(), self.password.clone());
        o.set_keep_alive(Duration::from_secs(20));
        o.set_clean_session(true);
        o.set_max_packet_size(64 * 1024, 64 * 1024);
        o.set_last_will(LastWill::new(
            plane::state_topic(&self.gw),
            GatewayState::offline().to_json(),
            QoS::AtLeastOnce,
            true,
        ));
        o.set_transport(MqttTransport::tls_with_config(TlsConfiguration::Simple {
            ca: self.ca_pem.clone(),
            alpn: None,
            client_auth: None,
        }));
        o
    }
}

/// Why the forwarder's caller stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The process is going down: the state says `offline`.
    Shutdown,
    /// A Northbridge firmware update was asked for: the state says the Northbridge is
    /// `updating`, its update `flashing`, until the forwarder is back (§2).
    Update,
}

/// What the broker task tells the bridge loop.
enum FromBroker {
    Connected,
    Down(Vec<u8>),
}

/// Run the forwarder until `stop` resolves or the UART dies. The UART is opened by the caller
/// (after the Northbridge firmware check), the broker session here; broker outages are ridden
/// out by rumqttc's reconnect, and every reconnect republishes the state, with the firmware
/// update `status` holds.
pub async fn run(
    cfg: ForwarderConfig,
    mut nb: Northbridge,
    status: NorthbridgeStatus,
    stop: impl std::future::Future<Output = Stop> + Send,
) -> Result<()> {
    let (client, mut eventloop) = AsyncClient::new(cfg.mqtt_options(), 128);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<FromBroker>(256);
    let down = plane::down_topic(&cfg.gw);
    let broker_task = {
        let (client, down) = (client.clone(), down.clone());
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        failures = 0;
                        // A clean session forgets the subscription: renew it on every connect.
                        if client
                            .subscribe(down.clone(), QoS::AtMostOnce)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if tx.send(FromBroker::Connected).await.is_err() {
                            break;
                        }
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) if p.topic == down => {
                        if tx.send(FromBroker::Down(p.payload.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    // Written and flushed after the last state: the session is over.
                    Ok(Event::Outgoing(Outgoing::Disconnect)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        if failures == 1 || failures.is_multiple_of(30) {
                            diag!("Server broker: {e} (attempt {failures}); retrying");
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        })
    };

    let state_topic = plane::state_topic(&cfg.gw);
    let up = plane::up_topic(&cfg.gw);
    let mut forwarded_up = 0u64;
    let mut forwarded_down = 0u64;
    let mut refused = 0u64;
    tokio::pin!(stop);
    let result = loop {
        tokio::select! {
            why = &mut stop => break Ok(why),
            frames = nb.read_frames() => {
                let frames = match frames {
                    Ok(f) => f,
                    Err(e) => break Err(e),
                };
                let host_unix_ms = u64::try_from(now_ms()).unwrap_or(0);
                for f in frames {
                    if !plane::is_answer_or_event(f.frame_type) {
                        refused += 1;
                        continue;
                    }
                    let msg = plane::encode_up(&UpFrame {
                        host_unix_ms,
                        frame: PlaneFrame { frame_type: f.frame_type, seq: f.seq, body: f.body },
                    });
                    // Never wait on the broker: an outage must not stall the UART reader. A frame
                    // lost here is what the link already survives (ring resend, RSP timeout).
                    if client.try_publish(up.clone(), QoS::AtMostOnce, false, msg).is_ok() {
                        forwarded_up += 1;
                    }
                }
            }
            msg = rx.recv() => match msg {
                None => break Err(cfg_err("the Server broker task ended")),
                Some(FromBroker::Connected) => {
                    diag!(
                        "connected to {}:{} as {}; forwarding (up {forwarded_up}, down \
                         {forwarded_down}, refused {refused})",
                        cfg.broker.host, cfg.broker.port, cfg.broker.username
                    );
                    let online = GatewayState::online(cfg.image.clone(), NorthbridgeLink::Up)
                        .with_update(status.snapshot().update.as_ref());
                    let _ = client
                        .publish(state_topic.clone(), QoS::AtLeastOnce, true, online.to_json())
                        .await;
                }
                Some(FromBroker::Down(b)) => match plane::decode_down(&b) {
                    Ok(f) => {
                        if let Err(e) = nb.write_frame(f.frame_type, f.seq, &f.body).await {
                            break Err(e);
                        }
                        forwarded_down += 1;
                    }
                    Err(e) => {
                        refused += 1;
                        diag!("down message refused: {e}");
                    }
                },
            },
        }
    };
    // Say why the gateway went away rather than leaving the will to guess: a shutdown is an
    // orderly offline, an update a Northbridge being flashed, a dead UART a gateway whose
    // Northbridge is down.
    let last = match &result {
        Ok(Stop::Shutdown) => GatewayState::offline(),
        Ok(Stop::Update) => {
            let update = status.snapshot().update.map(|u| UpdateSummary {
                state: "flashing",
                ..u
            });
            GatewayState::online(cfg.image.clone(), NorthbridgeLink::Updating)
                .with_update(update.as_ref())
        }
        Err(_) => GatewayState::online(cfg.image.clone(), NorthbridgeLink::Down)
            .with_update(status.snapshot().update.as_ref()),
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        client.publish(state_topic, QoS::AtLeastOnce, true, last.to_json()),
    )
    .await;
    // Both are only queued: the broker task has to write them. A clean DISCONNECT also keeps
    // the broker from publishing the will over the state just sent.
    let _ = tokio::time::timeout(Duration::from_secs(1), client.disconnect()).await;
    let mut broker_task = broker_task;
    if tokio::time::timeout(Duration::from_secs(3), &mut broker_task)
        .await
        .is_err()
    {
        broker_task.abort();
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_units_fallback_reads_as_no_credential() {
        assert_eq!(credential_value(b"absent".to_vec()), None);
        assert_eq!(credential_value(b"absent\n".to_vec()), None);
        assert_eq!(credential_value(b" \n".to_vec()), None);
        assert_eq!(
            credential_value(b"s3cret\n".to_vec()),
            Some(b"s3cret\n".to_vec())
        );
    }

    #[test]
    fn the_override_is_read_from_a_clients_rendered_config_only() {
        let client = r#"
radio:
  mode: p2p
mqtt:
  topic_prefix: "proximos"
  broker:
    host: "proximos-4e970f4a.local"
    port: 8883
    username: "peer-proximos-2200ad33"
    password_file: "/data/proximos/cluster/peer_password"
  tls:
    enabled: true
    ca_cert_path: "/data/proximos/cluster/ca.crt"
"#;
        assert_eq!(
            ClientBroker::from_device_config_yaml(client),
            Some(ClientBroker {
                host: "proximos-4e970f4a.local".into(),
                port: 8883,
                username: "peer-proximos-2200ad33".into(),
            })
        );
        let server = "radio:\n  mode: p2p\nmqtt:\n  topic_prefix: \"proximos\"\n";
        assert_eq!(ClientBroker::from_device_config_yaml(server), None);
        assert_eq!(ClientBroker::from_device_config_yaml(": not yaml ["), None);
    }

    #[test]
    fn the_session_has_its_own_client_id_and_an_offline_will() {
        let cfg = ForwarderConfig {
            gw: "proximos-2200ad33".into(),
            broker: ClientBroker {
                host: "proximos-4e970f4a.local".into(),
                port: 8883,
                username: "peer-proximos-2200ad33".into(),
            },
            password: "secret-password-value!".into(),
            ca_pem: b"-----BEGIN CERTIFICATE-----".to_vec(),
            image: None,
        };
        let o = cfg.mqtt_options();
        assert_eq!(o.client_id(), "peer-proximos-2200ad33-p2p");
        assert_eq!(
            o.broker_address(),
            ("proximos-4e970f4a.local".to_string(), 8883)
        );
        let will = o.last_will().expect("a will");
        assert_eq!(
            will.topic,
            "proximos/proximos-2200ad33/radio/p2p-gateway/state"
        );
        assert!(will.retain);
        assert_eq!(&will.message[..], &GatewayState::offline().to_json()[..]);
        assert!(
            !format!("{cfg:?}").contains("secret"),
            "Debug redacts the password"
        );
    }
}
