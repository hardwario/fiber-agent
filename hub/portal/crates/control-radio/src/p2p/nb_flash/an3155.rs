//! An AN3155 client: the STM32 ROM bootloader over USART — `northbridge_update.md` §3.3, §9.
//!
//! The subset the Northbridge update needs: sync, `GET`, `GET_ID`, Read Memory, Go, Write
//! Memory, Extended Erase and Get Checksum. Nothing here knows the Northbridge's flash
//! layout; the procedure in [`super`] decides what to erase and where to write, so the same
//! client can later drive the southbridge (§7).
//!
//! Every command is the command byte and its complement, answered `ACK` (0x79) or `NACK`
//! (0x1F). Addresses go big-endian with an XOR checksum. A `NACK` returns the bootloader to
//! waiting for a command, so a failed command can simply be sent again.

use std::time::Duration;

use crate::error::{Component, Error, Kind, Result, Transport};

use super::port::{read_exact, FlashPort};

pub const ACK: u8 = 0x79;
pub const NACK: u8 = 0x1F;
/// The autobaud / sync byte.
pub const SYNC: u8 = 0x7F;
/// The most a Read or Write Memory command moves.
pub const MAX_BLOCK: usize = 256;

/// Command bytes.
pub mod cmd {
    pub const GET: u8 = 0x00;
    pub const GET_ID: u8 = 0x02;
    pub const READ_MEMORY: u8 = 0x11;
    pub const GO: u8 = 0x21;
    pub const WRITE_MEMORY: u8 = 0x31;
    pub const EXTENDED_ERASE: u8 = 0x44;
    pub const GET_CHECKSUM: u8 = 0xA1;
}

/// STM32WL5x, the Northbridge's product ID (§9: `GET_ID` → `01 04 97`).
pub const PID_STM32WL5X: u16 = 0x0497;

/// How long each kind of answer may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// One answer to the sync byte.
    pub sync: Duration,
    /// An `ACK` and the bytes of an ordinary reply.
    pub reply: Duration,
    /// The `ACK` after a Write Memory block (flash programming).
    pub write: Duration,
    /// The `ACK` after an Extended Erase: fixed part …
    pub erase_base: Duration,
    /// … plus this per page (STM32WL page erase is ~22 ms max).
    pub erase_per_page: Duration,
    /// The CRC of a Get Checksum.
    pub checksum: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            sync: Duration::from_millis(500),
            reply: Duration::from_secs(1),
            write: Duration::from_secs(1),
            erase_base: Duration::from_secs(2),
            erase_per_page: Duration::from_millis(100),
            checksum: Duration::from_secs(5),
        }
    }
}

/// How the bootloader answered the sync byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synced {
    /// `ACK`: a fresh bootloader, autobaud done.
    Fresh,
    /// `NACK` (or silence then `NACK`) and a `GET` that worked: the bootloader was already
    /// synced, e.g. by an earlier, interrupted run.
    Already,
}

/// What `GET` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootloaderInfo {
    /// e.g. `0x33` for v3.3.
    pub version: u8,
    pub commands: Vec<u8>,
}

impl BootloaderInfo {
    pub fn supports(&self, command: u8) -> bool {
        self.commands.contains(&command)
    }
}

/// XOR of all bytes: AN3155's checksum.
pub fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |a, b| a ^ b)
}

/// An address as AN3155 sends it: 4 B big-endian plus their XOR.
pub fn address_frame(addr: u32) -> [u8; 5] {
    let b = addr.to_be_bytes();
    [b[0], b[1], b[2], b[3], xor(&b)]
}

