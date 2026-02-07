// FIBER Medical Thermometer main application

use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU8;
use std::io;
use std::fs;
use rppal::gpio::Gpio;
use fiber_app::{StmBridge, PowerMonitor, PowerStatus, AccelerometerMonitor, SensorMonitor, LedMonitor, Config, BuzzerController, DisplayMonitor, ButtonMonitor, QrCodeGenerator, MqttMonitor, PairingMonitor};
use fiber_app::libs::buzzer::BuzzerPriorityManager;
use fiber_app::libs::sensors::create_shared_sensor_state;
use fiber_app::libs::StorageThread;

/// Read BLE PIN from /data/ble/pin.txt
fn read_pin_from_file() -> io::Result<String> {
    let pin = fs::read_to_string("/data/ble/pin.txt")?;
    Ok(pin.trim().to_string())
}

/// Read BLE MAC address from /data/ble/mac.txt
fn read_mac_from_file() -> io::Result<String> {
    let mac = fs::read_to_string("/data/ble/mac.txt")?;
    Ok(mac.trim().to_string())
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

    // Read MAC address from file (written by ble-fiber service)
    let mac_address = match read_mac_from_file() {
        Ok(m) => {
            eprintln!("[main] MAC address read from /data/ble/mac.txt: {}", m);
            m
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to read MAC from /data/ble/mac.txt: {}", e);
            eprintln!("[main] Using placeholder MAC address");
            "00:00:00:00:00:00".to_string()
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

    let qr_generator = match QrCodeGenerator::new(mac_address, pin, hostname.clone()) {
        Ok(gen) => {
            eprintln!("[main] QR code generator initialized successfully");
            eprintln!("[main] QR content: {}", gen.get_content());
            Arc::new(gen)
        }
        Err(e) => {
            eprintln!("[main] Failed to initialize QR code generator: {}", e);
            return Err(io::Error::new(io::ErrorKind::Other, format!("QR code generation failed: {}", e)));
        }
    };

    // Create shared power status for power monitoring and display
    eprintln!("[main] Initializing shared power status...");
    let power_status = Arc::new(Mutex::new(PowerStatus::default()));
    eprintln!("[main] Power status initialized");

    // Create shared screen brightness for display backlight control (default 100%)
    eprintln!("[main] Initializing screen brightness control...");
    let screen_brightness = Arc::new(AtomicU8::new(100));
    eprintln!("[main] Screen brightness initialized at 100%");

    // Create and spawn display monitor thread
    eprintln!("[main] Starting display monitor...");
    // Get device label from config, defaulting to hostname
    let device_label = config.system.device_label.clone().unwrap_or_else(|| hostname.clone());
    let _display_monitor = DisplayMonitor::new(
        led_state.clone(),
        gpio.clone(),
        sensor_state.clone(),
        power_status.clone(),
        hostname.clone(),
        device_label,
        config.system.app_version.clone(),
        config.system.timezone_offset_hours,
        screen_brightness.clone(),
    )?;
    eprintln!("[main] Display monitor started with 250ms update interval");

    // Set QR code generator in display state
    {
        if let Ok(mut state) = _display_monitor.display_state.lock() {
            state.set_qr_generator(qr_generator.clone());
            eprintln!("[main] QR code generator attached to display state");
        }
    }

    // Create and spawn button monitor thread for screen navigation (initially without pairing)
    eprintln!("[main] Starting button monitor...");
    let _button_monitor = ButtonMonitor::new(_display_monitor.display_state.clone(), None)?;
    eprintln!("[main] Button monitor started");

    // Create buzzer controller for power monitoring alerts
    eprintln!("[main] Initializing buzzer for power management...");
    let power_buzzer = Arc::new(Mutex::new(BuzzerController::new(gpio.clone())?));

    // Create buzzer priority manager for coordinating battery and sensor critical alarms
    eprintln!("[main] Initializing buzzer priority manager...");
    let buzzer_priority_manager = Arc::new(BuzzerPriorityManager::new(power_buzzer.clone()));

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
    let (storage_handle, _storage_thread) = match StorageThread::spawn(&config.storage.db_path, config.storage.max_size_gb) {
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

    // Create and spawn MQTT monitor if enabled
    eprintln!("[main] Checking MQTT configuration...");
    eprintln!("[main]   config.mqtt present: {}", config.mqtt.is_some());
    if let Some(ref mqtt_config) = config.mqtt {
        eprintln!("[main]   config.mqtt.enabled: {}", mqtt_config.enabled);
    }

    let (mqtt_handle, mqtt_monitor) = if config.mqtt.as_ref().map(|m| m.enabled).unwrap_or(false) {
        eprintln!("[main] Starting MQTT monitor...");
        match MqttMonitor::new_with_stm(
            config.mqtt.clone().unwrap(),
            hostname.clone(),
            power_status.clone(),
            Some(stm_guard.clone()),
            Some(screen_brightness.clone()),
        ) {
            Ok(monitor) => {
                eprintln!("[main] MQTT monitor started with STM bridge and screen brightness control");
                let handle = monitor.handle();
                (Some(handle), Some(monitor))
            }
            Err(e) => {
                eprintln!("[main] Warning: Failed to start MQTT: {}", e);
                (None, None)
            }
        }
    } else {
        eprintln!("[main] MQTT disabled in configuration");
        (None, None)
    };

    // Create and spawn pairing monitor if MQTT is enabled
    let pairing_handle = if mqtt_handle.is_some() {
        eprintln!("[main] Starting pairing monitor...");
        let config_dir = std::path::Path::new("/data/fiber/config");
        match PairingMonitor::new(hostname.clone(), config_dir, _display_monitor.display_state.clone()) {
            Ok(monitor) => {
                eprintln!("[main] Pairing monitor started");
                let handle = monitor.handle();
                // Set pairing handle in MQTT monitor for routing
                if let Some(ref mqtt_mon) = mqtt_monitor {
                    mqtt_mon.set_pairing_handle(handle.clone());
                }
                // Keep monitor alive
                std::mem::forget(monitor);
                Some(handle)
            }
            Err(e) => {
                eprintln!("[main] Warning: Failed to start pairing monitor: {}", e);
                None
            }
        }
    } else {
        eprintln!("[main] Pairing monitor disabled (MQTT not enabled)");
        None
    };

    // Keep MQTT monitor alive (after pairing handle is set)
    if let Some(monitor) = mqtt_monitor {
        std::mem::forget(monitor);
    }

    // Update button monitor with pairing handle
    eprintln!("[main] Restarting button monitor with pairing support...");
    drop(_button_monitor);
    let _button_monitor = ButtonMonitor::new(_display_monitor.display_state.clone(), pairing_handle.clone())?;
    eprintln!("[main] Button monitor restarted with pairing support");

    // Create and spawn power monitoring thread with configured interval
    eprintln!("[main] Starting power monitor...");
    let _power_monitor = PowerMonitor::new(
        stm_guard.clone(),
        config.power.update_interval_ms,
        led_state.clone(),
        power_buzzer.clone(),
        buzzer_priority_manager.clone(),
        power_status.clone(),
    )?;
    eprintln!("[main] Power monitor started (interval: {}ms)", config.power.update_interval_ms);

    // Create and spawn sensor monitoring thread (pass MQTT handle)
    eprintln!("[main] Starting sensor monitor...");
    let _sensor_monitor = match SensorMonitor::new(config.sensors, stm_guard.clone(), led_state.clone(), power_buzzer.clone(), sensor_state.clone(), buzzer_priority_manager.clone(), mqtt_handle.clone(), Some(storage_handle)) {
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
