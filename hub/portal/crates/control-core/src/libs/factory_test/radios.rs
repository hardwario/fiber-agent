//! The two LoRa receivers on a Hub nobody has set up yet. Before provisioning
//! no radio arm runs: the Northbridge sits in BOOT with its UART unowned, and
//! concentratord is condition-skipped until LoRaWAN is created. The factory
//! test therefore listens on both itself, for its own duration only.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use control_radio::config::P2pRadio;
use control_radio::p2p::modem::RadioConfig;
use control_radio::p2p::modem_io::{gateway_state, Modem, ModemInfo, SerialModem};
use control_radio::p2p::tower::link::{self, TwrCmd};
use serde::Serialize;

use crate::libs::display::supervise::lock_recover;

/// The longest a factory listener runs if nobody stops it.
const MAX_LISTEN: Duration = Duration::from_secs(30 * 60);
const POLL: Duration = Duration::from_secs(3);

/// The shipped P2P channel (`portal/packaging/templates/defaults.yaml`), so a
/// STICKER at the station is heard on the channel it will use in the field.
const FACTORY_P2P_RADIO: &str = "frequency_hz: 869525000\n\
                                 spreading_factor: 7\n\
                                 bandwidth_khz: 125\n\
                                 coding_rate: \"4/5\"\n\
                                 preamble: 8\n";

/// No real network uses it; with ACKs off the gateway answers nobody.
const FACTORY_NET_ID: u32 = 0;

/// What one listener saw.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Listen {
    /// `starting`, `listening`, `stopped` or `error`.
    pub state: String,
    /// Firmware or gateway id, for the report.
    pub ident: Option<String>,
    /// Frames heard since the listener started.
    pub heard: u64,
    pub error: Option<String>,
}

impl Listen {
    fn starting() -> Self {
        Listen {
            state: "starting".into(),
            ..Default::default()
        }
    }
}

pub type Shared = Arc<Mutex<Listen>>;

/// A stop flag both listeners poll.
#[derive(Debug, Clone, Default)]
pub struct Stop(Arc<std::sync::atomic::AtomicBool>);