/// The CRC the STM32 CRC unit computes with its reset configuration — what Get Checksum
/// (0xA1) returns: polynomial 0x04C11DB7, init 0xFFFFFFFF, no reflection, no final XOR, fed
/// one 32-bit word at a time, MSB first. Each word is read from flash as the CPU reads it, so
/// it is the **little-endian** u32 of four consecutive bytes. A trailing partial word is padded
/// with 0xFF, as erased flash reads.
///
/// The word order and the byte order of the reply follow AN3155 and the reference manual,
/// but neither has been observed on the Northbridge yet: **confirm on hardware in U5** (run
/// with [`super::VerifyMode::Both`] once; a CRC mismatch with a matching read-back means this
/// function is wrong, not the flash).
pub fn stm32_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for chunk in data.chunks(4) {
        let mut w = [0xFFu8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        crc ^= u32::from_le_bytes(w);
        for _ in 0..32 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn nacked(doing: &str) -> Error {
    Error::new(
        Kind::Transport,
        Transport::P2p,
        Component::NorthbridgeFirmware,
        format!("ROM bootloader: {doing}: NACK"),
    )
}

/// Was this error a `NACK` from the bootloader (as opposed to silence or a broken line)?
pub fn is_nack(e: &Error) -> bool {
    e.kind() == Kind::Transport && e.message().ends_with(": NACK")
}

fn protocol(doing: &str, what: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::Transport,
        Transport::P2p,
        Component::NorthbridgeFirmware,
        format!("ROM bootloader: {doing}: {what}"),
    )
}

/// A session with the ROM bootloader on a port already switched to 8E1.
pub struct RomBootloader<'p, P: FlashPort + ?Sized> {
    port: &'p mut P,
    t: Timeouts,
}

impl<'p, P: FlashPort + ?Sized> RomBootloader<'p, P> {
    pub fn new(port: &'p mut P, t: Timeouts) -> Self {
        RomBootloader { port, t }
    }

    /// One `ACK`/`NACK` byte. Anything else is a protocol error.
    async fn answer(&mut self, timeout: Duration, doing: &str) -> Result<bool> {
        let b = read_exact(self.port, 1, timeout, &format!("ROM bootloader: {doing}")).await?;
        match b[0] {
            ACK => Ok(true),
            NACK => Ok(false),
            other => Err(protocol(
                doing,
                format!("answered {other:#04x}, not ACK/NACK"),
            )),
        }
    }

    async fn expect_ack(&mut self, timeout: Duration, doing: &str) -> Result<()> {
        if self.answer(timeout, doing).await? {
            Ok(())
        } else {
            Err(nacked(doing))
        }
    }

    /// Send a command byte and its complement; `ACK` expected.
    async fn command(&mut self, c: u8, doing: &str) -> Result<()> {
        self.port.write_all(&[c, !c]).await?;
        self.expect_ack(self.t.reply, doing).await
    }

    /// Sync: `0x7F` → `ACK`. A bootloader that is already synced takes the `0x7F` as a command
    /// byte: it answers `NACK` at once, or waits for the complement and `NACK`s the next
    /// `0x7F`. Either way a `GET` that works proves the session is usable.
    pub async fn sync(&mut self, attempts: u32) -> Result<Synced> {
        for _ in 0..attempts.max(1) {
            self.port.discard_input().await?;
            self.port.write_all(&[SYNC]).await?;
            match self.answer(self.t.sync, "sync").await {
                Ok(true) => return Ok(Synced::Fresh),
                Ok(false) => {
                    if self.get().await.is_ok() {
                        return Ok(Synced::Already);
                    }
                }
                // Silence, or a stray byte: send the sync byte again.
                Err(e) if e.kind() == Kind::Timeout || e.kind() == Kind::Transport => {}
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            Kind::Timeout,
            Transport::P2p,
            Component::NorthbridgeUart,
            format!(
                "ROM bootloader: no ACK to 0x7F in {} attempts at 8E1",
                attempts.max(1)
            ),
        ))
    }

