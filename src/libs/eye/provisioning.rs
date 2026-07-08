//! Provision a Teltonika EYE Sensor over BLE GATT.
//!
//! Sequence reverse-engineered from the official EYE APP (HCI snoop) and
//! verified on hardware. **No pairing/bonding/encryption is required** — the
//! config characteristics are gated by a plain application-level password.
//!
//! 1. connect (plain LE)
//! 2. write ASCII `"123456"` (6 bytes) to Password `e61c0008` (with response)
//!    → unlocks the config characteristics
//! 3. write the profile characteristics
//! 4. write `0x0010` (big-endian bytes `00 10`) to Command `e61c0007`
//!    ("write to flash") to persist
//!
//! Endianness gotchas: advertising interval is little-endian; the Command
//! value is big-endian.
//!
//! Verified live on hardware against 5 tags (2 white EN12830, 3 black standard)
//! via the FIBER's BLE radio. Two model variants exist and their ATT handle
//! layouts differ, but resolving characteristics by **UUID** (not fixed handle)
//! makes provisioning model-agnostic:
//!   * standard ("black") tags expose the full config set incl. Active Sensors
//!     (`e61c0021`);
//!   * white EN12830 tags have **no** `e61c0021` characteristic — their sensors
//!     are fixed (Temperature + Humidity) by the model, so the Active Sensors
//!     write is skipped when the characteristic is absent.

use std::collections::HashMap;
use std::time::Duration;

use bluer::gatt::remote::Characteristic;
use bluer::{Device, Uuid};

/// Default PIN shipped on every EYE device.
pub const DEFAULT_PIN: &[u8; 6] = b"123456";

// Characteristic UUIDs (see fiber-v2/application#4 / the BLE integration guide).
//
// Resolve by UUID, never by fixed handle — the ATT handle layout differs by model.
// Reference (value handles, from live Read-By-Type discovery on 2026-06):
//   characteristic   white EN12830   black standard
//   TX power (2a07)      0x0027          0x0021
//   protocol e61c0001    0x002b          0x0025
//   adv int  e61c0002    0x002d          0x0027
//   sensors  e61c0021    (absent)        0x0059
//   command  e61c0007    0x003b          0x0035
//   password e61c0008    0x0037          0x0031
const UUID_PASSWORD: &str = "e61c0008-7df2-4d4e-8e6d-c611745b92e9";
const UUID_PROTOCOL_TYPE: &str = "e61c0001-7df2-4d4e-8e6d-c611745b92e9";
const UUID_ACTIVE_SENSORS: &str = "e61c0021-7df2-4d4e-8e6d-c611745b92e9";
const UUID_ADV_INTERVAL: &str = "e61c0002-7df2-4d4e-8e6d-c611745b92e9";
const UUID_COMMAND: &str = "e61c0007-7df2-4d4e-8e6d-c611745b92e9";
const UUID_TX_POWER: &str = "00002a07-0000-1000-8000-00805f9b34fb";

/// Command opcode written (big-endian) to [`UUID_COMMAND`] to persist params.
const CMD_WRITE_TO_FLASH: u16 = 0x0010;

/// Protocol type value that makes the tag broadcast the EYE Sensor payload.
pub const PROTOCOL_EYE_SENSOR: u8 = 0x02;

/// `active_sensors` bitmask values (characteristic `e61c0021`).
pub mod sensors {
    pub const TEMPERATURE: u8 = 0b0000_0001;
    pub const HUMIDITY: u8 = 0b0000_0010;
    pub const MAGNET: u8 = 0b0000_0100;
    pub const MOVEMENT: u8 = 0b0000_1000;
}

/// Active sensors are **fixed to Temperature + Humidity** for Proximos — this is
/// not a per-tag configurable option. White EN12830 tags don't expose the
/// `e61c0021` characteristic at all (their sensor set is fixed by the model);
/// on standard tags this value is written so they match.
pub const ACTIVE_SENSORS: u8 = sensors::TEMPERATURE | sensors::HUMIDITY;

/// The configuration profile written to a tag during provisioning.
#[derive(Debug, Clone, PartialEq)]
pub struct EyeProfile {
    /// Protocol type (`0x02` = EYE Sensor / "Sensors").
    pub protocol_type: u8,
    /// Advertising interval in milliseconds (1000..=10000).
    pub advertising_interval_ms: u16,
    /// Tx power in dBm. Allowed: -14,-11,-8,-5,-2,2,4,8.
    pub tx_power_dbm: i8,
}

impl Default for EyeProfile {
    /// PROXIMOS default: EYE Sensor, 10 s, +8 dBm (sensors fixed to Temp+Hum,
    /// see [`ACTIVE_SENSORS`]). +8 dBm is the tag's maximum TX power — chosen for
    /// best BLE range to the FIBER gateway (tags ship at the +2 dBm factory default).
    fn default() -> Self {
        Self {
            protocol_type: PROTOCOL_EYE_SENSOR,
            advertising_interval_ms: 10_000,
            tx_power_dbm: 8,
        }
    }
}

