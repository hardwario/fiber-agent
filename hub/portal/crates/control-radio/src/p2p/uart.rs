//! The real northbridge link — `plan/system/lora/design.md` §2/§3: the dedicated
//! `/dev/ttyAMA3` UART, 115200 8N1, carrying [`crate::p2p::hdlc`] frames.
//!
//! Everything else in `p2p/` (`hdlc`, `modem`, `session`, `crypto`) is pure byte-in/byte-out
//! logic, exercised so far only against synthetic arrays (`p2p/mod.rs`'s own doc: "the on-air
//! path is not" tested). This is the first piece that actually opens a serial port — nothing
//! here decides what a frame *means*; it only gets bytes on and off the wire and hands
//! complete, CRC-valid frames to whatever calls it.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::hdlc;

/// The dedicated link (`design.md` §2) — distinct from the southbridge's housekeeping-only
/// `/dev/ttyAMA4`.
pub const DEFAULT_PATH: &str = "/dev/ttyAMA3";
/// `design.md` §3: HDLC framing at 115200 8N1.
pub const BAUD_RATE: u32 = 115_200;

/// `PROXIMOS_P2P_UART` overrides the path — a real Hub has exactly one, but a bench or a PTY
/// pair used for testing needs a different one.
pub fn device_path() -> String {
    std::env::var("PROXIMOS_P2P_UART").unwrap_or_else(|_| DEFAULT_PATH.to_string())
}

/// How long a caller waits for the northbridge to answer before calling the link dead.
/// The P2P RX1 budget is about a second (`p2p.md` §3), so a read that has not produced a
/// frame in several times that is not slow, it is not coming.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Pull bytes from `reader` until they complete at least one frame.
///
/// Split out of [`Northbridge::read_frames`] so the read loop is testable against any
/// `AsyncRead` — the serial port itself needs real hardware, the loop around it does not.
///
/// EOF is an error, not a retry: a serial read returning `Ok(0)` means the far side is gone,
/// and treating it as "nothing yet" spins the loop at full CPU forever without ever telling
/// the caller the link is dead.
pub async fn read_frames_from<R: AsyncRead + Unpin>(
    reader: &mut R,
    decoder: &mut hdlc::Decoder,
) -> Result<Vec<hdlc::Frame>> {
    let mut buf = [0u8; 512];
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| uart_err("read failed", e))?;
        if n == 0 {
            return Err(uart_err(
                "link closed",
                "EOF — the northbridge side hung up",
            ));
        }
        let frames = decoder.push(&buf[..n]);
        if !frames.is_empty() {
            return Ok(frames);
        }
    }
}

/// [`read_frames_from`] with a deadline. A silent link is `Timeout`, not a hang: without
/// this a caller waiting on a northbridge that never answers blocks forever, and the boot
/// path has no way to fail.
pub async fn read_frames_within<R: AsyncRead + Unpin>(
    reader: &mut R,
    decoder: &mut hdlc::Decoder,
    deadline: Duration,
) -> Result<Vec<hdlc::Frame>> {
    match tokio::time::timeout(deadline, read_frames_from(reader, decoder)).await {
        Ok(r) => r,
        Err(_) => Err(Error::new(
            Kind::Timeout,
            Transport::P2p,
            Component::NorthbridgeUart,
            format!(
                "northbridge UART: no complete frame within {} ms",
                deadline.as_millis()
            ),
        )),
    }
}

fn uart_err(doing: &str, e: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::ServiceDown,
        Transport::P2p,
        Component::NorthbridgeUart,
        format!("northbridge UART {doing}: {e}"),
    )
}

/// A framed connection to the northbridge.
pub struct Northbridge {
    port: tokio_serial::SerialStream,
    decoder: hdlc::Decoder,
}

impl std::fmt::Debug for Northbridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Northbridge").finish_non_exhaustive()
    }
}

impl Northbridge {
    /// Open the link. `ServiceDown`/`NorthbridgeUart` on failure — the same taxonomy entry
    /// `plan.md` §6 already reserves for "UART dead" — so the exit code is right by construction
    /// rather than by a caller mapping it after the fact.
    pub fn open(path: &str, baud: u32) -> Result<Self> {
        let port = tokio_serial::new(path, baud)
            .data_bits(tokio_serial::DataBits::Eight)
            .parity(tokio_serial::Parity::None)
            .stop_bits(tokio_serial::StopBits::One)
            .open_native_async()
            .map_err(|e| uart_err(&format!("cannot open {path}"), e))?;
        Ok(Northbridge {
            port,
            decoder: hdlc::Decoder::new(),
        })
    }

    /// Open the configured link — [`device_path`] honours `PROXIMOS_P2P_UART`, which nothing
    /// called before, so the override was documented but inert.
    pub fn open_default() -> Result<Self> {
        Self::open(&device_path(), BAUD_RATE)
    }

