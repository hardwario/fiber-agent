//! The EN12830 archive — `plan/control/ble/beacon-btsmp1.md` §3, `plan/control/ble/central.md` §6.
//!
//! The recorder variant is a distinct SKU with a white food-grade housing, certified against
//! EN 12830:2018, holding on the order of tens of thousands of internal logs. The vendor retrieves
//! them through a dedicated evaluation version of their app; **there is no published protocol.**
//! Everything here was inferred from observed traffic, which is why [`capability`] exists.
//!
//! The public API is **blocking** on purpose: it is driven from
//! [`capability::En12830Capability::run_contained`], which runs it under `spawn_blocking` with a hard
//! deadline, so the scan loop never blocks on a download.

pub mod att;
pub mod capability;
pub mod protocol;
pub mod xtea;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bluer::Uuid;

use crate::error::{Component, Error, Kind, Result, Role};
use crate::mac::MacAddr;

use super::provision::UUID_PASSWORD;
use att::{Att, ADDR_TYPE_LE_PUBLIC, ADDR_TYPE_LE_RANDOM};
use protocol::{
    parse_record_info, ArchiveDecoder, RecordInfo, CHUNK_PACING_MS, CMD_SEND_NEXT_CHUNK,
    CMD_START_RECORD, CMD_START_RECORD_SEND, CMD_START_RECORD_SEND_TS, CMD_STOP_RECORD,
    CMD_TIME_SYNC, MAX_CHUNKS, RESP_EMPTY, RESP_NO_MORE_CHUNK, RESP_OK, RESTART_RETRY_MS,
    STOP_SETTLE_MS,
};

/// Recorder service, base `e61c00xx-7df8-4d4e-8e6d-c611745b92e9`.
pub const UUID_RECORD_INFO: Uuid = Uuid::from_u128(0xe61c0001_7df8_4d4e_8e6d_c611745b92e9);
pub const UUID_RANDOM: Uuid = Uuid::from_u128(0xe61c0002_7df8_4d4e_8e6d_c611745b92e9);
pub const UUID_RECORD_DATA: Uuid = Uuid::from_u128(0xe61c0003_7df8_4d4e_8e6d_c611745b92e9);
pub const UUID_RECORDER_COMMAND: Uuid = Uuid::from_u128(0xe61c0004_7df8_4d4e_8e6d_c611745b92e9);

/// Connect retries, from §3.
pub const CONNECT_ATTEMPTS: u32 = 4;
pub const CONNECT_RETRY: Duration = Duration::from_millis(2_500);

/// The handles a recorder session needs, resolved **by UUID** over the whole attribute database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderHandles {
    pub password: u16,
    pub record_info: u16,
    pub random: u16,
    pub record_data: u16,
    pub command: u16,
}

/// Pick the handles out of a discovered characteristic map.
///
/// A **missing recorder characteristic is permanent** — it means a standard beacon, not a transient
/// failure — so it is never retried. It is also the **only** outcome that may set
/// `is_en12830 = Some(false)`.
pub fn resolve_handles(found: &BTreeMap<Uuid, u16>) -> Result<RecorderHandles> {
    let need = |uuid: Uuid, what: &str| -> Result<u16> {
        found.get(&uuid).copied().ok_or_else(|| {
            Error::new(
                Kind::NotFound,
                Role::Central,
                Component::BeaconGatt,
                format!("not an EN12830 recorder: {what} ({uuid}) is absent"),
            )
        })
    };
    Ok(RecorderHandles {
        password: need(UUID_PASSWORD, "the configuration password characteristic")?,
        record_info: need(UUID_RECORD_INFO, "record info")?,
        random: need(UUID_RANDOM, "random")?,
        record_data: need(UUID_RECORD_DATA, "record data")?,
        command: need(UUID_RECORDER_COMMAND, "the recorder command")?,
    })
}

/// Is this error the "standard beacon, not a recorder" answer?
///
/// The distinction is load-bearing: only this may write `is_en12830 = Some(false)`. Anything else
/// leaves the axis `None`, because "we could not tell" is not "not a recorder".
pub fn is_not_recorder(e: &Error) -> bool {
    e.kind() == Kind::NotFound && e.message().contains("not an EN12830 recorder")
}

