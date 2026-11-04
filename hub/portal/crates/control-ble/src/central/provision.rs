//! BTSMP1 provisioning over GATT — `plan/control/ble/beacon-btsmp1.md` §2,
//! `plan/control/ble/central.md` §5.
//!
//! **Three facts from §2 that will bite an implementation that does not know them**, all enforced
//! here rather than trusted:
//!
//! 1. **Resolve by UUID, never by handle.** The same characteristic sits at different ATT handles on
//!    the recorder and standard variants. A handle-indexed write works on the beacons it was
//!    developed against and *silently writes the wrong attribute* on the others — no error, wrong
//!    result. So the seam below hands over a map keyed by UUID and there is nowhere to put a handle.
//! 2. **The active-sensor characteristic is absent on recorder beacons.** Skipping it is the
//!    expected case, not an error case: a sequence that treats its absence as a failure cannot
//!    provision a recorder beacon at all.
//! 3. **Endianness is not uniform across characteristics.** The advertising interval is
//!    little-endian; the command value is big-endian. Getting one backwards produces a beacon
//!    advertising on a wrong interval, which looks like a range or battery problem rather than a
//!    configuration bug — so both are spelled out and asserted byte-for-byte.
//!
//! No pairing, no bonding, no encryption (§2.5): a plain application-level password write unlocks
//! the configuration characteristics on an ordinary LE connection.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bluer::Uuid;

use crate::config::{BeaconProfile, DialBudget, Sensor};
use crate::error::{Component, Error, Kind, Result, Role};
use crate::mac::MacAddr;

use super::ledger::ProvisionLedger;

// ---------------------------------------------------------------------------------------------
// The pinned bytes
// ---------------------------------------------------------------------------------------------

/// Configuration service, base `e61c00xx-7df2-4d4e-8e6d-c611745b92e9`.
pub const UUID_PROTOCOL_TYPE: Uuid = Uuid::from_u128(0xe61c0001_7df2_4d4e_8e6d_c611745b92e9);
pub const UUID_ADV_INTERVAL: Uuid = Uuid::from_u128(0xe61c0002_7df2_4d4e_8e6d_c611745b92e9);
pub const UUID_COMMAND: Uuid = Uuid::from_u128(0xe61c0007_7df2_4d4e_8e6d_c611745b92e9);
pub const UUID_PASSWORD: Uuid = Uuid::from_u128(0xe61c0008_7df2_4d4e_8e6d_c611745b92e9);
pub const UUID_ACTIVE_SENSORS: Uuid = Uuid::from_u128(0xe61c0021_7df2_4d4e_8e6d_c611745b92e9);
/// The BT SIG Tx Power Level characteristic, in the Tx Power service `0x1804`.
pub const UUID_TX_POWER: Uuid = Uuid::from_u128(0x00002a07_0000_1000_8000_00805f9b34fb);

/// `0x02` = "EYE Sensor" — the only value this component writes, and deliberately not a config key:
/// the decoder registry parses only the Teltonika-native frame, so any other value would silently
/// break decode. Hardware-verified, resolved 2026-08-25 (plan.md open question 8).
pub const PROTOCOL_EYE_SENSOR: u8 = 0x02;

/// What a BTSMP1 broadcasts, by its `e61c0001` protocol-type byte (Teltonika wiki, "EYE SENSOR /
/// BTSMP1"; 3 and 4 need firmware 01.02.10). Only [`BeaconProtocol::EyeSensor`] carries the readings
/// the decoder reads. Named on the wire beside the raw byte (central.md §5, amended 2026-09-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconProtocol {
    Ibeacon,
    Eddystone,
    EyeSensor,
    IbeaconEyeSensor,
    EddystoneEyeSensor,
    /// A byte outside the table; the raw `protocol_type` beside it says which.
    Unknown,
}

impl BeaconProtocol {
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Ibeacon,
            1 => Self::Eddystone,
            PROTOCOL_EYE_SENSOR => Self::EyeSensor,
            3 => Self::IbeaconEyeSensor,
            4 => Self::EddystoneEyeSensor,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ibeacon => "ibeacon",
            Self::Eddystone => "eddystone",
            Self::EyeSensor => "eye_sensor",
            Self::IbeaconEyeSensor => "ibeacon_eye_sensor",
            Self::EddystoneEyeSensor => "eddystone_eye_sensor",
            Self::Unknown => "unknown",
        }
    }
}

/// Flash-persist opcode, written **big-endian** to the command characteristic.
pub const CMD_WRITE_TO_FLASH: u16 = 0x0010;

/// The vendor default. A constant, not a secret: it is published in the vendor's own documentation,
/// and the policy question of changing it per beacon is plan.md open question 4.
pub const DEFAULT_PASSWORD: &[u8; 6] = b"123456";

pub const SERVICE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);

/// The advertising interval's valid range on the wire, in milliseconds.
pub const ADV_INTERVAL_MS_RANGE: std::ops::RangeInclusive<u16> = 1000..=10_000;

// ---------------------------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------------------------

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Why a write did not land. Kept as its own type so the **classification** lives here and is
/// testable, rather than in whichever transport happened to produce the failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GattWriteError {
    /// The beacon refused the value. On the password characteristic that is a wrong password.
    NotPermitted,
    /// The write did not complete within the dial budget.
    Timeout,
    /// Anything else on the link.
    Transport(String),
}

/// Every characteristic on a device, keyed by UUID — the shape §2 fact 1 forces: there is nowhere
/// in this map to put a handle, so a handle-indexed write cannot be written by accident.
pub type CharMap = BTreeMap<Uuid, Box<dyn GattChar>>;