    /// Encode one frame and write it whole. `hdlc::encode` already frames and byte-stuffs it;
    /// this is the only place that touches the wire.
    pub async fn write_frame(&mut self, frame_type: u8, seq: u8, body: &[u8]) -> Result<()> {
        let wire = hdlc::encode(frame_type, seq, body);
        self.port
            .write_all(&wire)
            .await
            .map_err(|e| uart_err("write failed", e))
    }

    /// Read whatever bytes are available and return every frame they completed. Blocks until at
    /// least one full frame arrives; a corrupt or partial frame is absorbed by the decoder's own
    /// resync (`hdlc::Decoder`, counted in `.stats`) and never surfaced as an error here — a
    /// noisy line is expected, not exceptional. A closed link is not: EOF is `ServiceDown`.
    pub async fn read_frames(&mut self) -> Result<Vec<hdlc::Frame>> {
        read_frames_from(&mut self.port, &mut self.decoder).await
    }

    /// [`Northbridge::read_frames`] bounded by a deadline — `Kind::Timeout` on silence.
    pub async fn read_frames_within(&mut self, deadline: Duration) -> Result<Vec<hdlc::Frame>> {
        read_frames_within(&mut self.port, &mut self.decoder, deadline).await
    }

    /// Decoder statistics (CRC errors, truncated/oversized frames) — diagnostics for a noisy
    /// link, not part of the framed protocol itself.
    pub fn decode_stats(&self) -> hdlc::DecodeStats {
        self.decoder.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The read loop is tested against `AsyncRead` rather than a serial port: `tokio::io::duplex`
    // reproduces every case that matters here — a whole frame, a frame split across two reads,
    // EOF, and silence — without needing linked pty device nodes (`socat pty,link=...`), which
    // is not a test-time dependency of this crate and is not installed everywhere this runs.
    //
    // What remains untested is only the `tokio-serial` wiring itself: opening a real port and
    // the bytes actually reaching a northbridge. That was verified by hand against a
    // Python-created PTY pair during development, and the on-air path stays unexercised until
    // there is hardware to exercise it against.

    #[test]
    fn opening_a_path_that_does_not_exist_is_service_down_northbridge_uart() {
        let err = Northbridge::open("/dev/does-not-exist-0825", BAUD_RATE).unwrap_err();
        assert_eq!(err.kind(), Kind::ServiceDown);
        assert_eq!(err.component(), Component::NorthbridgeUart);
    }

    #[test]
    fn the_default_path_is_ttyama3_not_ttyama4() {
        assert_eq!(DEFAULT_PATH, "/dev/ttyAMA3");
    }

    #[tokio::test]
    async fn eof_is_a_dead_link_not_a_spin() {
        let mut empty: &[u8] = &[];
        let mut decoder = hdlc::Decoder::new();
        let err = read_frames_from(&mut empty, &mut decoder)
            .await
            .expect_err("EOF must be an error");
        assert_eq!(err.kind(), Kind::ServiceDown);
        assert_eq!(err.component(), Component::NorthbridgeUart);
        assert!(err.to_string().contains("EOF"), "names the cause: {err}");
    }

    #[tokio::test]
    async fn a_complete_frame_on_the_wire_decodes() {
        let wire = hdlc::encode(0x81, 1, b"hello");
        let mut cursor: &[u8] = &wire;
        let mut decoder = hdlc::Decoder::new();
        let frames = read_frames_from(&mut cursor, &mut decoder)
            .await
            .expect("one frame");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_type, 0x81);
        assert_eq!(frames[0].seq, 1);
        assert_eq!(frames[0].body.as_slice(), b"hello");
    }

    #[tokio::test]
    async fn a_frame_split_across_reads_still_decodes() {
        let wire = hdlc::encode(0x81, 2, b"split");
        let (first, second) = wire.split_at(wire.len() / 2);
        let (first, second) = (first.to_vec(), second.to_vec());
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let feed = tokio::spawn(async move {
            writer.write_all(&first).await.expect("first half");
            tokio::task::yield_now().await;
            writer.write_all(&second).await.expect("second half");
        });
        let mut decoder = hdlc::Decoder::new();
        let frames = read_frames_from(&mut reader, &mut decoder)
            .await
            .expect("one frame");
        feed.await.expect("writer task");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].body.as_slice(), b"split");
    }

    #[tokio::test]
    async fn a_silent_link_times_out_as_timeout_northbridge_uart() {
        let (mut reader, _writer_kept_open) = tokio::io::duplex(64);
        let mut decoder = hdlc::Decoder::new();
        let err = read_frames_within(&mut reader, &mut decoder, Duration::from_millis(50))
            .await
            .expect_err("silence must time out");
        assert_eq!(err.kind(), Kind::Timeout);
        assert_eq!(err.component(), Component::NorthbridgeUart);
    }
}
