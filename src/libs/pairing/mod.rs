//! Device Pairing Protocol
//!
//! Implements secure pairing between FIBER devices and viewer applications.
//!
//! # Protocol Flow
//!
//! 1. User triggers pairing mode (UP+DOWN buttons held for 3 seconds)
//! 2. Device generates 6-character code and displays on LCD
//! 3. User enters code in viewer application
//! 4. Viewer backend sends pairing request via MQTT
//! 5. Device validates code, generates admin keypair
//! 6. Device encrypts private key with pairing code (AES-256-GCM)
//! 7. Device signs admin certificate with its CA key
//! 8. Device sends response with encrypted key and certificate
//! 9. Viewer decrypts private key and stores credentials

pub mod ca_key;
pub mod certificate;
pub mod code;
pub mod crypto;
pub mod messages;
pub mod state;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam::channel::{self, Receiver, Sender};

use ca_key::DeviceCaKey;
use certificate::create_admin_certificate;
use code::generate_pairing_code;
use crypto::encrypt_private_key;
use messages::EncryptedKeyResponse;
use state::PairingStateMachine;
use crate::libs::display::SharedDisplayStateHandle;

/// Commands that can be sent to the pairing monitor
#[derive(Debug)]
pub enum PairingCommand {
    /// Start pairing mode (triggered by button combination)
    StartPairing,
    /// Cancel pairing mode
    CancelPairing,
    /// Process incoming pairing request
    ProcessRequest(PairingRequest),
    /// Toggle the ble_active flag (notification-only, no response).
    SetBleActive(bool),
    /// Shutdown the monitor
    Shutdown,
}

/// Response from pairing operations
#[derive(Debug)]
pub enum PairingResult {
    /// Successful pairing response to publish
    Success(PairingResponse),
    /// Error response to publish
    Error(PairingError),
}

/// Handle for communicating with the PairingMonitor
#[derive(Clone)]
pub struct PairingHandle {
    command_tx: Sender<PairingCommand>,
    result_rx: Arc<Mutex<Receiver<PairingResult>>>,
}

impl PairingHandle {
    /// Start pairing mode
    pub fn start_pairing(&self) {
        let _ = self.command_tx.send(PairingCommand::StartPairing);
    }

    /// Cancel pairing mode
    pub fn cancel_pairing(&self) {
        let _ = self.command_tx.send(PairingCommand::CancelPairing);
    }

    /// Process an incoming pairing request
    pub fn process_request(&self, request: PairingRequest) {
        let _ = self.command_tx.send(PairingCommand::ProcessRequest(request));
    }

    /// Notify the pairing monitor that a BLE client is or is no longer connected.
    pub fn set_ble_active(&self, active: bool) {
        let _ = self.command_tx.send(PairingCommand::SetBleActive(active));
    }

    /// Try to receive a pairing result (non-blocking)
    pub fn try_recv_result(&self) -> Option<PairingResult> {
        self.result_rx.lock().ok()?.try_recv().ok()
    }

    /// Shutdown the monitor
    pub fn shutdown(&self) {
        let _ = self.command_tx.send(PairingCommand::Shutdown);
    }
}

/// Shared pairing state for display coordination
pub type SharedPairingStateHandle = Arc<Mutex<PairingStateMachine>>;

/// Pairing monitor thread
pub struct PairingMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    handle: PairingHandle,
    state: SharedPairingStateHandle,
}