/// One writable characteristic.
pub trait GattChar: Send + Sync {
    fn write(&self, value: Vec<u8>) -> BoxFuture<'_, std::result::Result<(), GattWriteError>>;
    /// The value the beacon holds now. Only read after the unlock: a locked configuration
    /// characteristic answers `NotPermitted` to a read as well as to a write.
    fn read(&self) -> BoxFuture<'_, std::result::Result<Vec<u8>, GattWriteError>>;
}

/// A connected beacon. The seam that lets the whole sequence be tested without BlueZ — which
/// matters because the sequence *is* the hardware-verified knowledge.
pub trait GattPeer: Send + Sync {
    fn connect(&self) -> BoxFuture<'_, std::result::Result<(), GattWriteError>>;
    /// Every characteristic on the device, keyed by UUID. Discovery is over the **whole** attribute
    /// database: the firmware's own service grouping is not reliable across variants.
    fn characteristics(&self) -> BoxFuture<'_, std::result::Result<CharMap, GattWriteError>>;
    fn disconnect(&self) -> BoxFuture<'_, ()>;
}

// ---------------------------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------------------------

/// What to write. Every field optional, because `beacon-send config` writes a **subset** and
/// leaves the rest of the beacon untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Profile {
    pub advertising_interval_ms: Option<u16>,
    pub tx_power_dbm: Option<i8>,
    pub active_sensors: Option<u8>,
}

impl Profile {
    /// The fleet default from `ble.central.beacon_profile`. The config layer is in **seconds** for a
    /// human editing YAML; the characteristic is milliseconds, so this is where the ×1000 happens.
    pub fn from_config(p: &BeaconProfile) -> Profile {
        Profile {
            advertising_interval_ms: Some(p.advertising_interval_s * 1000),
            tx_power_dbm: Some(p.tx_power_dbm),
            active_sensors: Some(Sensor::mask(&p.active_sensors)),
        }
    }

    /// An ad hoc partial override.
    pub fn partial(adv_s: Option<u16>, tx: Option<i8>, sensors: Option<&[Sensor]>) -> Profile {
        Profile {
            advertising_interval_ms: adv_s.map(|s| s * 1000),
            tx_power_dbm: tx,
            active_sensors: sensors.map(Sensor::mask),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.advertising_interval_ms.is_none()
            && self.tx_power_dbm.is_none()
            && self.active_sensors.is_none()
    }
}

/// What a provisioning session actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Written {
    /// Field names in **config vocabulary**, so a result reads like the YAML an operator edits.
    pub written: Vec<&'static str>,
    /// Fields deliberately not written, each with the reason. A skip is never a failure.
    pub skipped: Vec<String>,
    /// The configuration the beacon reported back after the flash commit — every field, not only
    /// the written ones. `Some` on every successful session: a session whose read-back does not
    /// match what it wrote is a failure, not a success with a caveat.
    pub read_back: Option<BleConfig>,
}

/// A beacon's configuration as it **reads back** over GATT, in wire units.
///
/// A Write Response says the beacon *permitted* a write, not that it now stores the value: an
/// endianness slip, a value the firmware clamps, or a write that landed on the wrong
/// characteristic all come back as success. Reading after the flash commit is what turns
/// "provisioned" into "configured as intended".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BleConfig {
    pub advertising_interval_ms: u16,
    pub tx_power_dbm: i8,
    pub protocol_type: u8,
    /// `None` on a recorder beacon, which has no active-sensor characteristic (§2 fact 2).
    pub active_sensors: Option<u8>,
}

fn gatt_err(kind: Kind, message: impl Into<String>) -> Error {
    Error::new(kind, Role::Central, Component::BeaconGatt, message)
}

/// Map a write failure to the shared error table. This is the classification central.md §6 asks to
/// be traced rather than assumed, applied at the one place every write goes through.
fn classify(e: GattWriteError, what: &str) -> Error {
    match e {
        GattWriteError::NotPermitted => gatt_err(
            Kind::PairingRejected,
            format!("the beacon refused the {what} write: wrong configuration password?"),
        ),
        GattWriteError::Timeout => gatt_err(
            Kind::BeaconUnreachable,
            format!("the beacon did not answer the {what} write within the dial budget"),
        ),
        GattWriteError::Transport(why) => {
            gatt_err(Kind::Transport, format!("{what} write failed: {why}"))
        }
    }
}

fn missing(uuid: Uuid, what: &str) -> Error {
    gatt_err(
        Kind::Transport,
        format!("characteristic {uuid} ({what}) not found (wrong device?)"),
    )
}

/// [`classify`] for the read-back. A refusal here comes after an accepted unlock, so it is not a
/// wrong password — it is a beacon that re-locked, or a characteristic it will not let us read.
fn classify_read(e: GattWriteError, what: &str) -> Error {
    match e {
        GattWriteError::NotPermitted => gatt_err(
            Kind::Transport,
            format!("the beacon refused to read back the {what} after the unlock"),
        ),
        GattWriteError::Timeout => gatt_err(
            Kind::BeaconUnreachable,
            format!("the beacon did not answer the {what} read-back within the dial budget"),
        ),
        GattWriteError::Transport(why) => {
            gatt_err(Kind::Transport, format!("{what} read-back failed: {why}"))
        }
    }
}

async fn read_value<const N: usize>(chars: &CharMap, uuid: Uuid, what: &str) -> Result<[u8; N]> {
    let c = chars.get(&uuid).ok_or_else(|| missing(uuid, what))?;
    let value = c.read().await.map_err(|e| classify_read(e, what))?;
    value.as_slice().try_into().map_err(|_| {
        gatt_err(
            Kind::Transport,
            format!(
                "the {what} read back as {} byte(s), expected {N}",
                value.len()
            ),
        )
    })
}