impl Stop {
    pub fn set(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    fn is_set(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Frames the Northbridge's receiver saw, whatever became of them.
pub fn nb_heard(info: &ModemInfo) -> u64 {
    // rx_total, radio_crc_err, radio_hdr_err (COUNTER_NAMES order).
    u64::from(info.counters[0]) + u64::from(info.counters[9]) + u64::from(info.counters[10])
}

/// [`FACTORY_P2P_RADIO`] in one line, for the report.
pub const FACTORY_CHANNEL: &str = "869.525 MHz SF7 BW125 4/5";

fn factory_radio() -> Result<RadioConfig, String> {
    let p: P2pRadio = serde_yaml::from_str(FACTORY_P2P_RADIO).map_err(|e| e.to_string())?;
    RadioConfig::from_hub_config(&p).map_err(|e| e.message().to_string())
}

/// Opens the Northbridge UART, arms its receiver on the factory channel and
/// counts what it hears until `stop`, then stops RX again.
pub fn listen_northbridge(out: Shared, stop: Stop) {
    *lock_recover(&out) = Listen::starting();
    let fail = |out: &Shared, e: String| {
        let mut l = lock_recover(out);
        l.state = "error".into();
        l.error = Some(e);
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(&out, format!("tokio runtime: {e}")),
    };
    let result: Result<(), String> = rt.block_on(async {
        let radio = factory_radio()?;
        let mut modem = SerialModem::open_default().map_err(|e| e.message().to_string())?;
        let info = modem
            .get_info()
            .await
            .map_err(|e| e.message().to_string())?;
        {
            let mut l = lock_recover(&out);
            l.ident = Some(format!(
                "fw {}.{}.{} {:02x}{:02x}{:02x}{:02x}, was {}",
                info.fw[0],
                info.fw[1],
                info.fw[2],
                info.git_sha[0],
                info.git_sha[1],
                info.git_sha[2],
                info.git_sha[3],
                gateway_state::name(info.state)
            ));
        }
        // The station may have left it receiving (`p2p listen`); SET_RADIO_CONFIG
        // is refused in GATEWAY, and RX_STOP is fine in any state.
        modem.rx_stop().await.map_err(|e| e.message().to_string())?;
        modem
            .set_radio_config(&radio)
            .await
            .map_err(|e| e.message().to_string())?;
        // Receive only: ACKs off, and with no peers there is nothing to send.
        let start = TwrCmd::Start {
            net_id: FACTORY_NET_ID,
            turnaround_ms: link::TURNAROUND_MS,
            dl_gap_ms: link::DL_GAP_MS,
            flags: 0,
        };
        modem
            .twr(&start)
            .await
            .map_err(|e| e.message().to_string())?;
        let base = nb_heard(
            &modem
                .get_info()
                .await
                .map_err(|e| e.message().to_string())?,
        );
        lock_recover(&out).state = "listening".into();

        let deadline = Instant::now() + MAX_LISTEN;
        while !stop.is_set() && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let info = modem
                .get_info()
                .await
                .map_err(|e| e.message().to_string())?;
            lock_recover(&out).heard = nb_heard(&info).saturating_sub(base);
        }
        // Leave it as quiet as it was; provisioning configures it afresh.
        let _ = modem.rx_stop().await;
        Ok(())
    });
    match result {
        Ok(()) => lock_recover(&out).state = "stopped".into(),
        Err(e) => fail(&out, format!("Northbridge: {e}")),
    }
}

/// The transient unit the factory test runs concentratord in. Its own unit,
/// as fiber_app may not write the `/run` directory concentratord binds in.
const CONCENTRATORD_UNIT: &str = "proximos-factory-concentratord";

/// Counts `Frame received` lines in concentratord's log (every frame the
/// SX1302 demodulated, CRC-filtered ones aside) and picks up the gateway id.
pub fn parse_concentratord_log(log: &str) -> (u64, Option<String>) {
    let frames = log
        .lines()
        .filter(|l| l.contains("Frame received,"))
        .count() as u64;
    let id = log.lines().find_map(|l| {
        l.split_once("Gateway ID retrieved, gateway_id: ")
            .map(|(_, id)| id.trim().trim_matches(|c| c == '"').to_string())
    });
    (frames, id)
}

/// Runs concentratord on the RAK5146 for the test's duration and counts the
/// frames it logs.
pub fn listen_concentrator(out: Shared, stop: Stop) {
    *lock_recover(&out) = Listen::starting();
    let unit = format!("--unit={CONCENTRATORD_UNIT}");
    let started = Command::new("systemd-run")
        .args([
            unit.as_str(),
            "--collect",
            "--quiet",
            "-p",
            "RuntimeDirectory=chirpstack-concentratord",
            "/usr/bin/chirpstack-concentratord-sx1302",
            "-c",
            "/etc/chirpstack-concentratord/chirpstack-concentratord.toml",
        ])
        .output();
    match started {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let mut l = lock_recover(&out);
            l.state = "error".into();
            l.error = Some(format!(
                "systemd-run: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
            return;
        }
        Err(e) => {
            let mut l = lock_recover(&out);
            l.state = "error".into();
            l.error = Some(format!("systemd-run: {e}"));
            return;
        }
    }
    let since = format!("--unit={CONCENTRATORD_UNIT}");
    let read_log = || {
        Command::new("journalctl")
            .args([
                since.as_str(),
                "--boot",
                "-o",
                "cat",
                "--no-pager",
                "--since=-35min",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    // Frames an earlier run of the unit logged in this boot are not ours.
    let (base, _) = parse_concentratord_log(&read_log());
    let deadline = Instant::now() + MAX_LISTEN;
    while !stop.is_set() && Instant::now() < deadline {
        thread::sleep(POLL);
        let (frames, id) = parse_concentratord_log(&read_log());
        let active = Command::new("systemctl")
            .args(["is-active", "--quiet", CONCENTRATORD_UNIT])
            .status()
            .is_ok_and(|s| s.success());
        let mut l = lock_recover(&out);
        l.heard = frames.saturating_sub(base);
        if id.is_some() {
            l.ident = id;
        }
        if active {
            l.state = "listening".into();
        } else {
            l.state = "error".into();
            l.error =
                Some("concentratord exited; see journalctl -u ".to_string() + CONCENTRATORD_UNIT);
            return;
        }
    }
    let _ = Command::new("systemctl")
        .args(["stop", CONCENTRATORD_UNIT])
        .status();
    lock_recover(&out).state = "stopped".into();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_factory_channel_encodes() {
        let r = factory_radio().unwrap();
        assert_eq!(r.freq_hz, 869_525_000);
        assert_eq!(r.sf, 7);
    }

    #[test]
    fn concentratord_frames_and_gateway_id_come_from_its_log() {
        let log = "Gateway ID retrieved, gateway_id: \"0016c001ff1e2a3b\"\n\
                   Frame received, uplink_id: 1, count_us: 5, freq: 868100000\n\
                   Frame received with invalid CRC, see disable_crc_filter\n\
                   Frame received, uplink_id: 2, count_us: 9, freq: 868300000\n";
        let (n, id) = parse_concentratord_log(log);
        assert_eq!(n, 2);
        assert_eq!(id.as_deref(), Some("0016c001ff1e2a3b"));
    }
}
