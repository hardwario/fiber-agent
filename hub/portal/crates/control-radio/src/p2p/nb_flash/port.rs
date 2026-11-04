//! The serial port the flasher drives — `northbridge_update.md` §3.3.
//!
//! One port, two line settings: the Northbridge application speaks HDLC at 115200 8N1, the
//! STM32 ROM bootloader speaks AN3155 at 115200 8E1. [`FlashPort`] is the seam that lets the
//! whole update procedure run against a simulated bootloader in the tests; [`SerialFlashPort`]
//! is the real `ttyAMA3`.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use tokio_serial::{SerialPort, SerialPortBuilderExt};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::uart::BAUD_RATE;

/// The two line settings of the Northbridge link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// The application's HDLC link, 8N1.
    Hdlc,
    /// The ROM bootloader's AN3155 session, 8E1 (AN3155 §2: even parity is mandatory).
    Bootloader,
}

impl Line {
    pub const fn as_str(self) -> &'static str {
        match self {
            Line::Hdlc => "8N1",
            Line::Bootloader => "8E1",
        }
    }
}

/// What the flasher needs from a serial port.
#[async_trait::async_trait]
pub trait FlashPort: Send {
    /// Switch the line setting. The baud rate stays at 115200.
    async fn set_line(&mut self, line: Line) -> Result<()>;
    /// Write all of `bytes`.
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()>;
    /// Read what arrives within `timeout`, at most `buf.len()` bytes. `Ok(0)` means nothing
    /// arrived in time — silence is not an error at this level.
    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
    /// Drop whatever has been received and not read yet.
    async fn discard_input(&mut self) -> Result<()>;
}

/// A borrowed port is a port, so a caller can lend one (a test its simulated Northbridge) and
/// still look at it afterwards.
#[async_trait::async_trait]
impl<P: FlashPort + ?Sized> FlashPort for &mut P {
    async fn set_line(&mut self, line: Line) -> Result<()> {
        (**self).set_line(line).await
    }
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        (**self).write_all(bytes).await
    }
    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        (**self).read(buf, timeout).await
    }
    async fn discard_input(&mut self) -> Result<()> {
        (**self).discard_input().await
    }
}

/// Read exactly `n` bytes within `timeout`, or `Timeout`/`NorthbridgeUart`.
pub async fn read_exact<P: FlashPort + ?Sized>(
    port: &mut P,
    n: usize,
    timeout: Duration,
    doing: &str,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut out = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let r = if remaining.is_zero() {
            0
        } else {
            port.read(&mut out[got..], remaining).await?
        };
        if r == 0 {
            return Err(Error::new(
                Kind::Timeout,
                Transport::P2p,
                Component::NorthbridgeUart,
                format!("{doing}: {got} of {n} B within {} ms", timeout.as_millis()),
            ));
        }
        got += r;
    }
    Ok(out)
}

fn io_err(doing: &str, e: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::ServiceDown,
        Transport::P2p,
        Component::NorthbridgeUart,
        format!("northbridge UART {doing}: {e}"),
    )
}

/// The real Northbridge port: `/dev/ttyAMA3` (or `PROXIMOS_P2P_UART`), 115200.
pub struct SerialFlashPort {
    port: tokio_serial::SerialStream,
}

impl std::fmt::Debug for SerialFlashPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialFlashPort").finish_non_exhaustive()
    }
}

impl SerialFlashPort {
    /// Open `path` at 115200 8N1, the application's setting.
    pub fn open(path: &str) -> Result<Self> {
        let port = tokio_serial::new(path, BAUD_RATE)
            .data_bits(tokio_serial::DataBits::Eight)
            .parity(tokio_serial::Parity::None)
            .stop_bits(tokio_serial::StopBits::One)
            .open_native_async()
            .map_err(|e| io_err(&format!("cannot open {path}"), e))?;
        Ok(SerialFlashPort { port })
    }

    /// Wrap a port that is already open, e.g. the one the P2P task holds.
    pub fn from_stream(port: tokio_serial::SerialStream) -> Self {
        SerialFlashPort { port }
    }

    /// Hand the port back, at whatever line setting it was left.
    pub fn into_stream(self) -> tokio_serial::SerialStream {
        self.port
    }
}

#[async_trait::async_trait]
impl FlashPort for SerialFlashPort {
    async fn set_line(&mut self, line: Line) -> Result<()> {
        let parity = match line {
            Line::Hdlc => tokio_serial::Parity::None,
            Line::Bootloader => tokio_serial::Parity::Even,
        };
        // Every switch happens after the far side answered, so nothing of ours is still in
        // the transmit queue when the setting changes.
        self.port
            .set_parity(parity)
            .map_err(|e| io_err(&format!("cannot switch to {}", line.as_str()), e))
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.port
            .write_all(bytes)
            .await
            .map_err(|e| io_err("write failed", e))?;
        self.port
            .flush()
            .await
            .map_err(|e| io_err("flush failed", e))
    }

    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        match tokio::time::timeout(timeout, self.port.read(buf)).await {
            Err(_) => Ok(0),
            Ok(Ok(0)) => Err(io_err("link closed", "EOF — the northbridge side hung up")),
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(io_err("read failed", e)),
        }
    }

    async fn discard_input(&mut self) -> Result<()> {
        self.port
            .clear(tokio_serial::ClearBuffer::Input)
            .map_err(|e| io_err("cannot discard input", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_path_that_does_not_exist_is_service_down_northbridge_uart() {
        let err = SerialFlashPort::open("/dev/does-not-exist-nbflash").unwrap_err();
        assert_eq!(err.kind(), Kind::ServiceDown);
        assert_eq!(err.component(), Component::NorthbridgeUart);
    }
}