/// Read every configuration characteristic back, in the same units and byte orders it is written
/// in — the advertising interval little-endian, exactly as §2 fact 3 pins it.
async fn read_back(chars: &CharMap) -> Result<BleConfig> {
    let [tx] = read_value::<1>(chars, UUID_TX_POWER, "Tx power").await?;
    let adv = read_value::<2>(chars, UUID_ADV_INTERVAL, "advertising interval").await?;
    let [protocol] = read_value::<1>(chars, UUID_PROTOCOL_TYPE, "protocol type").await?;
    let active_sensors = if chars.contains_key(&UUID_ACTIVE_SENSORS) {
        let [mask] = read_value::<1>(chars, UUID_ACTIVE_SENSORS, "active sensors").await?;
        Some(mask)
    } else {
        None
    };
    Ok(BleConfig {
        advertising_interval_ms: u16::from_le_bytes(adv),
        tx_power_dbm: tx as i8,
        protocol_type: protocol,
        active_sensors,
    })
}

/// Does a beacon already hold the fleet profile? The same judgement a full provisioning session
/// makes of its own read-back, applied to any read — so a beacon enrolled already configured, by
/// another Hub or the vendor app, counts as configured without this Hub rewriting it.
pub fn holds_profile(reported: &BleConfig, profile: &BeaconProfile) -> bool {
    verify(reported, &Profile::from_config(profile), true).is_ok()
}

/// Judge the read-back against what this session wrote — and only that: a partial override leaves
/// the rest of the beacon alone, so its other values are reported, never second-guessed.
fn verify(reported: &BleConfig, profile: &Profile, full: bool) -> Result<()> {
    let mut mismatches = Vec::new();
    if let Some(dbm) = profile.tx_power_dbm {
        if reported.tx_power_dbm != dbm {
            mismatches.push(format!(
                "Tx power wrote {dbm} dBm, read {} dBm",
                reported.tx_power_dbm
            ));
        }
    }
    if let Some(ms) = profile.advertising_interval_ms {
        if reported.advertising_interval_ms != ms {
            mismatches.push(format!(
                "advertising interval wrote {ms} ms, read {} ms",
                reported.advertising_interval_ms
            ));
        }
    }
    if full && reported.protocol_type != PROTOCOL_EYE_SENSOR {
        mismatches.push(format!(
            "protocol type wrote {PROTOCOL_EYE_SENSOR:#04x}, read {:#04x}",
            reported.protocol_type
        ));
    }
    if let (Some(mask), Some(read)) = (profile.active_sensors, reported.active_sensors) {
        if read != mask {
            mismatches.push(format!(
                "active sensors wrote {mask:#04x}, read {read:#04x}"
            ));
        }
    }
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(gatt_err(
            Kind::Transport,
            format!(
                "read-back mismatch after the flash commit: {}",
                mismatches.join("; ")
            ),
        ))
    }
}

/// Connect, resolve the attribute database and write the password. Every session starts here —
/// the configuration characteristics refuse a read as well as a write until it has been done.
async fn unlock(peer: &dyn GattPeer) -> Result<CharMap> {
    peer.connect().await.map_err(|e| classify(e, "connect"))?;
    let chars = peer
        .characteristics()
        .await
        .map_err(|e| classify(e, "service discovery"))?;
    let password = chars
        .get(&UUID_PASSWORD)
        .ok_or_else(|| missing(UUID_PASSWORD, "configuration password"))?;
    password
        .write(DEFAULT_PASSWORD.to_vec())
        .await
        .map_err(|e| classify(e, "password"))?;
    Ok(chars)
}

/// Read a beacon's configuration and change none of it — what follows an enrollment unless
/// configuring on enroll is switched on. The unlock is the only write, and the beacon stores
/// nothing from it.
pub async fn read_config(peer: &dyn GattPeer) -> Result<BleConfig> {
    let chars = unlock(peer).await?;
    let reported = read_back(&chars).await?;
    peer.disconnect().await;
    Ok(reported)
}

