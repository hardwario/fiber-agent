// FIBER Medical Thermometer main application

use std::sync::{Arc, Mutex};
use std::io;
use std::fs;
use rppal::gpio::Gpio;
use fiber_app::{StmBridge, PowerMonitor, AccelerometerMonitor, SensorMonitor, LedMonitor, Config, BuzzerController, DisplayMonitor, ButtonMonitor, QrCodeGenerator};
use fiber_app::libs::buzzer::BuzzerPriorityManager;
use fiber_app::libs::sensors::create_shared_sensor_state;
use fiber_app::libs::StorageThread;

/// Read BLE PIN from /data/ble/pin.txt
fn read_pin_from_file() -> io::Result<String> {
    let pin = fs::read_to_string("/data/ble/pin.txt")?;
    Ok(pin.trim().to_string())
}

/// Read hostname from /etc/hostname and convert to uppercase
fn read_hostname_from_file() -> io::Result<String> {
    let hostname = fs::read_to_string("/etc/hostname")?;
    Ok(hostname.trim().to_uppercase())
}

fn main() -> io::Result<()> {
    eprintln!("[main] Starting FIBER application");

    // Load configuration from /data/fiber/config/fiber.config.yaml
    eprintln!("[main] Loading configuration...");
    let config = match Config::load_default() {
        Ok(cfg) => {
            eprintln!("[main] Configuration loaded from /data/fiber/config/fiber.config.yaml");
            cfg
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to load /data/fiber/config/fiber.config.yaml: {}", e);
            eprintln!("[main] Using default configuration");
            Config::default_config()
        }
    };

    // Display configuration info
    eprintln!(
        "[main] Config: {} v{}",
        config.system.app_name, config.system.app_version
    );
    eprintln!(
        "[main] Power update interval: {}ms",
        config.power.update_interval_ms
    );
    eprintln!(
        "[main] Serial port: {} @ {} baud",
        config.serial.port, config.serial.baud_rate
    );

    // Initialize GPIO once and share with all drivers
    eprintln!("[main] Initializing GPIO...");
    let gpio = match Gpio::new() {
        Ok(g) => {
            eprintln!("[main] GPIO initialized successfully");
            Arc::new(g)
        }
        Err(e) => {
            eprintln!("[main] Failed to initialize GPIO: {}", e);
            return Err(io::Error::new(io::ErrorKind::Other, format!("GPIO initialization failed: {}", e)));
        }
    };

    // Initialize STM32 bridge for hardware communication
    eprintln!("[main] Initializing STM32 bridge...");
    let stm = StmBridge::new()?;
    eprintln!("[main] STM32 bridge initialized successfully");

    // Initialize sensor power lines
    eprintln!("[main] Activating sensor power...");
    let stm_guard = Arc::new(Mutex::new(stm));
    {
        let mut stm_locked = stm_guard.lock().unwrap();
        stm_locked.init_sensor_power()?;
        stm_locked.init_leds_off()?;
    }
    eprintln!("[main] Sensor power activated, LEDs initialized");

    // Create and spawn dedicated LED monitoring thread
    eprintln!("[main] Starting LED monitor...");
    let _led_monitor = LedMonitor::new(stm_guard.clone())?;
    let led_state = _led_monitor.shared_state.clone();
    eprintln!("[main] LED monitor started with 50ms update interval");

    // Create shared sensor state for temperature readings
    eprintln!("[main] Initializing shared sensor state...");
    let sensor_state = create_shared_sensor_state();
    eprintln!("[main] Sensor state initialized");

    // Initialize QR code generator for Bluetooth/WiFi configuration
    eprintln!("[main] Initializing QR code generator...");

    // Read PIN from file
    let pin = match read_pin_from_file() {
        Ok(p) => {
            eprintln!("[main] PIN read from /data/ble/pin.txt");
            p
        }
        Err(e) => {
            eprintln!("[main] Failed to read PIN from /data/ble/pin.txt: {}", e);
            return Err(e);
        }
    };

    // Read hostname from file
    let hostname = match read_hostname_from_file() {
        Ok(h) => {
            eprintln!("[main] Hostname read from /etc/hostname: {}", h);
            h
        }
        Err(e) => {
            eprintln!("[main] Failed to read hostname from /etc/hostname: {}", e);
            return Err(e);
        }
    };

    let qr_generator = match QrCodeGenerator::new(hostname, pin) {
        Ok(gen) => {
            eprintln!("[main] QR code generator initialized successfully");
            Arc::new(gen)
        }
        Err(e) => {
            eprintln!("[main] Failed to initialize QR code generator: {}", e);
            return Err(io::Error::new(io::ErrorKind::Other, format!("QR code generation failed: {}", e)));
        }
    };

    // Create and spawn display monitor thread
    eprintln!("[main] Starting display monitor...");
    let _display_monitor = DisplayMonitor::new(led_state.clone(), gpio.clone(), sensor_state.clone())?;
    eprintln!("[main] Display monitor started with 250ms update interval");

    // Set QR code generator in display state
    {
        if let Ok(mut state) = _display_monitor.display_state.lock() {
            state.set_qr_generator(qr_generator.clone());
            eprintln!("[main] QR code generator attached to display state");
        }
    }

    // Create and spawn button monitor thread for screen navigation
    eprintln!("[main] Starting button monitor...");
    let _button_monitor = ButtonMonitor::new(_display_monitor.display_state.clone())?;
    eprintln!("[main] Button monitor started");

    // Create buzzer controller for power monitoring alerts
    eprintln!("[main] Initializing buzzer for power management...");
    let power_buzzer = Arc::new(Mutex::new(BuzzerController::new(gpio.clone())?));

    // Create buzzer priority manager for coordinating battery and sensor critical alarms
    eprintln!("[main] Initializing buzzer priority manager...");
    let buzzer_priority_manager = Arc::new(BuzzerPriorityManager::new(power_buzzer.clone()));

    // Create and spawn power monitoring thread with configured interval
    eprintln!("[main] Starting power monitor...");
    let _power_monitor = PowerMonitor::new(stm_guard.clone(), config.power.update_interval_ms, led_state.clone(), power_buzzer.clone(), buzzer_priority_manager.clone())?;
    eprintln!("[main] Power monitor started (interval: {}ms)", config.power.update_interval_ms);

    // Create and spawn accelerometer monitoring thread if enabled
    let _accel_monitor = if config.accelerometer.enabled {
        eprintln!("[main] Starting accelerometer monitor...");
        match AccelerometerMonitor::new(config.accelerometer) {
            Ok(monitor) => {
                eprintln!("[main] Accelerometer monitor started");
                Some(monitor)
            }
            Err(e) => {
                eprintln!("[main] Warning: Failed to initialize accelerometer: {}", e);
                None
            }
        }
    } else {
        eprintln!("[main] Accelerometer monitoring disabled in configuration");
        None
    };

    // Initialize storage thread for medical data persistence
    eprintln!("[main] Starting storage thread...");
    let (_storage_handle, _storage_thread) = match StorageThread::spawn(&config.storage.db_path, config.storage.max_size_gb) {
        Ok((handle, thread)) => {
            eprintln!(
                "[main] Storage thread started - database: {}, max size: {}GB",
                config.storage.db_path, config.storage.max_size_gb
            );
            (handle, thread)
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to initialize storage thread: {}", e);
            eprintln!("[main] Continuing without persistent storage");
            return Err(io::Error::new(io::ErrorKind::Other, format!("Storage initialization failed: {}", e)));
        }
    };

    // Create and spawn sensor monitoring thread
    eprintln!("[main] Starting sensor monitor...");
    let _sensor_monitor = match SensorMonitor::new(config.sensors, stm_guard.clone(), led_state.clone(), power_buzzer.clone(), sensor_state.clone(), buzzer_priority_manager.clone()) {
        Ok(monitor) => {
            eprintln!("[main] Sensor monitor started");
            Some(monitor)
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to initialize sensor monitor: {}", e);
            None
        }
    };

    // Application is now running with background monitoring
    eprintln!("[main] Application running with medical data persistence. Press Ctrl+C to exit.");

    // Keep the application alive
    // In a full application, this would run the main event loop
    // (button handling, display updates, sensor reading, etc.)
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // Note: PowerMonitor, AccelerometerMonitor, and SensorMonitor will be dropped here and shutdown gracefully
}
