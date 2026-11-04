//! Device Pairing Protocol
//!
//! Implements secure pairing between FIBER devices and viewer applications.
//!
//! # Protocol Flow
//!
//! 1. User triggers pairing mode (UP+DOWN buttons held for 2 seconds)
//! 2. Device generates 6-character code and displays on LCD
//! 3. User enters code in viewer application
//! 4. Viewer backend sends pairing request via MQTT
//! 5. Device validates code, generates admin keypair
//! 6. Device encrypts private key with pairing code (AES-256-GCM)
//! 7. Device signs admin certificate with its CA key
//! 8. Device sends response with encrypted key and certificate
//! 9. Viewer decrypts private key and stores credentials
//!
//! # Local (server) pairing
//!
//! The portal usually runs on this very device, and then step 3 is asking
//! the operator to retype a code from the LCD into a page served by the
//! same box. A request that sets `local` (`messages::PairingRequest`) skips
//! it: step 6 encrypts under
//! [`crypto::LOCAL_PAIRING_PASSWORD`] instead of the displayed code.
//!
//! Steps 1–2 are untouched and still gate everything. The window is opened
//! by hand, lives ~5 minutes, and is consumed by one request
//! (`state::PairingStateMachine::begin_processing`); `local` does not widen
//! it, it only removes the typing. Deciding a request *is* local is the
//! portal's call (it compares its own hostname against the device's), so
//! the code path stays in place for remote client hubs paired through the
//! same portal.

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

use crate::libs::display::SharedDisplayStateHandle;
use ca_key::DeviceCaKey;
use certificate::create_admin_certificate;
use code::generate_pairing_code;
use crypto::{encrypt_private_key, LOCAL_PAIRING_PASSWORD};
use messages::EncryptedKeyResponse;
use state::PairingStateMachine;

/// Commands that can be sent to the pairing monitor
#[derive(Debug)]
pub enum PairingCommand {
    /// Start pairing mode (triggered by button combination)
    StartPairing,
    /// Cancel pairing mode
    CancelPairing,
    /// Process incoming pairing request
    ProcessRequest(PairingRequest),
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
        let _ = self
            .command_tx
            .send(PairingCommand::ProcessRequest(request));
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
        eprintln!("[PairingMonitor] CA key loaded, ID: {}", ca_key.ca_id());

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
                    Self::handle_process_request(
                        &state,
                        &display_state,
                        &ca_key,
                        &result_tx,
                        request,
                    );
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
        // Update state machine
        let was_active = {
            let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
            state_lock.cancel()
        };

        if !was_active {
            // Nothing to cancel. Say so and leave the panel alone: this arrives
            // unconditionally from the BLE event router's ClientConnected arm, and
            // wiping the screen here would close a system-info or menu screen the
            // operator had just opened by holding a button.
            eprintln!("[PairingMonitor] Cancel ignored: no pairing was active");
            return;
        }

        eprintln!("[PairingMonitor] Cancelling pairing mode");

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
            "[PairingMonitor] Processing pairing request: {} from {} (local={})",
            request.request_id, request.admin_username, request.local
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
        let admin_certificate =
            create_admin_certificate(&request.admin_username, &ca_key.public_key_bytes(), ca_key);

        // Encrypt the CA private key. A local request has nobody to read the
        // LCD to, so it uses the shared constant; the pairing window above
        // still had to be open either way, and `pairing_code` was consumed
        // by `begin_processing` regardless.
        let password = if request.local {
            LOCAL_PAIRING_PASSWORD
        } else {
            &pairing_code
        };
        let encrypted = match encrypt_private_key(&ca_private_key, password) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[PairingMonitor] Encryption failed: {}", e);
                let mut state_lock = state.lock().unwrap_or_else(|e| e.into_inner());
                state_lock.complete();
                let error =
                    PairingError::new(request.request_id, "Internal error: encryption failed");
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
pub use messages::{PairingError, PairingRequest, PairingResponse};

#[cfg(test)]
mod cancel_screen_tests {
    use super::*;
    use crate::libs::display::{DisplayState, Screen};

    fn handles() -> (SharedPairingStateHandle, SharedDisplayStateHandle) {
        (
            Arc::new(Mutex::new(PairingStateMachine::new())),
            Arc::new(Mutex::new(DisplayState::new())),
        )
    }

    #[test]
    fn a_cancel_with_nothing_to_cancel_leaves_the_operators_screen_alone() {
        // BleEvent::ClientConnected calls cancel_pairing() unconditionally, so a
        // phantom connect used to wipe whatever the operator was reading — even
        // though no pairing was ever active.
        let (pairing, display) = handles();
        display.lock().unwrap().show_system_info();

        PairingMonitor::handle_cancel_pairing(&pairing, &display);

        assert!(
            matches!(
                display.lock().unwrap().current_screen,
                Screen::SystemInfo { .. }
            ),
            "a no-op cancel must not touch the screen"
        );
    }

