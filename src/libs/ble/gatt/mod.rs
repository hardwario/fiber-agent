//! BLE GATT server: BleMonitor + BleHandle.
//!
//! BleMonitor spawns a dedicated thread that runs a multi-thread Tokio
//! runtime. The runtime drives the bluer GATT server. Communication with
//! the rest of the application happens over crossbeam channels (commands
//! in, events out). BleMonitor does not hold references to display_state,
//! pairing_handle, sensor_state, mqtt, or storage — that integration lives
//! in the event_router thread (see crate::libs::ble::event_router).

pub mod auth;
pub mod device_info;
pub mod lan;
pub mod net_error;
pub mod service;
pub mod sticker;
pub mod state;
pub mod terminal;
pub mod time_sync;
pub mod wifi;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam::channel::{self, Receiver, Sender};

use super::config::BleConfig;
use crate::libs::config_applier::ConfigApplier;
use crate::libs::network::SharedProvisioningSession;

#[derive(Debug, Clone)]
pub enum BleCommand {
    EnableAdvertising,
    DisableAdvertising,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum BleEvent {
    ClientConnected { addr: String },
    ClientDisconnected,
    AuthSuccess,
    AuthFailed,
    WifiConnecting { ssid: String },
    WifiConnected { ssid: String, ip: String },
    WifiFailed { error: String },
    LanConfigured { mode: String, ip: String },
    LanFailed { error: String },
}

#[derive(Clone)]
pub struct BleHandle {
    command_tx: Sender<BleCommand>,
    event_rx: Arc<std::sync::Mutex<Receiver<BleEvent>>>,
}

impl BleHandle {
    pub fn enable_advertising(&self) {
        let _ = self.command_tx.send(BleCommand::EnableAdvertising);
    }
    pub fn disable_advertising(&self) {
        let _ = self.command_tx.send(BleCommand::DisableAdvertising);
    }
    pub fn shutdown(&self) {
        let _ = self.command_tx.send(BleCommand::Shutdown);
    }
    /// Non-blocking receive of the next event. Returns None if no event
    /// is available right now or if the channel has been closed.
    pub fn try_recv_event(&self) -> Option<BleEvent> {
        self.event_rx.lock().ok()?.try_recv().ok()
    }
}

pub struct BleMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    handle: BleHandle,
}

impl BleMonitor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: BleConfig,
        provisioning_session: SharedProvisioningSession,
        config_applier: Option<Arc<ConfigApplier>>,
        storage: Option<crate::libs::storage::StorageHandle>,
        lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        lorawan_state_slot: Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>,
        >,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (command_tx, command_rx) = channel::unbounded::<BleCommand>();
        let (event_tx_xbeam, event_rx) = channel::unbounded::<BleEvent>();

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let thread_handle = thread::Builder::new()
            .name("ble-monitor".to_string())
            .spawn(move || {
                Self::thread_main(
                    config,
                    provisioning_session,
                    config_applier,
                    storage,
                    lorawan_configs,
                    lorawan_state_slot,
                    command_rx,
                    event_tx_xbeam,
                    shutdown_flag_clone,
                );
            })?;

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            handle: BleHandle {
                command_tx,
                event_rx: Arc::new(std::sync::Mutex::new(event_rx)),
            },
        })
    }

    pub fn handle(&self) -> BleHandle {
        self.handle.clone()
    }

    #[allow(clippy::too_many_arguments)]
    fn thread_main(
        config: BleConfig,
        provisioning_session: SharedProvisioningSession,
        config_applier: Option<Arc<ConfigApplier>>,
        storage: Option<crate::libs::storage::StorageHandle>,
        lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        lorawan_state_slot: Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>,
        >,
        command_rx: Receiver<BleCommand>,
        event_tx: Sender<BleEvent>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        eprintln!("[BleMonitor] Thread started");

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[BleMonitor] FATAL: Tokio runtime build failed: {}", e);
                return;
            }
        };

        runtime.block_on(async move {
            if let Err(e) = run_server(
                config,
                provisioning_session,
                config_applier,
                storage,
                lorawan_configs,
                lorawan_state_slot,
                command_rx,
                event_tx,
                shutdown_flag,
            )
            .await
            {
                eprintln!("[BleMonitor] FATAL: GATT server returned error: {:?}", e);
            }
        });

        eprintln!("[BleMonitor] Thread exited");
    }
}