/// Map a socket error to the shared table.
///
/// This is the misclassification central.md §6 flags: `ENODEV`/`ENETDOWN`/`EAFNOSUPPORT` and the
/// permission errors are the **adapter** being absent or forbidden, not the beacon being out of
/// range, and reporting them as `BeaconUnreachable` sends an operator to look at the wrong device.
pub fn classify_io(e: &std::io::Error, mac: &MacAddr) -> Error {
    use std::io::ErrorKind as K;
    let raw = e.raw_os_error();
    let adapter_gone = matches!(
        raw,
        Some(libc::ENODEV) | Some(libc::ENETDOWN) | Some(libc::EAFNOSUPPORT)
    ) || matches!(e.kind(), K::PermissionDenied);
    if adapter_gone {
        return Error::new(
            Kind::AdapterDown,
            Role::Central,
            Component::BluetoothAdapter,
            format!("the local controller is not usable for a raw L2CAP dial: {e}"),
        );
    }
    let unreachable = matches!(
        e.kind(),
        K::TimedOut | K::WouldBlock | K::ConnectionRefused | K::HostUnreachable | K::NotConnected
    ) || matches!(raw, Some(libc::EHOSTDOWN) | Some(libc::ECONNRESET));
    if unreachable {
        return Error::new(
            Kind::BeaconUnreachable,
            Role::Central,
            Component::BeaconGatt,
            format!("{mac} did not answer the raw L2CAP dial: {e}"),
        );
    }
    Error::new(
        Kind::Transport,
        Role::Central,
        Component::L2cap,
        format!("raw L2CAP failure against {mac}: {e}"),
    )
}

/// A connected recorder session. **Blocking**; run it inside `run_contained`.
#[derive(Debug)]
pub struct Recorder {
    att: Att,
    handles: RecorderHandles,
    mac: MacAddr,
}

/// One decrypted archive chunk as it came off the air: the plaintext index and the decrypted body.
pub type RawChunk = (u16, Vec<u8>);

/// What the chunk loop reports after every chunk, so a caller can publish progress and stream the
/// records while the dial is still running (beacon_history.md §2.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownloadProgress<'a> {
    pub chunks_read: u16,
    /// Records decoded so far that fall inside the requested window.
    pub received: usize,
    /// This chunk's records that are new: inside the window, not ahead of the clock, not already
    /// reported by an earlier chunk; sorted. Final — the end of the download filters nothing more
    /// out of them, so a caller may send them at once.
    pub fresh: &'a [(i64, f64)],
}

/// What a download produced.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Download {
    pub records: Vec<(i64, f64)>,
    /// Whether the beacon was left recording. **A beacon left not recording is the failure to
    /// surface**, so this is a reported field rather than an assumption.
    pub restart_ok: bool,
    pub chunks_read: u16,
    /// Session headers walked (see [`protocol::ArchiveDecoder`]).
    pub sessions: u16,
    /// Chunks dropped because they were neither data nor a believable session header.
    pub rejected_chunks: u16,
    /// Records dropped for a timestamp ahead of the Hub's clock.
    pub future_dropped: usize,
    /// Every chunk read, decrypted, in order — kept so the block CRC can be identified offline
    /// (beacon_history.md §7 Q6 layer 2). Not part of the archive segment, which stays as it was.
    #[serde(skip)]
    pub chunks: Vec<RawChunk>,
}

/// Whether a record falls inside `[since, until]` and is not ahead of the clock that was synced.
fn in_window(ts: i64, since: i64, until: Option<i64>, horizon: Option<i64>) -> bool {
    ts >= since && until.is_none_or(|u| ts <= u) && horizon.is_none_or(|h| ts <= h)
}

/// The horizon past which a record cannot be real: the synced clock plus one interval. `None` when
/// the Hub's own clock is unknown (`now == 0`).
fn future_horizon(now: u32, interval_s: u16) -> Option<i64> {
    (now != 0).then(|| i64::from(now) + i64::from(interval_s.max(60)))
}

