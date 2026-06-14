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
pub mod service;
pub mod state;
pub mod terminal;
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
    pub fn new(
        config: BleConfig,
        provisioning_session: SharedProvisioningSession,
        config_applier: Option<Arc<ConfigApplier>>,
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

    fn thread_main(
        config: BleConfig,
        provisioning_session: SharedProvisioningSession,
        config_applier: Option<Arc<ConfigApplier>>,
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
async fn run_server(
    config: BleConfig,
    provisioning_session: SharedProvisioningSession,
    config_applier: Option<Arc<ConfigApplier>>,
    command_rx: Receiver<BleCommand>,
    event_tx_xbeam: Sender<BleEvent>,
    shutdown_flag: Arc<AtomicBool>,
) -> bluer::Result<()> {
    use futures::{pin_mut, StreamExt};
    use tokio::sync::Mutex;

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    // Register a "Just Works" (NoInputNoOutput) pairing agent. Otherwise,
    // some phones (Android in particular) initiate BLE SMP bonding on first
    // GATT connect; with no agent registered bluez rejects the bond and the
    // phone disconnects + retries in a loop, surfacing as "Pair rejected"
    // on the phone UI. Accepting authorization unconditionally makes the
    // bond succeed silently — the actual FIBER auth still happens at the
    // app layer via the FB01 PIN characteristic; this agent only satisfies
    // the OS-level BLE bonding handshake.
    let _agent_handle = session
        .register_agent(bluer::agent::Agent {
            request_default: false,
            request_authorization: Some(Box::new(|_req| {
                Box::pin(async move { Ok(()) })
            })),
            authorize_service: Some(Box::new(|_req| {
                Box::pin(async move { Ok(()) })
            })),
            ..Default::default()
        })
        .await?;

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

    // Pairable=true plus the NoInputNoOutput Just Works agent registered
    // above lets BLE SMP bonding succeed silently — phones (Android
    // especially) initiate bonding on first GATT connect and disconnect-loop
    // if it's rejected. Classic BT Discoverable stays OFF so the device only
    // appears once on phones (via LE scan only); General Discoverable in the
    // LE advertisement payload (`btmgmt add-adv -g`) handles LE visibility.
    // The actual FIBER auth still runs at the app layer via FB01 PIN — the
    // SMP bond is just satisfying the OS handshake.
    adapter.set_pairable(true).await?;

    // bluer/bluez 5.72 would issue MGMT_OP_ADD_EXT_ADV_DATA here, which the
    // BT 4.2-only BCM4345C0 on the Pi CM4 rejects with Invalid Parameters.
    // Drive the legacy advertising path via btmgmt directly instead — the
    // GATT app is already registered via `serve_gatt_application` above, so
    // this just attaches the advertising data side. See
    // crate::libs::ble::advertising::start_persistent_advertising for the
    // full story.
    let service_uuid_str = service::FIBER_SERVICE_UUID.hyphenated().to_string();
    if let Err(e) = crate::libs::ble::advertising::start_persistent_advertising(&service_uuid_str) {
        eprintln!("[BleMonitor] WARN: failed to start LE advertisement: {}", e);
    } else {
        eprintln!("[BleMonitor] BLE advertising started (name={}, mac={})", advertising_name, mac_str);
    }

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
                        let _ = event_tx_xbeam.send(BleEvent::ClientDisconnected);
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }

    eprintln!("[BleMonitor] Cleaning up...");
    let _ = crate::libs::ble::advertising::stop_persistent_advertising();
    drop(app_handle);
    Ok(())
}
