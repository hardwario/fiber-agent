use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use std::io::{self, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

const PORT_PATH: &str = "/dev/ttyAMA4";
const BAUD_RATE: u32 = 115_200;

/// Holds the complete data set returned by the STM32 ADC command
#[derive(Debug, Clone, Copy)]
pub struct AdcReading {
    pub raw: u16,        // The raw 12-bit ADC count
    pub pin_mv: u32,     // The voltage at the uC pin (mV)
    pub voltage_mv: u32, // The calculated real voltage (mV)
}

pub struct StmBridge {
    port: Box<dyn SerialPort>,
}

impl StmBridge {
    pub fn new() -> io::Result<Self> {
        eprintln!("Opening serial port {} at {} baud...", PORT_PATH, BAUD_RATE);

        let builder = serialport::new(PORT_PATH, BAUD_RATE)
            .data_bits(DataBits::Eight)
            .stop_bits(StopBits::One)
            .parity(Parity::None)
            .flow_control(FlowControl::None)
            .timeout(Duration::from_millis(100));

        let mut port = builder.open().map_err(|e| {
            eprintln!("Failed to open {}: {}", PORT_PATH, e);
            e
        })?;

        eprintln!("Port opened successfully.");
        eprintln!("Flushing pending data from STM...");

        // Flush boot messages
        loop {
            match Self::read_line_inner(&mut *port, Duration::from_millis(100))? {
                Some(line) => eprintln!("(boot) << {}", line),
                None => break,
            }
        }

        sleep(Duration::from_millis(200));

        Ok(Self { port })
    }

    fn read_line_inner(port: &mut dyn SerialPort, timeout: Duration) -> io::Result<Option<String>> {
        let start = Instant::now();
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];

        loop {
            match port.read(&mut byte) {
                Ok(0) => {
                    if start.elapsed() > timeout {
                        return Ok(if buf.is_empty() {
                            None
                        } else {
                            Some(String::from_utf8_lossy(&buf).into_owned())
                        });
                    }
                    sleep(Duration::from_millis(1));
                }
                Ok(_) => {
                    let b = byte[0];
                    if b == b'\n' {
                        break;
                    } else if b != b'\r' {
                        buf.push(b);
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                    if start.elapsed() > timeout {
                        if buf.is_empty() {
                            return Ok(None);
                        } else {
                            break;
                        }
                    }
                }
                Err(e) => return Err(e),
            }

            if start.elapsed() > timeout {
                break;
            }
        }

        if buf.is_empty() {
            return Ok(None);
        }

        let line = String::from_utf8_lossy(&buf).into_owned();
        Ok(Some(line))
    }

    fn read_line(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        Self::read_line_inner(&mut *self.port, timeout)
    }

    fn send_cmd(&mut self, cmd: &str) -> io::Result<Option<String>> {
        eprintln!("[STM] >> {}", cmd);
        self.port.write_all(cmd.as_bytes())?;
        self.port.write_all(b"\n")?;
        self.port.flush()?;

        let reply = self.read_line(Duration::from_millis(1000))?;
        if let Some(ref line) = reply {
            eprintln!("[STM] << {}", line);
        } else {
            eprintln!("[STM] << (no response)");
        }
        Ok(reply)
    }

    // LED helpers (generic LEDxG/LEDxR control – used by your old test screen)

    // ADC helpers (VIN/VBAT)

    fn parse_adc_line(line: &str, target_name: &str) -> Option<AdcReading> {
        let parts: Vec<&str> = line.split_whitespace().collect();

        let mut raw: Option<u16> = None;
        let mut pin: Option<u32> = None;
        let mut real: Option<u32> = None;

        let key_raw = format!("{}_raw", target_name);
        let key_pin = format!("{}_pin", target_name);
        let key_real = target_name;

        for part in parts {
            if let Some((key, val_str)) = part.split_once('=') {
                let numeric_part = val_str.trim_end_matches("mV");

                if let Ok(val) = numeric_part.parse::<u32>() {
                    if key == key_raw {
                        raw = Some(val as u16);
                    } else if key == key_pin {
                        pin = Some(val);
                    } else if key == key_real {
                        real = Some(val);
                    }
                }
            }
        }

        if let (Some(r), Some(p), Some(v)) = (raw, pin, real) {
            Some(AdcReading {
                raw: r,
                pin_mv: p,
                voltage_mv: v,
            })
        } else {
            None
        }
    }

    pub fn read_adc_data(&mut self) -> io::Result<(Option<AdcReading>, Option<AdcReading>)> {
        let vin_line_opt = self.send_cmd("VIN READ")?;
        let vin_data = if let Some(line) = vin_line_opt {
            Self::parse_adc_line(&line, "VIN")
        } else {
            None
        };

        let vbat_line_opt = self.send_cmd("VBAT READ")?;
        let vbat_data = if let Some(line) = vbat_line_opt {
            Self::parse_adc_line(&line, "VBAT")
        } else {
            None
        };

        Ok((vin_data, vbat_data))
    }

    // PWR LED helpers (PWRLEDG / PWRLEDY) – great for alarm LED mapping

    pub fn set_pwr_leds(&mut self, green_on: bool, yellow_on: bool) -> io::Result<()> {
        let g_cmd = if green_on { "PWRLEDG ON" } else { "PWRLEDG OFF" };
        let y_cmd = if yellow_on { "PWRLEDY ON" } else { "PWRLEDY OFF" };

        let _ = self.send_cmd(g_cmd)?;
        let _ = self.send_cmd(y_cmd)?;
        Ok(())
    }

    pub fn set_pwr_led_yellow(&mut self, on: bool) -> io::Result<()> {
        let cmd = if on { "PWRLEDY ON" } else { "PWRLEDY OFF" };
        let _ = self.send_cmd(cmd)?;
        Ok(())
    }

    /// Activate sensor power pins (P0-P7) at startup
    pub fn init_sensor_power(&mut self) -> io::Result<()> {
        eprintln!("[stm] Activating sensor power pins (P0-P7)...");
        for i in 0..8 {
            let cmd = format!("P{} ON", i);
            match self.send_cmd(&cmd) {
                Ok(Some(response)) => {
                    if !response.contains("OK") {
                        eprintln!("[stm] Warning: unexpected response for P{}: {}", i, response);
                    }
                }
                Ok(None) => {
                    eprintln!("[stm] Warning: no response from STM for P{}", i);
                }
                Err(e) => {
                    eprintln!("[stm] Warning: failed to activate P{}: {}", i, e);
                    // Continue trying remaining pins even if one fails
                }
            }
        }
        eprintln!("[stm] Sensor power initialization complete");
        Ok(())
    }

    /// Initialize all 8 line LEDs to OFF state at startup
    /// Ensures clean state without lingering LED colors from previous runs
    pub fn init_leds_off(&mut self) -> io::Result<()> {
        eprintln!("[stm] Initializing all LEDs to OFF...");
        for i in 0..8 {
            // Turn off both green and red LEDs for each line
            match self.set_line_leds(i, false, false) {
                Ok(_) => {
                    //eprintln!("[stm] LED {} turned OFF", i);
                }
                Err(e) => {
                    eprintln!("[stm] Warning: failed to turn off LED {}: {}", i, e);
                    // Continue with remaining LEDs even if one fails
                }
            }
        }
        eprintln!("[stm] LED initialization complete");
        Ok(())
    }

    /// Control one of the 8 line LEDs (legacy method - 2 commands):
    /// - `index` in 0..8
    /// - `green_on` / `red_on` as booleans.
    pub fn set_line_leds(&mut self, index: u8, green_on: bool, red_on: bool) -> io::Result<()> {
        let i = index as u8;

        let g_cmd = if green_on {
            format!("LED{}G ON", i)
        } else {
            format!("LED{}G OFF", i)
        };
        let r_cmd = if red_on {
            format!("LED{}R ON", i)
        } else {
            format!("LED{}R OFF", i)
        };

        let _ = self.send_cmd(&g_cmd)?;
        let _ = self.send_cmd(&r_cmd)?;
        Ok(())
    }

    /// Set LED with color and blink pattern (firmware-managed blinking)
    /// - `index`: 0-7 for line LEDs
    /// - `color`: 'O'=off, 'G'=green, 'R'=red, 'Y'=yellow
    /// - `pattern`: 'S'=steady, 'L'=slow blink, 'F'=fast blink
    pub fn set_led_state(&mut self, index: u8, color: char, pattern: char) -> io::Result<()> {
        let cmd = format!("LED {} {} {}", index, color, pattern);
        let _ = self.send_cmd(&cmd)?;
        Ok(())
    }

    /// Set power LED with color and blink pattern (firmware-managed blinking)
    /// - `color`: 'O'=off, 'G'=green, 'Y'=yellow, 'L'=lime (both)
    /// - `pattern`: 'S'=steady, 'L'=slow blink, 'F'=fast blink
    pub fn set_pwr_led_state(&mut self, color: char, pattern: char) -> io::Result<()> {
        let cmd = format!("PWR {} {}", color, pattern);
        let _ = self.send_cmd(&cmd)?;
        Ok(())
    }

    /// Sync blink phase (optional - call on startup to reset phase to 0)
    pub fn sync_blink(&mut self) -> io::Result<()> {
        let _ = self.send_cmd("SYNC")?;
        Ok(())
    }

    /// Set LED brightness (0-100%)
    /// Controls brightness for all LEDs (line LEDs + power LED) via software PWM
    pub fn set_brightness(&mut self, brightness: u8) -> io::Result<()> {
        let val = brightness.min(100);
        let cmd = format!("BRIGHT {}", val);
        let _ = self.send_cmd(&cmd)?;
        Ok(())
    }
}