/// One chunk's share of the window, as [`DownloadProgress::fresh`] reports it. `seen` holds the
/// timestamps already reported; the first record for a timestamp wins, as in [`keep_window`].
fn fresh_in_window(
    decoded: &[(i64, f64)],
    since: i64,
    until: Option<i64>,
    horizon: Option<i64>,
    seen: &mut BTreeSet<i64>,
) -> Vec<(i64, f64)> {
    let mut fresh: Vec<(i64, f64)> = decoded
        .iter()
        .copied()
        .filter(|(ts, _)| in_window(*ts, since, until, horizon) && seen.insert(*ts))
        .collect();
    fresh.sort_by_key(|(ts, _)| *ts);
    fresh
}

/// Cut a decoded archive to what the caller asked for: drop records ahead of the synced clock
/// (counted), then everything outside `[since, until]`, and return the rest sorted and unique by
/// timestamp. The device returns whole pages, so this is where the window is actually applied.
pub fn keep_window(
    mut records: Vec<(i64, f64)>,
    since: i64,
    until: Option<i64>,
    now: u32,
    interval_s: u16,
) -> (Vec<(i64, f64)>, usize) {
    // A record ahead of the clock that was just synced cannot be real; one would also poison the
    // resume point (`MAX(ts)`) of every later download.
    let horizon = future_horizon(now, interval_s);
    let before = records.len();
    records.retain(|(ts, _)| horizon.is_none_or(|h| *ts <= h));
    let future_dropped = before - records.len();
    records.retain(|(ts, _)| in_window(*ts, since, until, None));
    records.sort_by_key(|(ts, _)| *ts);
    records.dedup_by_key(|(ts, _)| *ts);
    (records, future_dropped)
}

