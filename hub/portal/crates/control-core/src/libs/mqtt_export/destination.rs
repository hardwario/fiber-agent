//! Per-destination `rumqttc` adapter that implements the `Publisher` trait
//! consumed by the drain loop. Each enabled destination gets one of these
//! plus a tokio task driving its `EventLoop`.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS, Transport};
use tokio::sync::Mutex;

use super::config::DestinationConfig;
use super::drain::Publisher;

pub struct RumqttcDestination {
    pub broker_id: String,
    pub host: String,
    pub port: u16,
    client: AsyncClient,
    eventloop: Arc<Mutex<EventLoop>>,
    qos: QoS,
    hostname: String,
    /// The configured root, not a literal (plan/portal/mqtt/design.md §9.1).
    /// This was the hardcoded `fiber/` the prefix audit missed: every other
    /// publisher in the agent already took `mqtt.publish.topic_prefix`, so a
    /// flipped prefix would have moved the whole fleet *except* the
    /// store-and-forward export, which would have gone on publishing to a root
    /// nothing subscribed to — and silently, because a publish to an
    /// unsubscribed topic is not an error anywhere.
    topic_prefix: String,
}

impl RumqttcDestination {
    /// Build a destination wrapper from config. The returned object holds
    /// the eventloop until `spawn_event_loop` is called; without that call
    /// no I/O happens and `publish` will simply block on the channel.
    pub fn connect(
        cfg: &DestinationConfig,
        hostname: &str,
        topic_prefix: &str,
        publish_qos: u8,
    ) -> Result<Self, String> {
        let client_id = if cfg.client_id.is_empty() {
            format!("fiber-export-{}-{}", hostname, cfg.broker_id)
        } else {
            cfg.client_id.clone()
        };
        let mut opts = MqttOptions::new(client_id, &cfg.host, cfg.port);
        opts.set_keep_alive(Duration::from_secs(30));
        // Export batches go out to a customer broker — be generous outgoing; incoming is
        // only acks. Stated rather than inherited (rumqttc defaults to 10 KiB).
        opts.set_max_packet_size(256 * 1024, 256 * 1024);
        if !cfg.username.is_empty() {
            opts.set_credentials(cfg.username.clone(), cfg.password.clone());
        }
        if cfg.tls.enabled {
            let ca = std::fs::read(&cfg.tls.ca_cert_path)
                .map_err(|e| format!("read CA cert {}: {}", cfg.tls.ca_cert_path, e))?;
            // Built manually (rather than TlsConfiguration::SimpleNative) so we can
            // disable the OS root store — trust only the configured CA.
            use rumqttc::tokio_native_tls::native_tls::{Certificate, TlsConnector};
            let mut builder = TlsConnector::builder();
            builder.disable_built_in_roots(true);
            let ca_cert = Certificate::from_pem(&ca)
                .map_err(|e| format!("invalid CA cert {}: {}", cfg.tls.ca_cert_path, e))?;
            builder.add_root_certificate(ca_cert);
            let connector = builder
                .build()
                .map_err(|e| format!("failed to build TLS connector: {}", e))?;
            opts.set_transport(Transport::tls_with_config(
                rumqttc::TlsConfiguration::NativeConnector(connector),
            ));
        }
        let (client, eventloop) = AsyncClient::new(opts, 100);

        Ok(Self {
            broker_id: cfg.broker_id.clone(),
            host: cfg.host.clone(),
            port: cfg.port,
            client,
            eventloop: Arc::new(Mutex::new(eventloop)),
            qos: match publish_qos {
                0 => QoS::AtMostOnce,
                2 => QoS::ExactlyOnce,
                _ => QoS::AtLeastOnce,
            },
            hostname: hostname.to_string(),
            topic_prefix: topic_prefix.to_string(),
        })
    }

    /// Drive the rumqttc eventloop in the background. Must be called from a
    /// tokio runtime. Errors are logged and the loop keeps polling so the
    /// destination self-heals across network blips.
    pub fn spawn_event_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let eventloop = self.eventloop.clone();
        let id = self.broker_id.clone();
        tokio::spawn(async move {
            let mut g = eventloop.lock().await;
            loop {
                match g.poll().await {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[mqtt_export:{}] eventloop error: {} (retrying)", id, e);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        })
    }
}

#[async_trait::async_trait]
impl Publisher for RumqttcDestination {
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), String> {
        // Prepend root and hostname so the topic becomes
        // {prefix}/{host}/export/… — two segments before the area, like every
        // other family (design.md §9.3).
        let full = format!("{}/{}/{}", self.topic_prefix, self.hostname, topic);
        self.client
            .publish(full, self.qos, false, payload.to_vec())
            .await
            .map_err(|e| format!("publish: {}", e))
    }
}