impl PairingMonitor {
    /// Create and start the pairing monitor
    ///
    /// # Arguments
    /// * `hostname` - Device hostname for CA ID
    /// * `config_dir` - Directory for CA key persistence
    /// * `display_state` - Shared display state for LCD updates
    pub fn new(
        hostname: String,
        config_dir: &Path,
        display_state: SharedDisplayStateHandle,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Load or generate CA key
        let ca_key = DeviceCaKey::load_or_generate(config_dir, &hostname)?;
        eprintln!(
            "[PairingMonitor] CA key loaded, ID: {}",
            ca_key.ca_id()
        );

        // Create channels
        let (command_tx, command_rx) = channel::unbounded();
        let (result_tx, result_rx) = channel::unbounded();

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let state = Arc::new(Mutex::new(PairingStateMachine::new()));
        let state_clone = state.clone();

        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(
                shutdown_flag_clone,
                command_rx,
                result_tx,
                state_clone,
                display_state,
                ca_key,
            );
        });

        let handle = PairingHandle {
            command_tx,
            result_rx: Arc::new(Mutex::new(result_rx)),
        };

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            handle,
            state,
        })
    }

    /// Get a handle for communicating with this monitor
    pub fn handle(&self) -> PairingHandle {
        self.handle.clone()
    }

    /// Get shared state handle
    pub fn state(&self) -> SharedPairingStateHandle {
        self.state.clone()
    }

    /// Main monitoring loop
    fn monitor_loop(
        shutdown_flag: Arc<AtomicBool>,
        command_rx: Receiver<PairingCommand>,
        result_tx: Sender<PairingResult>,
        state: SharedPairingStateHandle,
        display_state: SharedDisplayStateHandle,
        ca_key: DeviceCaKey,
    ) {
        eprintln!("[PairingMonitor] Started pairing monitor thread");

        let check_interval = Duration::from_millis(100);

        loop {
            // Check shutdown
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[PairingMonitor] Shutdown signal received");
                break;
            }

            // Check for expired pairing code
            {
                let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
                if state_lock.check_expiration() {
                    // Update display to exit pairing screen
                    if let Ok(mut display) = display_state.lock() {
                        display.show_sensor_overview();
                    }
                }
            }

            // Process commands (with timeout)
            match command_rx.recv_timeout(check_interval) {
                Ok(PairingCommand::StartPairing) => {
                    Self::handle_start_pairing(&state, &display_state);
                }
                Ok(PairingCommand::CancelPairing) => {
                    Self::handle_cancel_pairing(&state, &display_state);
                }
                Ok(PairingCommand::ProcessRequest(request)) => {
                    Self::handle_process_request(&state, &display_state, &ca_key, &result_tx, request);
                }
                Ok(PairingCommand::SetBleActive(active)) => {
                    let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
                    state_lock.set_ble_active(active);
                }
                Ok(PairingCommand::Shutdown) => {
                    eprintln!("[PairingMonitor] Shutdown command received");
                    break;
                }
                Err(channel::RecvTimeoutError::Timeout) => {
                    // Normal timeout, continue loop
                }
                Err(channel::RecvTimeoutError::Disconnected) => {
                    eprintln!("[PairingMonitor] Command channel disconnected");
                    break;
                }
            }
        }

        eprintln!("[PairingMonitor] Pairing monitor thread exited");
    }

    /// Handle start pairing command
    fn handle_start_pairing(
        state: &SharedPairingStateHandle,
        display_state: &SharedDisplayStateHandle,
    ) {
        let code = generate_pairing_code();
        eprintln!("[PairingMonitor] Starting pairing mode with code: {}", code);

        // Update state machine
        {
            let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
            state_lock.start_pairing(code.clone());
        }

        // Update display to show pairing screen
        if let Ok(mut display) = display_state.lock() {
            display.show_pairing(code);
        }
    }

    /// Handle cancel pairing command
    fn handle_cancel_pairing(
        state: &SharedPairingStateHandle,
        display_state: &SharedDisplayStateHandle,
    ) {
        eprintln!("[PairingMonitor] Cancelling pairing mode");

        // Update state machine
        {
            let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
            state_lock.cancel();
        }

        // Return to sensor overview
        if let Ok(mut display) = display_state.lock() {
            display.show_sensor_overview();
        }
    }

    /// Handle incoming pairing request
    fn handle_process_request(
        state: &SharedPairingStateHandle,
        display_state: &SharedDisplayStateHandle,
        ca_key: &DeviceCaKey,
        result_tx: &Sender<PairingResult>,
        request: PairingRequest,
    ) {
        eprintln!(
            "[PairingMonitor] Processing pairing request: {} from {}",
            request.request_id, request.admin_username
        );

        // Try to begin processing
        let pairing_code = {
            let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());

            // Check if expired first
            if state_lock.is_expired() {
                state_lock.cancel();
                let error = PairingError::code_expired(request.request_id.clone());
                let _ = result_tx.send(PairingResult::Error(error));
                return;
            }

            match state_lock.begin_processing(request.request_id.clone()) {
                Some(code) => code,
                None => {
                    // Not in pairing mode
                    let error = PairingError::not_in_pairing_mode(request.request_id.clone());
                    let _ = result_tx.send(PairingResult::Error(error));
                    return;
                }
            }
        };

        // Get CA's private key bytes for encryption
        // The backend needs the CA private key to sign certificates for users
        let ca_private_key = ca_key.private_key_bytes();

        // Create certificate with CA's public key
        let admin_certificate = create_admin_certificate(
            &request.admin_username,
            &ca_key.public_key_bytes(),
            ca_key,
        );

        // Encrypt CA private key with pairing code
        let encrypted = match encrypt_private_key(&ca_private_key, &pairing_code) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[PairingMonitor] Encryption failed: {}", e);
                let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
                state_lock.complete();
                let error = PairingError::new(request.request_id, "Internal error: encryption failed");
                let _ = result_tx.send(PairingResult::Error(error));
                return;
            }
        };

        // Build response
        let response = PairingResponse::new(
            request.request_id,
            ca_key.public_key_hex(),
            ca_key.ca_id(),
            admin_certificate,
            EncryptedKeyResponse {
                ciphertext: encrypted.ciphertext_base64(),
                salt: encrypted.salt_base64(),
                nonce: encrypted.nonce_base64(),
            },
        );

        // Complete pairing
        {
            let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
            state_lock.complete();
        }

        // Update display to show success briefly, then return to sensors
        if let Ok(mut display) = display_state.lock() {
            display.show_sensor_overview();
        }

        eprintln!(
            "[PairingMonitor] Pairing successful for {}",
            response.admin_certificate.signer_id
        );

        let _ = result_tx.send(PairingResult::Success(response));
    }
}

impl Drop for PairingMonitor {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);

        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

// Re-exports for convenience
pub use messages::{PairingRequest, PairingResponse, PairingError};