impl Recorder {
    /// Connect and resolve the handles.
    ///
    /// Four attempts 2.5 s apart, then give up — except for a missing recorder characteristic,
    /// which is permanent and is not retried at all.
    pub fn connect(mac: &MacAddr) -> Result<Recorder> {
        let mut last: Option<Error> = None;
        for attempt in 0..CONNECT_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(CONNECT_RETRY);
            }
            // Public first, then random: §3's documented order.
            for addr_type in [ADDR_TYPE_LE_PUBLIC, ADDR_TYPE_LE_RANDOM] {
                match Att::connect(mac, addr_type) {
                    Ok(mut att) => {
                        let _ = att.exchange_mtu();
                        let decls = att.read_by_type_chars().map_err(|e| classify_io(&e, mac))?;
                        let map: BTreeMap<Uuid, u16> =
                            decls.iter().map(|d| (d.uuid(), d.value_handle)).collect();
                        // Permanent: return immediately rather than burning the retry budget on a
                        // beacon that is simply a standard one.
                        let handles = resolve_handles(&map)?;
                        let r = Recorder {
                            att,
                            handles,
                            mac: *mac,
                        };
                        r.unlock()?;
                        return Ok(r);
                    }
                    Err(e) => last = Some(classify_io(&e, mac)),
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::new(
                Kind::BeaconUnreachable,
                Role::Central,
                Component::BeaconGatt,
                format!("{mac} did not answer the raw L2CAP dial"),
            )
        }))
    }

    /// The plaintext password write that unlocks the recorder reads.
    fn unlock(&self) -> Result<()> {
        self.att
            .write_req(self.handles.password, super::provision::DEFAULT_PASSWORD)
            .map_err(|e| classify_io(&e, &self.mac))?;
        Ok(())
    }

    fn io<T>(&self, r: std::io::Result<T>) -> Result<T> {
        r.map_err(|e| classify_io(&e, &self.mac))
    }

    /// `send_cmd`: read the random, build `random ‖ opcode ‖ params`, write it encrypted, read the
    /// response code back.
    pub fn send_cmd(&self, opcode: u16, params: &[u8]) -> Result<u8> {
        let rnd = self.io(self.att.read_req(self.handles.random))?;
        let random = if rnd.len() >= 2 {
            u16::from_le_bytes([rnd[0], rnd[1]])
        } else {
            // §3: zero when the read was shorter than two bytes.
            0
        };
        let plaintext = protocol::command_plaintext(random, opcode, params);
        self.io(self
            .att
            .write_req(self.handles.command, &xtea::encrypt(&plaintext)))?;
        let back = self.io(self.att.read_req(self.handles.command))?;
        Ok(back.first().copied().unwrap_or(RESP_EMPTY))
    }

    pub fn read_record_info(&self) -> Result<RecordInfo> {
        let raw = self.io(self.att.read_long(self.handles.record_info))?;
        parse_record_info(&xtea::decrypt(&raw))
    }

    /// One chunk: the index is plaintext, the body encrypted.
    fn read_chunk(&self) -> Result<(u16, Vec<u8>)> {
        let raw = self.io(self.att.read_long(self.handles.record_data))?;
        if raw.len() < 2 {
            return Err(Error::new(
                Kind::Transport,
                Role::Central,
                Component::L2cap,
                format!(
                    "an archive chunk from {} was {} bytes: too short to carry its index",
                    self.mac,
                    raw.len()
                ),
            ));
        }
        let idx = u16::from_le_bytes([raw[0], raw[1]]);
        Ok((idx, xtea::decrypt(&raw[2..])))
    }

    /// §3's download procedure, in order.
    ///
    /// STOP_RECORD → settle → START_RECORD_SEND_TS (falling back to the full send if refused) →
    /// chunk loop → TIME_SYNC → re-START at `restart_s`, and only records inside `[since, until]`
    /// are kept, because the device returns whole pages. `progress` is called after every chunk.
    /// `restart_s: None` leaves the recorder stopped (and reports `restart_ok: false`): the drain
    /// of a stopped recorder before it is started, which its own start follows.
    pub fn download_since(
        &self,
        since: i64,
        until: Option<i64>,
        interval_s: u16,
        restart_s: Option<u16>,
        now: u32,
        progress: &mut dyn FnMut(DownloadProgress<'_>),
    ) -> Result<Download> {
        self.send_cmd(CMD_STOP_RECORD, &[])?;
        std::thread::sleep(Duration::from_millis(STOP_SETTLE_MS));

        // Ask for just the tail if the beacon supports it; some firmware refuses the opcode.
        let ts_from = u32::try_from(since.max(1)).unwrap_or(1);
        let mut resp = self.send_cmd(CMD_START_RECORD_SEND_TS, &protocol::u32_params(ts_from))?;
        if resp != RESP_OK {
            resp = self.send_cmd(CMD_START_RECORD_SEND, &[])?;
        }
        if resp != RESP_OK {
            return Err(Error::new(
                Kind::Transport,
                Role::Central,
                Component::L2cap,
                format!(
                    "{} refused to begin an archive send (response 0x{resp:02X})",
                    self.mac
                ),
            ));
        }

        let mut records: Vec<(i64, f64)> = Vec::new();
        let mut chunks: Vec<RawChunk> = Vec::new();
        let mut decoder = ArchiveDecoder::new(interval_s, now);
        let mut chunks_read = 0u16;
        let mut received = 0usize;
        let mut seen: BTreeSet<i64> = BTreeSet::new();
        let mut last_idx: Option<u16> = None;
        let horizon = future_horizon(now, interval_s);

        for chunk in 0..MAX_CHUNKS {
            let mut last = false;
            if chunk > 0 {
                let r = self.send_cmd(CMD_SEND_NEXT_CHUNK, &[])?;
                if r == RESP_NO_MORE_CHUNK {
                    // The tail of the active session can still be waiting behind NO_MORE_CHUNK
                    // (hardware: a header counted 699 samples, the chunks before it carried 660).
                    // Read once more and keep it only if it is a new index.
                    last = true;
                } else if r != RESP_OK {
                    break;
                }
                std::thread::sleep(Duration::from_millis(CHUNK_PACING_MS));
            }
            let (idx, body) = match self.read_chunk() {
                Ok(c) => c,
                Err(_) if last => break,
                Err(e) => return Err(e),
            };
            if last && last_idx.is_some_and(|l| idx <= l) {
                break;
            }
            chunks_read = chunks_read.saturating_add(1);
            last_idx = Some(idx);
            let decoded = decoder.feed(idx, &body);
            let fresh = fresh_in_window(&decoded, since, until, horizon, &mut seen);
            received += fresh.len();
            records.extend(decoded);
            chunks.push((idx, body));
            progress(DownloadProgress {
                chunks_read,
                received,
                fresh: &fresh,
            });
            if last {
                break;
            }
        }

        self.send_cmd(CMD_TIME_SYNC, &protocol::u32_params(now))?;

        // Leave it recording. One retry, then report the truth rather than assuming.
        let mut restart_ok = false;
        if let Some(restart_s) = restart_s {
            let restart = protocol::start_record_params(restart_s, now);
            restart_ok = self.send_cmd(CMD_START_RECORD, &restart)? == RESP_OK;
            if !restart_ok {
                std::thread::sleep(Duration::from_millis(RESTART_RETRY_MS));
                restart_ok = self.send_cmd(CMD_START_RECORD, &restart)? == RESP_OK;
            }
        }

        let (records, future_dropped) = keep_window(records, since, until, now, interval_s);
        Ok(Download {
            records,
            restart_ok,
            chunks_read,
            sessions: decoder.sessions,
            rejected_chunks: decoder.rejected_chunks,
            future_dropped,
            chunks,
        })
    }

    /// Start or stop recording. `interval_s == 0` stops.
    ///
    /// **A START at an interval other than the running one, or on a stopped recorder, erases the
    /// whole archive** (hardware, 2026-10-09; beacon-btsmp1.md §3): read it first.
    pub fn set_recording(&self, interval_s: u16, now: u32) -> Result<bool> {
        let resp = if interval_s == 0 {
            self.send_cmd(CMD_STOP_RECORD, &[])?
        } else {
            self.send_cmd(
                CMD_START_RECORD,
                &protocol::start_record_params(interval_s, now),
            )?
        };
        Ok(resp == RESP_OK)
    }
}