    /// `GET` (0x00): the bootloader version and the commands it supports.
    pub async fn get(&mut self) -> Result<BootloaderInfo> {
        self.command(cmd::GET, "GET").await?;
        let n = read_exact(self.port, 1, self.t.reply, "ROM bootloader: GET").await?[0] as usize;
        let body = read_exact(self.port, n + 1, self.t.reply, "ROM bootloader: GET").await?;
        self.expect_ack(self.t.reply, "GET").await?;
        Ok(BootloaderInfo {
            version: body[0],
            commands: body[1..].to_vec(),
        })
    }

    /// `GET_ID` (0x02): the product ID.
    pub async fn get_id(&mut self) -> Result<u16> {
        self.command(cmd::GET_ID, "GET_ID").await?;
        let n = read_exact(self.port, 1, self.t.reply, "ROM bootloader: GET_ID").await?[0] as usize;
        let id = read_exact(self.port, n + 1, self.t.reply, "ROM bootloader: GET_ID").await?;
        self.expect_ack(self.t.reply, "GET_ID").await?;
        match id.as_slice() {
            [hi, lo] => Ok(u16::from_be_bytes([*hi, *lo])),
            other => Err(protocol("GET_ID", format!("{} B of PID", other.len()))),
        }
    }

    /// Read Memory (0x11): `len` bytes from `addr`, `1..=256`.
    pub async fn read_memory(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        if !(1..=MAX_BLOCK).contains(&len) {
            return Err(protocol(
                "Read Memory",
                format!("length {len} not in 1..=256"),
            ));
        }
        let doing = format!("Read Memory {addr:#010x}+{len}");
        self.command(cmd::READ_MEMORY, &doing).await?;
        self.port.write_all(&address_frame(addr)).await?;
        self.expect_ack(self.t.reply, &doing).await?;
        let n = (len - 1) as u8;
        self.port.write_all(&[n, !n]).await?;
        self.expect_ack(self.t.reply, &doing).await?;
        read_exact(
            self.port,
            len,
            self.t.reply,
            &format!("ROM bootloader: {doing}"),
        )
        .await
    }

    /// Write Memory (0x31): `data` at `addr`, `1..=256` bytes. The STM32WL programs double
    /// words, so the caller keeps `addr` and the length multiples of 8.
    pub async fn write_memory(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        if !(1..=MAX_BLOCK).contains(&data.len()) {
            return Err(protocol(
                "Write Memory",
                format!("length {} not in 1..=256", data.len()),
            ));
        }
        let doing = format!("Write Memory {addr:#010x}+{}", data.len());
        self.command(cmd::WRITE_MEMORY, &doing).await?;
        self.port.write_all(&address_frame(addr)).await?;
        self.expect_ack(self.t.reply, &doing).await?;
        let mut frame = Vec::with_capacity(data.len() + 2);
        frame.push((data.len() - 1) as u8);
        frame.extend_from_slice(data);
        frame.push(xor(&frame));
        self.port.write_all(&frame).await?;
        self.expect_ack(self.t.write, &doing).await
    }

    /// Extended Erase (0x44) of `pages` (numbered from 0 at the flash base): `N-1` as a u16,
    /// the pages as u16, all big-endian, then the XOR of all of them. The `ACK` comes after
    /// the erase, so its timeout grows with the page count.
    pub async fn extended_erase(&mut self, pages: &[u16]) -> Result<()> {
        // 0xFFF0.. are the special mass/bank erases; never sent from here.
        if pages.is_empty() || pages.len() > 0xFFF0 {
            return Err(protocol("Extended Erase", format!("{} pages", pages.len())));
        }
        let doing = format!("Extended Erase of {} pages", pages.len());
        self.command(cmd::EXTENDED_ERASE, &doing).await?;
        let mut frame = Vec::with_capacity(2 * pages.len() + 3);
        frame.extend_from_slice(&((pages.len() - 1) as u16).to_be_bytes());
        for p in pages {
            frame.extend_from_slice(&p.to_be_bytes());
        }
        frame.push(xor(&frame));
        self.port.write_all(&frame).await?;
        let timeout = self.t.erase_base + self.t.erase_per_page * pages.len() as u32;
        self.expect_ack(timeout, &doing).await
    }