/// Write a profile to a connected beacon.
///
/// `full` means "this is provisioning": the protocol type is written too. A partial override
/// (`beacon-send config`) passes `false` and **never rewrites the protocol type** — that value
/// decides whether we can decode the beacon's advertisements at all, so an ad hoc command has no
/// business touching it.
///
/// The write order is the hardware-verified one from §2: password → Tx power → advertising interval
/// → protocol type → active sensors (if present) → flash commit. Then every configuration
/// characteristic is read back and checked against what was written; a mismatch fails the session.
pub async fn provision(peer: &dyn GattPeer, profile: &Profile, full: bool) -> Result<Written> {
    // 1. Unlock. Everything after this depends on it, so a refusal here is the whole session.
    let chars = unlock(peer).await?;

    let mut out = Written::default();

    // 2. Tx power — a single signed byte, so a negative value is its two's-complement encoding.
    if let Some(dbm) = profile.tx_power_dbm {
        let c = chars
            .get(&UUID_TX_POWER)
            .ok_or_else(|| missing(UUID_TX_POWER, "Tx power"))?;
        c.write(vec![dbm as u8])
            .await
            .map_err(|e| classify(e, "Tx power"))?;
        out.written.push("tx_power_dbm");
    }

    // 3. Advertising interval — u16 LITTLE-endian milliseconds.
    if let Some(ms) = profile.advertising_interval_ms {
        if !ADV_INTERVAL_MS_RANGE.contains(&ms) {
            return Err(Error::new(
                Kind::InvalidArgument,
                Role::Central,
                Component::BeaconGatt,
                format!(
                    "advertising interval {ms} ms is outside the characteristic's valid range \
                     {}..={} ms",
                    ADV_INTERVAL_MS_RANGE.start(),
                    ADV_INTERVAL_MS_RANGE.end()
                ),
            ));
        }
        let c = chars
            .get(&UUID_ADV_INTERVAL)
            .ok_or_else(|| missing(UUID_ADV_INTERVAL, "advertising interval"))?;
        c.write(ms.to_le_bytes().to_vec())
            .await
            .map_err(|e| classify(e, "advertising interval"))?;
        out.written.push("advertising_interval_s");
    }

    // 4. Protocol type — provisioning only.
    if full {
        let c = chars
            .get(&UUID_PROTOCOL_TYPE)
            .ok_or_else(|| missing(UUID_PROTOCOL_TYPE, "protocol type"))?;
        c.write(vec![PROTOCOL_EYE_SENSOR])
            .await
            .map_err(|e| classify(e, "protocol type"))?;
        out.written.push("protocol_type");
    }

    // 5. Active sensors — **absent on recorder beacons, and that is expected.**
    if let Some(mask) = profile.active_sensors {
        match chars.get(&UUID_ACTIVE_SENSORS) {
            Some(c) => {
                c.write(vec![mask])
                    .await
                    .map_err(|e| classify(e, "active sensors"))?;
                out.written.push("active_sensors");
            }
            None => out.skipped.push(
                "active_sensors: recorder beacon has no active-sensor characteristic".to_string(),
            ),
        }
    }

    // 6. Flash commit — u16 BIG-endian, so the profile survives the beacon's next power cycle.
    let c = chars
        .get(&UUID_COMMAND)
        .ok_or_else(|| missing(UUID_COMMAND, "command"))?;
    c.write(CMD_WRITE_TO_FLASH.to_be_bytes().to_vec())
        .await
        .map_err(|e| classify(e, "flash commit"))?;

    // 7. Read back, still inside the unlocked session, and hold the beacon to what was written.
    let reported = read_back(&chars).await?;
    verify(&reported, profile, full)?;
    out.read_back = Some(reported);

    peer.disconnect().await;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The allowlist gate
// ---------------------------------------------------------------------------------------------

/// May this beacon be auto-provisioned?
///
/// **Pure**, and the negative answer is the important one. With auto-provisioning on and no gating, a
/// Hub attempts default-password unlocks against *every* Teltonika beacon it can hear — including a
/// neighbouring company's fleet in the next unit. So:
///
/// - a beacon qualifies only if it is enrolled here, or on the fleet allowlist;
/// - **an empty allowlist is not a licence to provision the neighbourhood** — it narrows to
///   enrolled-only rather than widening to everything;
/// - and a beacon that has spent its budget is left alone until an operator acts.
pub fn may_auto_provision(
    mac: &MacAddr,
    enrolled: &BTreeSet<MacAddr>,
    known: &BTreeSet<MacAddr>,
    ledger: &ProvisionLedger,
    cfg: &DialBudget,
) -> bool {
    let mine = enrolled.contains(mac) || known.contains(mac);
    mine && !ledger.exhausted(mac, cfg.per_beacon_attempts)
}

// ---------------------------------------------------------------------------------------------
// The BlueZ implementation of the seam
// ---------------------------------------------------------------------------------------------

/// [`GattPeer`] over a real `bluer::Device`.
///
/// The caller must already hold the shared adapter slot: this dials, and a dial that races a phone
/// connection or another beacon session is exactly what §0.1 exists to prevent.
pub struct BluerPeer {
    device: bluer::Device,
}

impl BluerPeer {
    pub fn new(device: bluer::Device) -> BluerPeer {
        BluerPeer { device }
    }
}

/// Map a BlueZ error onto the classification this module owns.
fn from_bluer(e: &bluer::Error) -> GattWriteError {
    let text = e.to_string();
    let lower = text.to_lowercase();
    if lower.contains("not permitted")
        || lower.contains("not authorized")
        || lower.contains("notpermitted")
    {
        GattWriteError::NotPermitted
    } else if lower.contains("timed out") || lower.contains("timeout") {
        GattWriteError::Timeout
    } else {
        GattWriteError::Transport(text)
    }
}

/// One remote characteristic.
struct BluerChar {
    inner: bluer::gatt::remote::Characteristic,
}

impl GattChar for BluerChar {
    fn write(&self, value: Vec<u8>) -> BoxFuture<'_, std::result::Result<(), GattWriteError>> {
        Box::pin(async move {
            // Write **with response**: §2 says so for the password, and a silent
            // write-without-response would make a refusal indistinguishable from success.
            self.inner
                .write_ext(
                    &value,
                    &bluer::gatt::remote::CharacteristicWriteRequest {
                        op_type: bluer::gatt::WriteOp::Request,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| from_bluer(&e))
        })
    }

    fn read(&self) -> BoxFuture<'_, std::result::Result<Vec<u8>, GattWriteError>> {
        Box::pin(async move { self.inner.read().await.map_err(|e| from_bluer(&e)) })
    }
}