/// The recorder operations a recording dial uses: the seam that lets the drain-before-change rule
/// (beacon_history.md §8, amended 2026-10-09) be tested without a radio. [`Recorder`] is the one
/// production implementation.
pub trait RecorderOps {
    fn read_record_info(&self) -> Result<RecordInfo>;
    /// Start (`interval_s > 0`) or stop (`0`) recording; whether the beacon answered OK.
    fn set_recording(&self, interval_s: u16, now: u32) -> Result<bool>;
    /// [`Recorder::download_since`].
    fn download_since(
        &self,
        since: i64,
        until: Option<i64>,
        interval_s: u16,
        restart_s: Option<u16>,
        now: u32,
        progress: &mut dyn FnMut(DownloadProgress<'_>),
    ) -> Result<Download>;
}

impl RecorderOps for Recorder {
    fn read_record_info(&self) -> Result<RecordInfo> {
        Recorder::read_record_info(self)
    }
    fn set_recording(&self, interval_s: u16, now: u32) -> Result<bool> {
        Recorder::set_recording(self, interval_s, now)
    }
    fn download_since(
        &self,
        since: i64,
        until: Option<i64>,
        interval_s: u16,
        restart_s: Option<u16>,
        now: u32,
        progress: &mut dyn FnMut(DownloadProgress<'_>),
    ) -> Result<Download> {
        Recorder::download_since(self, since, until, interval_s, restart_s, now, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac() -> MacAddr {
        MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac")
    }

    fn handle_map(present: &[Uuid]) -> BTreeMap<Uuid, u16> {
        present
            .iter()
            .enumerate()
            .map(|(i, u)| (*u, 0x20 + i as u16))
            .collect()
    }

    const ALL: [Uuid; 5] = [
        UUID_PASSWORD,
        UUID_RECORD_INFO,
        UUID_RANDOM,
        UUID_RECORD_DATA,
        UUID_RECORDER_COMMAND,
    ];

    #[test]
    fn a_recorder_resolves_every_handle_by_uuid() {
        let h = resolve_handles(&handle_map(&ALL)).expect("resolves");
        assert_eq!(h.password, 0x20);
        assert_eq!(h.record_info, 0x21);
        assert_eq!(h.random, 0x22);
        assert_eq!(h.record_data, 0x23);
        assert_eq!(h.command, 0x24);
    }

    #[test]
    fn a_standard_beacon_is_not_a_recorder_and_that_is_the_only_way_to_learn_it() {
        // Each missing recorder characteristic gives the one error that may set is_en12830=false.
        for missing in ALL {
            let present: Vec<Uuid> = ALL.iter().copied().filter(|u| *u != missing).collect();
            let err = resolve_handles(&handle_map(&present)).expect_err("must refuse");
            assert_eq!(err.kind(), Kind::NotFound);
            assert_eq!(err.component(), Component::BeaconGatt);
            assert!(
                is_not_recorder(&err),
                "this is the tri-state's only false branch: {}",
                err.message()
            );
            assert!(
                err.message().contains(&missing.to_string()),
                "the message names what was absent: {}",
                err.message()
            );
        }
    }

    #[test]
    fn nothing_else_is_allowed_to_claim_not_a_recorder() {
        // A dial that timed out means "we could not tell", and must leave the axis None.
        for e in [
            Error::new(
                Kind::BeaconUnreachable,
                Role::Central,
                Component::BeaconGatt,
                "no answer",
            ),
            Error::new(Kind::Timeout, Role::Central, Component::L2cap, "deadline"),
            Error::new(
                Kind::AdapterDown,
                Role::Central,
                Component::BluetoothAdapter,
                "no controller",
            ),
            Error::new(
                Kind::NotFound,
                Role::Central,
                Component::BeaconRegistry,
                "not enrolled",
            ),
        ] {
            assert!(!is_not_recorder(&e), "{}", e.message());
        }
    }

    #[test]
    fn an_absent_controller_is_adapter_down_and_not_beacon_unreachable() {
        // The misclassification central.md §6 flags: sending an operator to look at the beacon when
        // the problem is the Hub's own controller.
        for raw in [libc::ENODEV, libc::ENETDOWN, libc::EAFNOSUPPORT] {
            let e = classify_io(&std::io::Error::from_raw_os_error(raw), &mac());
            assert_eq!(e.kind(), Kind::AdapterDown, "errno {raw}");
            assert_eq!(e.component(), Component::BluetoothAdapter);
        }
        let e = classify_io(&std::io::Error::from_raw_os_error(libc::EACCES), &mac());
        assert_eq!(e.kind(), Kind::AdapterDown);
        let e = classify_io(&std::io::Error::from_raw_os_error(libc::EPERM), &mac());
        assert_eq!(e.kind(), Kind::AdapterDown);
    }

    #[test]
    fn a_silent_beacon_is_beacon_unreachable() {
        for raw in [libc::ETIMEDOUT, libc::ECONNREFUSED, libc::EHOSTUNREACH] {
            let e = classify_io(&std::io::Error::from_raw_os_error(raw), &mac());
            assert_eq!(e.kind(), Kind::BeaconUnreachable, "errno {raw}");
            assert_eq!(e.component(), Component::BeaconGatt);
            assert!(e.message().contains("AA:BB:CC:DD:EE:FF"));
        }
    }

    #[test]
    fn anything_unrecognised_is_a_transport_failure_which_is_the_only_kind_that_alarms() {
        let e = classify_io(&std::io::Error::from_raw_os_error(libc::EINVAL), &mac());
        assert_eq!(e.kind(), Kind::Transport);
        assert_eq!(e.component(), Component::L2cap);
    }

    #[test]
    fn the_recorder_uuids_are_the_documented_strings() {
        assert_eq!(
            UUID_RECORD_INFO.to_string(),
            "e61c0001-7df8-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_RANDOM.to_string(),
            "e61c0002-7df8-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_RECORD_DATA.to_string(),
            "e61c0003-7df8-4d4e-8e6d-c611745b92e9"
        );
        assert_eq!(
            UUID_RECORDER_COMMAND.to_string(),
            "e61c0004-7df8-4d4e-8e6d-c611745b92e9"
        );
        // The recorder base differs from the config base in exactly one nibble pair: f8 vs f2.
        assert!(UUID_RECORD_INFO.to_string().contains("7df8"));
        assert!(UUID_PASSWORD.to_string().contains("7df2"));
    }

    #[test]
    fn what_streams_per_chunk_is_exactly_what_the_window_keeps() {
        // Records go out chunk by chunk while the dial runs, so nothing sent may later be filtered:
        // the union of every chunk's `fresh` must equal the final window, in content and order.
        let now = 10_000u32;
        let chunks: [&[(i64, f64)]; 3] = [
            &[(7_000, 3.0), (8_000, 2.0), (9_000, 1.0)],
            // A re-sent page: the duplicate keeps its first value, as keep_window does.
            &[(9_000, 9.9), (9_400, 4.0), (8_500, 6.0)],
            &[(20_000, 5.0), (9_600, 7.0), (9_700, 8.0)],
        ];
        let (since, until) = (8_000, Some(9_600));
        let horizon = future_horizon(now, 60);
        let mut seen = BTreeSet::new();
        let streamed: Vec<Vec<(i64, f64)>> = chunks
            .iter()
            .map(|c| fresh_in_window(c, since, until, horizon, &mut seen))
            .collect();
        assert_eq!(
            streamed,
            vec![
                vec![(8_000, 2.0), (9_000, 1.0)],
                vec![(8_500, 6.0), (9_400, 4.0)],
                vec![(9_600, 7.0)],
            ]
        );
        let all: Vec<(i64, f64)> = chunks.concat();
        let (kept, _) = keep_window(all, since, until, now, 60);
        let mut union: Vec<(i64, f64)> = streamed.concat();
        union.sort_by_key(|(ts, _)| *ts);
        assert_eq!(union, kept);
    }

    #[test]
    fn the_window_keeps_since_to_until_and_counts_only_the_future_as_dropped() {
        let now = 10_000u32;
        let records = vec![
            (9_000, 1.0),
            (8_000, 2.0),
            (8_000, 2.0),
            (7_000, 3.0),
            (9_500, 4.0),
            // Ahead of now + 60 s: impossible, counted.
            (20_000, 5.0),
        ];
        let (kept, future) = keep_window(records.clone(), 8_000, Some(9_000), now, 60);
        assert_eq!(
            kept,
            vec![(8_000, 2.0), (9_000, 1.0)],
            "sorted, unique, inclusive"
        );
        assert_eq!(
            future, 1,
            "only the future record counts; the window cut does not"
        );

        let (kept, _) = keep_window(records.clone(), 0, None, now, 60);
        assert_eq!(
            kept.len(),
            4,
            "no window: everything that is not in the future"
        );

        // An unknown clock keeps the far record rather than guessing.
        let (kept, future) = keep_window(records, 0, None, 0, 60);
        assert_eq!((kept.len(), future), (5, 0));
    }

    #[test]
    fn the_raw_chunks_never_reach_the_archive_segment() {
        let d = Download {
            records: vec![(1_000, 4.5)],
            restart_ok: true,
            chunks_read: 1,
            sessions: 1,
            rejected_chunks: 0,
            future_dropped: 0,
            chunks: vec![(0, vec![0xAB; 64])],
        };
        let v = serde_json::to_value(&d).expect("serializes");
        assert!(v.get("chunks").is_none(), "{v}");
        assert_eq!(v["records"][0][0], 1_000);
    }

    #[test]
    fn the_connect_budget_is_the_documented_one() {
        assert_eq!(CONNECT_ATTEMPTS, 4);
        assert_eq!(CONNECT_RETRY, Duration::from_millis(2_500));
    }
}