impl Drop for BleMonitor {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        self.handle.shutdown();
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(3);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Async GATT server entry point — analogue of run_server() in ble-fiber.
#[allow(clippy::too_many_arguments)]
async fn run_server(
    config: BleConfig,
    provisioning_session: SharedProvisioningSession,
    config_applier: Option<Arc<ConfigApplier>>,
    storage: Option<crate::libs::storage::StorageHandle>,
    lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
    lorawan_state_slot: Arc<std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>>,
    command_rx: Receiver<BleCommand>,
    event_tx_xbeam: Sender<BleEvent>,
    shutdown_flag: Arc<AtomicBool>,
) -> bluer::Result<()> {
    use bluer::adv::Advertisement;
    use futures::{pin_mut, StreamExt};
    use tokio::sync::Mutex;

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    // Register a pairing agent that auto-accepts incoming bonds. We don't
    // depend on the SMP bond for security — FIBER auth is the app-layer
    // FB01 PIN derived from the QR code shown on the device — but some
    // Android builds proactively call createBond() right after the GATT
    // connect. Without a registered agent, bluez rejects the SMP and the
    // user sees a "Pair rejected by FIBER" toast plus a disconnect.
    //
    // Capability is *implicit* in bluer 0.17.4: it's derived from which
    // callbacks are populated (see bluer/src/agent.rs::capability). With
    // request_confirmation + request_authorization + authorize_service
    // all set, the published capability is "DisplayYesNo", so bluez
    // negotiates Numeric Comparison pairing — the phone shows a 6-digit
    // passkey, user taps Confirm, bond completes. The bond persists on
    // the phone, so subsequent connections come in silently.
    //
    // request_confirmation auto-accepts without comparing the passkey
    // against anything on the device side (the LCD doesn't display it).
    // That nominally breaks the MITM-protection part of Numeric
    // Comparison, but the threat model already assumes the user is
    // physically at the device (they just scanned the QR code), and the
    // FB01 PIN flow is the real auth gate.
    //
    // History: 546d31b removed all agent / set_pairable calls on
    // suspicion that they wedged the MGMT socket and broke external
    // btmgmt invocations. Since 148ccae we no longer call btmgmt at all
    // (advertising routes through bluer's D-Bus API), so any MGMT-state
    // side effect from registering the agent is harmless. If
    // bluer.advertise() turns out to regress, that's the next thing to
    // check — see the commit message for context.
    let _agent_handle = session
        .register_agent(bluer::agent::Agent {
            request_default: false,
            request_authorization: Some(Box::new(|req| {
                Box::pin(async move {
                    eprintln!(
                        "[BleAgent] Just Works pairing from {} → accepting",
                        req.device
                    );
                    Ok(())
                })
            })),
            request_confirmation: Some(Box::new(|req| {
                Box::pin(async move {
                    eprintln!(
                        "[BleAgent] Numeric Comparison from {} passkey={:06} → accepting",
                        req.device, req.passkey
                    );
                    Ok(())
                })
            })),
            authorize_service: Some(Box::new(|req| {
                Box::pin(async move {
                    eprintln!(
                        "[BleAgent] Service authorization from {} uuid={} → accepting",
                        req.device, req.service
                    );
                    Ok(())
                })
            })),
            ..Default::default()
        })
        .await?;

    // Pairable must be true for the Just Works bond above to actually go
    // through — otherwise bluez refuses the SMP regardless of the agent.
    adapter.set_pairable(true).await?;

    let mac = adapter.address().await?;
    let mac_str = mac.to_string();

    let hostname = state::get_hostname();

    let advertising_name = config.advertising_name.clone().unwrap_or_else(|| hostname.clone());
    adapter.set_alias(advertising_name.clone()).await?;

    let state = Arc::new(Mutex::new(state::ServiceState::new(
        provisioning_session,
        hostname.clone(),
        mac_str.clone(),
        config_applier,
        storage,
        lorawan_configs,
        lorawan_state_slot,
    )));

    // Bridge crossbeam Sender to a tokio mpsc that GATT closures can move into.
    let (event_tx_async, mut event_rx_async) = tokio::sync::mpsc::channel::<BleEvent>(64);
    let event_tx_xbeam_for_bridge = event_tx_xbeam.clone();
    tokio::spawn(async move {
        while let Some(ev) = event_rx_async.recv().await {
            if event_tx_xbeam_for_bridge.send(ev).is_err() {
                break;
            }
        }
    });

    let app = service::create_gatt_app(state.clone(), event_tx_async, config.enable_terminal).await?;

    eprintln!("[BleMonitor] Registering GATT application...");
    let app_handle = adapter.serve_gatt_application(app).await?;

    // Primary AD: Flags + 128-bit FIBER service UUID (~21 B, fits the
    // legacy 31-byte adv_data buffer). Scan response: the adapter alias,
    // requested via `Includes = ["local-name"]` rather than the explicit
    // `LocalName` property.
    //
    // Why `system_includes` instead of `local_name`: setting `LocalName`
    // makes bluez try to fit the literal string into the primary AD, and
    // when that overflows it escalates to EXTENDED advertising via
    // MGMT_OP_ADD_EXT_ADV_DATA — which BCM4345C0 firmware rejects (the
    // remnant of the periphid regression; PR #7023 fixed the BT bringup
    // path but not the extended-adv-data path). With `Includes` carrying
    // the MGMT_ADV_FLAG_LOCAL_NAME hint, the kernel itself places the
    // alias into the scan-response slot during legacy
    // MGMT_OP_ADD_ADVERTISING, so the request stays on the legacy path
    // the chip handles fine — and the phone sees the name in the scan
    // response before connection (matching app-side name filters).
    let adv = Advertisement {
        service_uuids: std::iter::once(service::FIBER_SERVICE_UUID).collect(),
        system_includes: std::iter::once(bluer::adv::Feature::LocalName).collect(),
        ..Default::default()
    };
    let adv_handle = adapter.advertise(adv).await?;
    eprintln!(
        "[BleMonitor] BLE advertising started (UUID-only, alias={}, mac={})",
        advertising_name, mac_str
    );

    let events = adapter.events().await?;
    pin_mut!(events);

    loop {
        if shutdown_flag.load(Ordering::Relaxed) { break; }

        // Drain any pending commands (non-blocking).
        while let Ok(cmd) = command_rx.try_recv() {
            match cmd {
                BleCommand::Shutdown => {
                    shutdown_flag.store(true, Ordering::Relaxed);
                }
                BleCommand::EnableAdvertising | BleCommand::DisableAdvertising => {
                    // Phase 1: advertising is always on while the monitor is alive.
                    // These commands are reserved for future use.
                }
            }
        }

        tokio::select! {
            Some(event) = events.next() => {
                use bluer::AdapterEvent;
                match event {
                    AdapterEvent::DeviceAdded(addr) => {
                        let addr_str = addr.to_string();
                        eprintln!("[BleMonitor] Client connected: {}", addr_str);
                        {
                            let st = state.lock().await;
                            st.authenticated.store(false, Ordering::SeqCst);
                        }
                        let _ = event_tx_xbeam.send(BleEvent::ClientConnected { addr: addr_str });
                    }
                    AdapterEvent::DeviceRemoved(addr) => {
                        eprintln!("[BleMonitor] Client disconnected: {}", addr);
                        let mut st = state.lock().await;
                        st.authenticated.store(false, Ordering::SeqCst);
                        if let Some(shell_arc) = st.shell_process.take() {
                            if let Ok(mut shell) = shell_arc.try_lock() {
                                shell.cancel_token.cancel();
                                let _ = shell.child.start_kill();
                            }
                        }
                        st.terminal_notifier = None;
                        // Cancel any in-flight FB0D enrollment and clear its
                        // result slot — a slow ChirpStack add would otherwise
                        // keep running and leak its outcome (deveui + final
                        // message) to whichever client connects next.
                        if let Some(task) = st.sticker_task.take() {
                            task.abort();
                        }
                        crate::libs::ble::gatt::sticker::reset(&st.sticker_result);
                        let _ = event_tx_xbeam.send(BleEvent::ClientDisconnected);
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }

    eprintln!("[BleMonitor] Cleaning up...");
    drop(adv_handle);
    drop(app_handle);
    Ok(())
}