impl GattPeer for BluerPeer {
    fn connect(&self) -> BoxFuture<'_, std::result::Result<(), GattWriteError>> {
        Box::pin(async move {
            if self.device.is_connected().await.unwrap_or(false) {
                return Ok(());
            }
            self.device.connect().await.map_err(|e| from_bluer(&e))
        })
    }

    fn characteristics(&self) -> BoxFuture<'_, std::result::Result<CharMap, GattWriteError>> {
        Box::pin(async move {
            // Wait for the whole database, then walk **every** service: §2 fact 1 — the same
            // characteristic sits at different handles per variant, and the firmware's own service
            // grouping is not reliable either, so nothing here may assume a layout.
            let deadline = std::time::Instant::now() + SERVICE_RESOLVE_TIMEOUT;
            loop {
                if self.device.is_services_resolved().await.unwrap_or(false) {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(GattWriteError::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            let mut map: CharMap = BTreeMap::new();
            let services = self.device.services().await.map_err(|e| from_bluer(&e))?;
            for service in services {
                let chars = service
                    .characteristics()
                    .await
                    .map_err(|e| from_bluer(&e))?;
                for c in chars {
                    if let Ok(uuid) = c.uuid().await {
                        map.insert(uuid, Box::new(BluerChar { inner: c }) as Box<dyn GattChar>);
                    }
                }
            }
            Ok(map)
        })
    }

    fn disconnect(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.device.disconnect().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const A: &str = "AA:BB:CC:DD:EE:FF";
    const B: &str = "00:11:22:33:44:55";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).expect("mac")
    }
    fn set(macs: &[&str]) -> BTreeSet<MacAddr> {
        macs.iter().map(|m| mac(m)).collect()
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every write, in order: which characteristic and what bytes.
    type WriteLog = std::sync::Arc<Mutex<Vec<(Uuid, Vec<u8>)>>>;
    /// Every write AND read, in order — `'w'` or `'r'` and the characteristic.
    type OpLog = std::sync::Arc<Mutex<Vec<(char, Uuid)>>>;

    /// A beacon that records every write, in order, and can be told to behave like each variant.
    #[derive(Default)]
    struct FakePeer {
        writes: WriteLog,
        ops: OpLog,
        /// Simulates a recorder beacon: no active-sensor characteristic at all.
        recorder: bool,
        reject_password: bool,
        omit: Option<Uuid>,
        /// A characteristic that reads back this value whatever was written — a beacon that does
        /// not store what it acknowledged.
        corrupt: Option<(Uuid, Vec<u8>)>,
        /// A characteristic that refuses the read-back.
        unreadable: Option<Uuid>,
        disconnected: std::sync::Arc<Mutex<bool>>,
    }

    /// What an untouched beacon holds before this session writes anything: 4 dBm, 1 000 ms,
    /// EYE Sensor, temperature only. Deliberately unlike the fleet default, so a read-back that
    /// returned the old value instead of the written one could not pass by accident.
    fn factory_value(uuid: Uuid) -> Option<Vec<u8>> {
        match uuid {
            u if u == UUID_TX_POWER => Some(vec![0x04]),
            u if u == UUID_ADV_INTERVAL => Some(vec![0xE8, 0x03]),
            u if u == UUID_PROTOCOL_TYPE => Some(vec![0x02]),
            u if u == UUID_ACTIVE_SENSORS => Some(vec![0x01]),
            _ => None,
        }
    }

    struct FakeChar {
        uuid: Uuid,
        writes: WriteLog,
        ops: OpLog,
        reject: bool,
        corrupt: Option<Vec<u8>>,
        unreadable: bool,
    }

    impl GattChar for FakeChar {
        fn write(&self, value: Vec<u8>) -> BoxFuture<'_, std::result::Result<(), GattWriteError>> {
            let uuid = self.uuid;
            let writes = self.writes.clone();
            let ops = self.ops.clone();
            let reject = self.reject;
            Box::pin(async move {
                if reject {
                    return Err(GattWriteError::NotPermitted);
                }
                lock(&ops).push(('w', uuid));
                lock(&writes).push((uuid, value));
                Ok(())
            })
        }

        fn read(&self) -> BoxFuture<'_, std::result::Result<Vec<u8>, GattWriteError>> {
            let uuid = self.uuid;
            let writes = self.writes.clone();
            let ops = self.ops.clone();
            let corrupt = self.corrupt.clone();
            let unreadable = self.unreadable;
            Box::pin(async move {
                lock(&ops).push(('r', uuid));
                if unreadable {
                    return Err(GattWriteError::NotPermitted);
                }
                if let Some(value) = corrupt {
                    return Ok(value);
                }
                let last = lock(&writes)
                    .iter()
                    .rev()
                    .find(|(u, _)| *u == uuid)
                    .map(|(_, v)| v.clone());
                last.or_else(|| factory_value(uuid))
                    .ok_or_else(|| GattWriteError::Transport("no value to read".into()))
            })
        }
    }

    impl GattPeer for FakePeer {
        fn connect(&self) -> BoxFuture<'_, std::result::Result<(), GattWriteError>> {
            Box::pin(async { Ok(()) })
        }
        fn characteristics(&self) -> BoxFuture<'_, std::result::Result<CharMap, GattWriteError>> {
            let mut present = vec![
                UUID_PASSWORD,
                UUID_TX_POWER,
                UUID_ADV_INTERVAL,
                UUID_PROTOCOL_TYPE,
                UUID_COMMAND,
            ];
            if !self.recorder {
                present.push(UUID_ACTIVE_SENSORS);
            }
            if let Some(skip) = self.omit {
                present.retain(|u| *u != skip);
            }
            let writes = self.writes.clone();
            let ops = self.ops.clone();
            let reject_password = self.reject_password;
            let corrupt = self.corrupt.clone();
            let unreadable = self.unreadable;
            Box::pin(async move {
                let mut map: CharMap = BTreeMap::new();
                for uuid in present {
                    map.insert(
                        uuid,
                        Box::new(FakeChar {
                            uuid,
                            writes: writes.clone(),
                            ops: ops.clone(),
                            reject: reject_password && uuid == UUID_PASSWORD,
                            corrupt: corrupt
                                .as_ref()
                                .filter(|(u, _)| *u == uuid)
                                .map(|(_, v)| v.clone()),
                            unreadable: unreadable == Some(uuid),
                        }) as Box<dyn GattChar>,
                    );
                }
                Ok(map)
            })
        }
        fn disconnect(&self) -> BoxFuture<'_, ()> {
            let flag = self.disconnected.clone();
            Box::pin(async move {
                *lock(&flag) = true;
            })
        }
    }

    fn default_profile() -> Profile {
        Profile::from_config(&BeaconProfile::default())
    }

    #[tokio::test]
    async fn the_full_sequence_writes_the_documented_bytes_in_order() {
        let peer = FakePeer::default();
        let out = provision(&peer, &default_profile(), true)
            .await
            .expect("provisions");

        let writes = lock(&peer.writes).clone();
        let values: Vec<Vec<u8>> = writes.iter().map(|(_, v)| v.clone()).collect();
        assert_eq!(
            values,
            vec![
                DEFAULT_PASSWORD.to_vec(), // unlock first — §2's verified order
                vec![0x08],                // Tx power +8 dBm, one signed byte
                vec![0x10, 0x27],          // 10 000 ms, u16 LITTLE-endian
                vec![0x02],                // protocol type = EYE Sensor
                vec![0x03],                // temperature | humidity
                vec![0x00, 0x10],          // flash commit, u16 BIG-endian
            ],
            "the hardware-verified order and byte layout of beacon-btsmp1.md §2"
        );
        let uuids: Vec<Uuid> = writes.iter().map(|(u, _)| *u).collect();
        assert_eq!(
            uuids,
            vec![
                UUID_PASSWORD,
                UUID_TX_POWER,
                UUID_ADV_INTERVAL,
                UUID_PROTOCOL_TYPE,
                UUID_ACTIVE_SENSORS,
                UUID_COMMAND
            ]
        );
        assert_eq!(
            out.written,
            vec![
                "tx_power_dbm",
                "advertising_interval_s",
                "protocol_type",
                "active_sensors"
            ]
        );
        assert!(out.skipped.is_empty());
        assert!(*lock(&peer.disconnected), "the link is released");
    }

    #[tokio::test]
    async fn the_two_endiannesses_are_not_the_same_and_neither_is_reversed() {
        // §2 fact 3: the advertising interval is little-endian and the command is big-endian.
        // Getting one backwards produces a beacon on a wrong interval, which looks like a range or
        // battery problem rather than a configuration bug.
        let peer = FakePeer::default();
        provision(&peer, &default_profile(), true)
            .await
            .expect("provisions");
        let writes = lock(&peer.writes).clone();
        let adv = writes
            .iter()
            .find(|(u, _)| *u == UUID_ADV_INTERVAL)
            .expect("interval written")
            .1
            .clone();
        let cmd = writes
            .iter()
            .find(|(u, _)| *u == UUID_COMMAND)
            .expect("command written")
            .1
            .clone();
        assert_eq!(adv, vec![0x10, 0x27], "10000 LE");
        assert_ne!(adv, vec![0x27, 0x10], "not big-endian");
        assert_eq!(cmd, vec![0x00, 0x10], "0x0010 BE");
        assert_ne!(cmd, vec![0x10, 0x00], "not little-endian");
    }

    #[tokio::test]
    async fn a_recorder_beacon_skips_active_sensors_and_reports_it() {
        let peer = FakePeer {
            recorder: true,
            ..FakePeer::default()
        };
        let out = provision(&peer, &default_profile(), true)
            .await
            .expect("provisioning a recorder must succeed");
        assert!(
            !out.written.contains(&"active_sensors"),
            "nothing was written for it"
        );
        assert_eq!(out.skipped.len(), 1);
        assert!(
            out.skipped[0].contains("recorder beacon has no active-sensor characteristic"),
            "{:?}",
            out.skipped
        );
        // And the rest of the profile still landed, including the flash commit.
        let uuids: Vec<Uuid> = lock(&peer.writes).iter().map(|(u, _)| *u).collect();
        assert!(uuids.contains(&UUID_COMMAND));
        assert!(!uuids.contains(&UUID_ACTIVE_SENSORS));
    }

    #[tokio::test]
    async fn a_rejected_password_is_pairing_rejected() {
        let peer = FakePeer {
            reject_password: true,
            ..FakePeer::default()
        };
        let err = provision(&peer, &default_profile(), true)
            .await
            .expect_err("must fail");
        assert_eq!(err.kind(), Kind::PairingRejected);
        assert_eq!(err.exit_code(), 15);
        assert_eq!(err.component(), Component::BeaconGatt);
        assert!(
            lock(&peer.writes).is_empty(),
            "nothing is written after a refused unlock"
        );
    }

    #[tokio::test]
    async fn a_missing_required_characteristic_is_transport_and_names_the_uuid() {
        for (omit, label) in [
            (UUID_PASSWORD, "password"),
            (UUID_COMMAND, "command"),
            (UUID_ADV_INTERVAL, "advertising interval"),
            (UUID_PROTOCOL_TYPE, "protocol type"),
            (UUID_TX_POWER, "Tx power"),
        ] {
            let peer = FakePeer {
                omit: Some(omit),
                ..FakePeer::default()
            };
            let err = provision(&peer, &default_profile(), true)
                .await
                .expect_err("a required characteristic is missing");
            assert_eq!(err.kind(), Kind::Transport, "{label}");
            assert_eq!(err.component(), Component::BeaconGatt, "{label}");
            assert!(
                err.message().contains(&omit.to_string()),
                "the message must name the UUID so a bench operator can grep for it: {}",
                err.message()
            );
            assert!(
                err.message().contains("wrong device?"),
                "and it must suggest the likely cause: {}",
                err.message()
            );
        }
    }

    #[tokio::test]
    async fn partial_override_never_rewrites_protocol_type() {
        let peer = FakePeer::default();
        let p = Profile::partial(None, Some(4), None);
        let out = provision(&peer, &p, false).await.expect("writes");
        assert_eq!(out.written, vec!["tx_power_dbm"]);
        let uuids: Vec<Uuid> = lock(&peer.writes).iter().map(|(u, _)| *u).collect();
        assert!(
            !uuids.contains(&UUID_PROTOCOL_TYPE),
            "an ad hoc command has no business touching the value that decides whether we can \
             decode this beacon at all"
        );
        assert!(!uuids.contains(&UUID_ADV_INTERVAL));
        assert!(!uuids.contains(&UUID_ACTIVE_SENSORS));
        // The password and the flash commit still bracket it.
        assert_eq!(uuids.first(), Some(&UUID_PASSWORD));
        assert_eq!(uuids.last(), Some(&UUID_COMMAND));
    }

    #[tokio::test]
    async fn negative_tx_power_is_the_two_s_complement_byte() {
        let peer = FakePeer::default();
        provision(&peer, &Profile::partial(None, Some(-8), None), false)
            .await
            .expect("writes");
        let v = lock(&peer.writes)
            .iter()
            .find(|(u, _)| *u == UUID_TX_POWER)
            .expect("written")
            .1
            .clone();
        assert_eq!(v, vec![0xF8], "-8 dBm is 0xF8, not 0x08 with a sign bit");
    }

    #[tokio::test]
    async fn an_interval_outside_the_characteristic_range_is_refused_before_the_write() {
        let peer = FakePeer::default();
        // 500 ms is below the wire minimum, so nothing should be written for it.
        let p = Profile {
            advertising_interval_ms: Some(500),
            ..Profile::default()
        };
        let err = provision(&peer, &p, false).await.expect_err("must refuse");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        let uuids: Vec<Uuid> = lock(&peer.writes).iter().map(|(u, _)| *u).collect();
        assert!(!uuids.contains(&UUID_ADV_INTERVAL));
    }

    #[tokio::test]
    async fn every_setting_is_read_back_after_the_flash_commit_and_reported() {
        let peer = FakePeer::default();
        let out = provision(&peer, &default_profile(), true)
            .await
            .expect("provisions");
        assert_eq!(
            out.read_back,
            Some(BleConfig {
                advertising_interval_ms: 10_000,
                tx_power_dbm: 8,
                protocol_type: PROTOCOL_EYE_SENSOR,
                active_sensors: Some(0x03),
            })
        );
        let ops = lock(&peer.ops).clone();
        let commit = ops
            .iter()
            .position(|op| *op == ('w', UUID_COMMAND))
            .expect("flash commit written");
        let reads: Vec<Uuid> = ops
            .iter()
            .filter(|(op, _)| *op == 'r')
            .map(|(_, u)| *u)
            .collect();
        assert_eq!(
            reads,
            vec![
                UUID_TX_POWER,
                UUID_ADV_INTERVAL,
                UUID_PROTOCOL_TYPE,
                UUID_ACTIVE_SENSORS
            ]
        );
        assert!(
            ops.iter()
                .enumerate()
                .all(|(i, (op, _))| *op != 'r' || i > commit),
            "the read-back checks what was committed, so it comes after the commit: {ops:?}"
        );
        assert!(*lock(&peer.disconnected), "the link is released");
    }

    #[tokio::test]
    async fn a_value_the_beacon_did_not_store_fails_the_session_and_names_both_sides() {
        // 5 000 ms read back where 10 000 ms was written and acknowledged.
        let peer = FakePeer {
            corrupt: Some((UUID_ADV_INTERVAL, vec![0x88, 0x13])),
            ..FakePeer::default()
        };
        let err = provision(&peer, &default_profile(), true)
            .await
            .expect_err("an unstored value is not a provisioned beacon");
        assert_eq!(err.kind(), Kind::Transport);
        assert_eq!(err.component(), Component::BeaconGatt);
        assert!(
            err.message()
                .contains("advertising interval wrote 10000 ms, read 5000 ms"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn a_protocol_type_read_back_wrong_fails_provisioning() {
        let peer = FakePeer {
            corrupt: Some((UUID_PROTOCOL_TYPE, vec![0x03])),
            ..FakePeer::default()
        };
        let err = provision(&peer, &default_profile(), true)
            .await
            .expect_err("a beacon we cannot decode is not provisioned");
        assert!(
            err.message()
                .contains("protocol type wrote 0x02, read 0x03"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn a_recorder_reads_back_without_active_sensors() {
        let peer = FakePeer {
            recorder: true,
            ..FakePeer::default()
        };
        let out = provision(&peer, &default_profile(), true)
            .await
            .expect("provisioning a recorder must succeed");
        let reported = out.read_back.expect("read back");
        assert_eq!(reported.active_sensors, None, "there is nothing to read");
        assert!(!lock(&peer.ops).contains(&('r', UUID_ACTIVE_SENSORS)));
    }

    #[tokio::test]
    async fn a_partial_override_reports_the_values_it_did_not_touch_without_judging_them() {
        // The beacon's protocol type reads back as something other than EYE Sensor, and a
        // partial override must neither rewrite nor judge it (partial_override_never_rewrites_…).
        let peer = FakePeer {
            corrupt: Some((UUID_PROTOCOL_TYPE, vec![0x03])),
            ..FakePeer::default()
        };
        let out = provision(&peer, &Profile::partial(None, Some(4), None), false)
            .await
            .expect("only the written field is judged");
        assert_eq!(
            out.read_back,
            Some(BleConfig {
                advertising_interval_ms: 1_000,
                tx_power_dbm: 4,
                protocol_type: 0x03,
                active_sensors: Some(0x01),
            }),
            "the untouched fields are what the beacon holds, reported as read"
        );
    }

    #[tokio::test]
    async fn a_characteristic_that_refuses_the_read_back_fails_the_session() {
        let peer = FakePeer {
            unreadable: Some(UUID_ADV_INTERVAL),
            ..FakePeer::default()
        };
        let err = provision(&peer, &default_profile(), true)
            .await
            .expect_err("an unverifiable session is not a verified one");
        assert_eq!(
            err.kind(),
            Kind::Transport,
            "not PairingRejected: the unlock was already accepted"
        );
        assert!(
            err.message()
                .contains("refused to read back the advertising interval"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn a_read_back_of_the_wrong_length_is_transport() {
        let peer = FakePeer {
            corrupt: Some((UUID_TX_POWER, vec![0x08, 0x00])),
            ..FakePeer::default()
        };
        let err = provision(&peer, &default_profile(), true)
            .await
            .expect_err("must fail");
        assert_eq!(err.kind(), Kind::Transport);
        assert!(
            err.message()
                .contains("Tx power read back as 2 byte(s), expected 1"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn reading_the_configuration_writes_nothing_but_the_unlock() {
        let peer = FakePeer::default();
        let reported = read_config(&peer).await.expect("reads");
        assert_eq!(
            reported,
            BleConfig {
                advertising_interval_ms: 1_000,
                tx_power_dbm: 4,
                protocol_type: PROTOCOL_EYE_SENSOR,
                active_sensors: Some(0x01),
            },
            "what the beacon holds, untouched"
        );
        let writes: Vec<Uuid> = lock(&peer.writes).iter().map(|(u, _)| *u).collect();
        assert_eq!(
            writes,
            vec![UUID_PASSWORD],
            "no profile write and no flash commit"
        );
        assert!(*lock(&peer.disconnected), "the link is released");
    }

    #[tokio::test]
    async fn a_read_with_a_refused_password_is_pairing_rejected_and_reads_nothing() {
        let peer = FakePeer {
            reject_password: true,
            ..FakePeer::default()
        };
        let err = read_config(&peer).await.expect_err("must fail");
        assert_eq!(err.kind(), Kind::PairingRejected);
        assert!(!lock(&peer.ops).iter().any(|(op, _)| *op == 'r'));
    }

    #[test]
    fn a_sensor_mask_names_itself_back_and_drops_unnamed_bits() {
        assert_eq!(
            Sensor::from_mask(0x03),
            vec![Sensor::Temperature, Sensor::Humidity]
        );
        assert_eq!(Sensor::from_mask(0x0F), Sensor::ALL.to_vec());
        assert_eq!(Sensor::from_mask(0x10), Vec::<Sensor>::new());
        for mask in 0u8..=0x0F {
            assert_eq!(Sensor::mask(&Sensor::from_mask(mask)), mask);
        }
    }

    #[test]
    fn the_config_layer_is_seconds_and_the_wire_is_milliseconds() {
        let p = Profile::from_config(&BeaconProfile::default());
        assert_eq!(p.advertising_interval_ms, Some(10_000), "10 s → 10 000 ms");
        assert_eq!(p.tx_power_dbm, Some(8));
        assert_eq!(p.active_sensors, Some(0x03));
        let partial = Profile::partial(Some(5), None, Some(&[Sensor::Movement]));
        assert_eq!(partial.advertising_interval_ms, Some(5_000));
        assert_eq!(partial.active_sensors, Some(0x08));
        assert_eq!(partial.tx_power_dbm, None);
        assert!(!partial.is_empty());
        assert!(Profile::default().is_empty());
    }

    #[test]
    fn a_stranger_off_the_allowlist_is_never_provisioned() {
        let ledger = ProvisionLedger::default();
        let cfg = DialBudget::default();
        let enrolled = set(&[A]);
        let known = set(&[B]);

        assert!(may_auto_provision(
            &mac(A),
            &enrolled,
            &known,
            &ledger,
            &cfg
        ));
        assert!(
            may_auto_provision(&mac(B), &enrolled, &known, &ledger, &cfg),
            "a fleet-allowed beacon is ours to provision"
        );
        assert!(
            !may_auto_provision(&mac("FF:FF:FF:FF:FF:FF"), &enrolled, &known, &ledger, &cfg),
            "the neighbour's fleet is never unlocked"
        );
    }

    #[test]
    fn an_empty_allowlist_narrows_to_enrolled_rather_than_widening_to_everything() {
        let ledger = ProvisionLedger::default();
        let cfg = DialBudget::default();
        let enrolled = set(&[A]);
        let no_allowlist = BTreeSet::new();
        assert!(may_auto_provision(
            &mac(A),
            &enrolled,
            &no_allowlist,
            &ledger,
            &cfg
        ));
        assert!(
            !may_auto_provision(&mac(B), &enrolled, &no_allowlist, &ledger, &cfg),
            "an empty allowlist is not a licence to provision the neighbourhood"
        );
    }

    #[test]
    fn an_exhausted_budget_stops_auto_provisioning_even_for_our_own_beacon() {
        let mut ledger = ProvisionLedger::default();
        let cfg = DialBudget::default();
        let enrolled = set(&[A]);
        let known = BTreeSet::new();
        for _ in 0..cfg.per_beacon_attempts {
            ledger.record_attempt(&mac(A), 1_000);
        }
        assert!(!may_auto_provision(
            &mac(A),
            &enrolled,
            &known,
            &ledger,
            &cfg
        ));
        // …and an operator action reopens it.
        ledger.reset(&mac(A));
        assert!(may_auto_provision(
            &mac(A),
            &enrolled,
            &known,
            &ledger,
            &cfg
        ));
    }

    #[test]
    fn the_pinned_uuids_are_the_documented_strings() {
        // Typing a UUID wrong is silent: the characteristic simply is not found. Spell them out.
        assert_eq!(
            UUID_PASSWORD.to_string(),
            "e61c0008-7df2-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_PROTOCOL_TYPE.to_string(),
            "e61c0001-7df2-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_ADV_INTERVAL.to_string(),
            "e61c0002-7df2-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_ACTIVE_SENSORS.to_string(),
            "e61c0021-7df2-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_COMMAND.to_string(),
            "e61c0007-7df2-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_TX_POWER.to_string(),
            "00002a07-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(PROTOCOL_EYE_SENSOR, 0x02);
        assert_eq!(CMD_WRITE_TO_FLASH, 0x0010);
        assert_eq!(DEFAULT_PASSWORD, b"123456");
    }
}