    /// Get Checksum (0xA1): the CRC ([`stm32_crc32`]) of `len` bytes from `addr`, both sent as
    /// 4 B big-endian plus XOR; the answer is `ACK`, then the CRC as 4 B big-endian plus their
    /// XOR. `Ok(None)` when the bootloader `NACK`s the command, its address or its size: it
    /// does not do Get Checksum here, and the caller verifies by reading back instead. The
    /// bench bootloader v3.3 lists the command and `NACK`s the size (U5, 2026-10-09).
    pub async fn get_checksum(&mut self, addr: u32, len: u32) -> Result<Option<u32>> {
        let doing = format!("Get Checksum {addr:#010x}+{len}");
        self.port
            .write_all(&[cmd::GET_CHECKSUM, !cmd::GET_CHECKSUM])
            .await?;
        if !self.answer(self.t.reply, &doing).await? {
            return Ok(None);
        }
        self.port.write_all(&address_frame(addr)).await?;
        if !self.answer(self.t.reply, &doing).await? {
            return Ok(None);
        }
        self.port.write_all(&address_frame(len)).await?;
        if !self.answer(self.t.checksum, &doing).await? {
            return Ok(None);
        }
        let r = read_exact(
            self.port,
            5,
            self.t.checksum,
            &format!("ROM bootloader: {doing}"),
        )
        .await?;
        if xor(&r[..4]) != r[4] {
            return Err(protocol(&doing, "the CRC's XOR does not match"));
        }
        Ok(Some(u32::from_be_bytes([r[0], r[1], r[2], r[3]])))
    }

    /// Go (0x21): jump to `addr`. After the second `ACK` the bootloader is gone.
    pub async fn go(&mut self, addr: u32) -> Result<()> {
        let doing = format!("Go {addr:#010x}");
        self.command(cmd::GO, &doing).await?;
        self.port.write_all(&address_frame(addr)).await?;
        self.expect_ack(self.t.reply, &doing).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference vector for the STM32 CRC unit's reset configuration: the single word
    /// 0x12345678 gives 0xDF8A8A2B (RM0453 CRC unit, ST's own examples).
    #[test]
    fn stm32_crc32_of_the_reference_word() {
        assert_eq!(stm32_crc32(&0x1234_5678u32.to_le_bytes()), 0xDF8A_8A2B);
    }

    /// CRC-32/MPEG-2 (same polynomial and init, byte-wise MSB first), for the cross-check.
    fn mpeg2(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= u32::from(b) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// Word-wise over little-endian words equals CRC-32/MPEG-2 over each word's bytes
    /// reversed: that pins the byte order inside a word.
    #[test]
    fn stm32_crc32_is_mpeg2_over_byte_swapped_words() {
        assert_eq!(mpeg2(b"123456789"), 0x0376_E6E7, "the MPEG-2 check value");
        let data: Vec<u8> = (0u8..64).map(|i| i.wrapping_mul(37) ^ 0x5A).collect();
        let swapped: Vec<u8> = data
            .chunks(4)
            .flat_map(|w| w.iter().rev().copied())
            .collect();
        assert_eq!(stm32_crc32(&data), mpeg2(&swapped));
        assert_eq!(stm32_crc32(&[]), 0xFFFF_FFFF);
        // A partial word reads as 0xFF-padded flash.
        assert_eq!(
            stm32_crc32(&[1, 2, 3, 4, 5]),
            stm32_crc32(&[1, 2, 3, 4, 5, 0xFF, 0xFF, 0xFF])
        );
    }

    #[test]
    fn an_address_frame_is_big_endian_with_its_xor() {
        assert_eq!(address_frame(0x0800_0800), [0x08, 0x00, 0x08, 0x00, 0x00]);
        assert_eq!(
            address_frame(0x0803_B800),
            [0x08, 0x03, 0xB8, 0x00, 0x08 ^ 0x03 ^ 0xB8]
        );
    }
}