/// Errors that can occur while provisioning a tag.
#[derive(Debug)]
pub enum ProvisionError {
    /// Could not connect to the device.
    Connect(bluer::Error),
    /// GATT service discovery failed.
    Discovery(bluer::Error),
    /// A required characteristic was not found on the device.
    MissingCharacteristic(&'static str),
    /// A GATT write/read failed (commonly `NotPermitted` = wrong PIN / lockout).
    Gatt(bluer::Error),
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisionError::Connect(e) => write!(f, "connect failed: {e}"),
            ProvisionError::Discovery(e) => write!(f, "service discovery failed: {e}"),
            ProvisionError::MissingCharacteristic(u) => {
                write!(f, "characteristic {u} not found (wrong device?)")
            }
            ProvisionError::Gatt(e) => write!(
                f,
                "GATT write/read failed: {e} (wrong PIN or anti-bruteforce lockout?)"
            ),
        }
    }
}

impl std::error::Error for ProvisionError {}

/// Connect to `device`, unlock with the default PIN, apply `profile`, and
/// persist to flash. Leaves the device connected on success — the caller
/// decides when to disconnect.
pub async fn provision(device: &Device, profile: &EyeProfile) -> Result<(), ProvisionError> {
    if !device.is_connected().await.map_err(ProvisionError::Connect)? {
        device.connect().await.map_err(ProvisionError::Connect)?;
    }

    let chars = collect_characteristics(device).await?;
    let get = |uuid: &'static str| -> Result<&Characteristic, ProvisionError> {
        let u = Uuid::parse_str(uuid).expect("static UUID literal");
        chars
            .get(&u)
            .ok_or(ProvisionError::MissingCharacteristic(uuid))
    };

    // 1. Unlock with the default PIN (6 ASCII bytes, write-with-response).
    write(get(UUID_PASSWORD)?, DEFAULT_PIN).await?;

    // 2. Profile.
    write(get(UUID_TX_POWER)?, &[profile.tx_power_dbm as u8]).await?;
    write(
        get(UUID_ADV_INTERVAL)?,
        &profile.advertising_interval_ms.to_le_bytes(), // little-endian
    )
    .await?;
    write(get(UUID_PROTOCOL_TYPE)?, &[profile.protocol_type]).await?;
    // Active Sensors is fixed to Temperature + Humidity ([`ACTIVE_SENSORS`]).
    // The write is conditional only because white EN12830 tags don't expose
    // `e61c0021` (their sensor set is already fixed to Temp+Hum by the model) —
    // skip it when absent rather than failing the whole provisioning.
    if let Some(ch) = chars.get(&Uuid::parse_str(UUID_ACTIVE_SENSORS).expect("static UUID")) {
        write(ch, &[ACTIVE_SENSORS]).await?;
    }

    // 3. Persist (command 0x0010, big-endian bytes 00 10).
    write(get(UUID_COMMAND)?, &CMD_WRITE_TO_FLASH.to_be_bytes()).await?;

    Ok(())
}

/// Flatten every characteristic of every service into a `Uuid → Characteristic` map.
async fn collect_characteristics(
    device: &Device,
) -> Result<HashMap<Uuid, Characteristic>, ProvisionError> {
    let mut map = HashMap::new();
    let services = device.services().await.map_err(ProvisionError::Discovery)?;
    for service in services {
        let chars = service
            .characteristics()
            .await
            .map_err(ProvisionError::Discovery)?;
        for ch in chars {
            let uuid = ch.uuid().await.map_err(ProvisionError::Discovery)?;
            map.insert(uuid, ch);
        }
    }
    Ok(map)
}

async fn write(ch: &Characteristic, value: &[u8]) -> Result<(), ProvisionError> {
    ch.write(value).await.map_err(ProvisionError::Gatt)
}

/// How long to wait for GATT service resolution before giving up on a connect.
pub const SERVICE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_proximos() {
        let p = EyeProfile::default();
        assert_eq!(p.protocol_type, PROTOCOL_EYE_SENSOR);
        assert_eq!(p.advertising_interval_ms, 10_000);
        assert_eq!(p.tx_power_dbm, 8); // max TX power for best range
    }

    #[test]
    fn active_sensors_fixed_to_temp_hum() {
        assert_eq!(ACTIVE_SENSORS, 0b0000_0011); // temperature + humidity, fixed
    }

    #[test]
    fn adv_interval_is_little_endian() {
        // 10000 ms = 0x2710 → LE bytes 10 27
        assert_eq!(10_000u16.to_le_bytes(), [0x10, 0x27]);
    }

    #[test]
    fn flash_command_is_big_endian() {
        // 0x0010 → BE bytes 00 10
        assert_eq!(CMD_WRITE_TO_FLASH.to_be_bytes(), [0x00, 0x10]);
    }

    #[test]
    fn negative_tx_power_byte() {
        let p = EyeProfile {
            tx_power_dbm: -8,
            ..Default::default()
        };
        assert_eq!(p.tx_power_dbm as u8, 0xF8);
    }
}