    #[test]
    fn cancelling_a_live_pairing_returns_to_the_overview() {
        let (pairing, display) = handles();
        pairing.lock().unwrap().start_pairing("CODE12".to_string());
        display.lock().unwrap().show_pairing("CODE12".to_string());

        PairingMonitor::handle_cancel_pairing(&pairing, &display);

        assert!(matches!(
            display.lock().unwrap().current_screen,
            Screen::SensorOverview { .. }
        ));
    }
}

#[cfg(test)]
mod local_pairing_tests {
    use super::*;
    use crate::libs::display::DisplayState;
    use crypto::{decrypt_private_key, EncryptedPrivateKey};

    struct Harness {
        state: SharedPairingStateHandle,
        display: SharedDisplayStateHandle,
        ca_key: DeviceCaKey,
        result_tx: Sender<PairingResult>,
        result_rx: Receiver<PairingResult>,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let ca_key =
                DeviceCaKey::load_or_generate(dir.path(), "FIBER-TEST-001").expect("ca key");
            let (result_tx, result_rx) = channel::unbounded();
            Self {
                state: Arc::new(Mutex::new(PairingStateMachine::new())),
                display: Arc::new(Mutex::new(DisplayState::new())),
                ca_key,
                result_tx,
                result_rx,
                _dir: dir,
            }
        }

        /// Open the physical pairing window, as the operator does on the LCD.
        fn open_window(&self, code: &str) {
            self.state.lock().unwrap().start_pairing(code.to_string());
            self.display.lock().unwrap().show_pairing(code.to_string());
        }

        fn process(&self, request: PairingRequest) -> PairingResult {
            PairingMonitor::handle_process_request(
                &self.state,
                &self.display,
                &self.ca_key,
                &self.result_tx,
                request,
            );
            self.result_rx.try_recv().expect("a result is published")
        }
    }

    fn request(local: bool) -> PairingRequest {
        PairingRequest {
            request_id: "req-1".to_string(),
            timestamp: 1_704_067_200,
            admin_username: "admin@fiber.local".to_string(),
            local,
        }
    }

    fn encrypted_of(response: &PairingResponse) -> EncryptedPrivateKey {
        EncryptedPrivateKey::from_base64(
            &response.encrypted_private_key.ciphertext,
            &response.encrypted_private_key.salt,
            &response.encrypted_private_key.nonce,
        )
        .expect("well-formed base64 components")
    }

    #[test]
    fn local_request_in_the_window_encrypts_under_the_constant() {
        let h = Harness::new();
        h.open_window("WINDOW");

        let PairingResult::Success(response) = h.process(request(true)) else {
            panic!("a local request inside the window must succeed");
        };

        let encrypted = encrypted_of(&response);
        assert_eq!(
            decrypt_private_key(&encrypted, LOCAL_PAIRING_PASSWORD).unwrap(),
            h.ca_key.private_key_bytes(),
            "local pairing must hand back the CA seed under the shared constant"
        );
        assert!(
            decrypt_private_key(&encrypted, "WINDOW").is_err(),
            "the displayed code must not be what encrypted a local response"
        );
    }

    #[test]
    fn local_request_still_consumes_the_one_shot_window() {
        let h = Harness::new();
        h.open_window("WINDOW");

        assert!(matches!(
            h.process(request(true)),
            PairingResult::Success(_)
        ));

        // Second attempt without reopening the window on the LCD.
        let PairingResult::Error(err) = h.process(request(true)) else {
            panic!("the window is one-shot; a second local request must be refused");
        };
        assert_eq!(err.error, "Not in pairing mode");
    }

    #[test]
    fn local_request_outside_the_window_is_refused() {
        // The whole point of keeping the physical window: `local` removes the
        // typing, it does not make the device pair on demand.
        let h = Harness::new();

        let PairingResult::Error(err) = h.process(request(true)) else {
            panic!("a local request with no window open must be refused");
        };
        assert_eq!(err.error, "Not in pairing mode");
    }

    #[test]
    fn remote_request_still_encrypts_under_the_displayed_code() {
        let h = Harness::new();
        h.open_window("WINDOW");

        let PairingResult::Success(response) = h.process(request(false)) else {
            panic!("a remote request inside the window must succeed");
        };

        let encrypted = encrypted_of(&response);
        assert_eq!(
            decrypt_private_key(&encrypted, "WINDOW").unwrap(),
            h.ca_key.private_key_bytes(),
            "the remote path is unchanged: the displayed code is the password"
        );
        assert!(
            decrypt_private_key(&encrypted, LOCAL_PAIRING_PASSWORD).is_err(),
            "the constant must not open a remote response"
        );
    }
}
