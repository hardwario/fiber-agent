//! A simulated Northbridge for the flasher's tests: the HDLC application, the boot stub and
//! an STM32WL ROM bootloader (AN3155) over an in-memory 256 KB flash.
//!
//! It is a [`FlashPort`]: what the host writes is processed at once and the answer queued, so
//! a read with nothing queued is a timeout without waiting. The bootloader only hears the host
//! at 8E1 and the application only at 8N1; bytes at the wrong setting are counted and lost,
//! as a parity mismatch loses them on the wire.

use std::collections::VecDeque;
use std::time::Duration;

use crate::error::Result;
use crate::p2p::hdlc;
use crate::p2p::modem::{self, BootEvent};
use crate::p2p::modem_io::{AckCtrlCounters, ModemInfo};

use super::an3155::{self, cmd, stm32_crc32, xor, ACK, NACK};
use super::image::{self, crc32, parse_record, FLASH_BASE, FLASH_SIZE, PAGE_SIZE};
use super::port::{FlashPort, Line};
use super::ENTER_BOOTLOADER_MAGIC;

/// How the bootloader treats Get Checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumSupport {
    /// Listed by `GET` and answered (the bench bootloader v3.3, §9).
    Yes,
    /// Listed, but `NACK`ed.
    Nack,
    /// Not listed, and `NACK`ed.
    Absent,
    /// Listed, the command and the address `ACK`ed, the size `NACK`ed: what the bench
    /// bootloader v3.3 did with the app's length on 2026-10-09 (U5).
    NackSize,
}

/// How an already-synced bootloader answers another `0x7F`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncedSync {
    /// `NACK` at once.
    Nack,
    /// Takes it as a command byte and waits for the complement.
    WaitComplement,
}

/// How the application answers `ENTER_BOOTLOADER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterBl {
    Supported,
    /// A 0.3.x image: `UNSUPPORTED`.
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct SimConfig {
    pub pid: u16,
    pub checksum: ChecksumSupport,
    pub synced_sync: SyncedSync,
    pub enter_bl: EnterBl,
    /// `NACK` this many Write Memory data phases, writing nothing.
    pub nack_writes: u32,
    /// `NACK` this many Extended Erase commands, erasing nothing.
    pub nack_erases: u32,
    /// Program this address with one bit flipped: a bad flash cell.
    pub corrupt_at: Option<u32>,
    /// `GET_INFO` length after the status byte: 180 or 188.
    pub info_len: usize,
    /// What the booted app reports instead of its record's version.
    pub report_fw: Option<[u8; 3]>,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            pid: an3155::PID_STM32WL5X,
            checksum: ChecksumSupport::Yes,
            synced_sync: SyncedSync::Nack,
            enter_bl: EnterBl::Supported,
            nack_writes: 0,
            nack_erases: 0,
            corrupt_at: None,
            info_len: 188,
            report_fw: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Unsynced,
    Idle,
    Addr(u8),
    ReadLen(u32),
    WriteData(u32),
    Erase,
    ChecksumSize(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    App,
    Bootloader(Stage),
}

/// The fill of the stub page and of `storage`, to prove the flasher never touches them.
pub const STUB_FILL: u8 = 0xB5;
pub const STORAGE_FILL: u8 = 0x5A;

pub struct SimNb {
    pub cfg: SimConfig,
    pub flash: Vec<u8>,
    mode: Mode,
    line: Line,
    out: VecDeque<u8>,
    inbuf: Vec<u8>,
    decoder: hdlc::Decoder,
    /// `GO` started the app; its `EVT_BOOT` goes out on the next read at 8N1.
    booting: bool,
    // What happened, for the assertions.
    pub host_bytes: usize,
    pub wrong_line_bytes: usize,
    pub line_changes: Vec<Line>,
    pub hdlc_commands: Vec<u8>,
    pub erased: Vec<u16>,
    pub writes: Vec<(u32, usize)>,
    pub gos: Vec<u32>,
    pub checksums: u32,
}

impl SimNb {
    fn blank(cfg: SimConfig, mode: Mode) -> Self {
        let mut flash = vec![0xFFu8; FLASH_SIZE as usize];
        let page = PAGE_SIZE as usize;
        flash[..page].fill(STUB_FILL);
        flash[image::STORAGE_FIRST_PAGE as usize * page..].fill(STORAGE_FILL);
        SimNb {
            cfg,
            flash,
            mode,
            line: Line::Hdlc,
            out: VecDeque::new(),
            inbuf: Vec::new(),
            decoder: hdlc::Decoder::new(),
            booting: false,
            host_bytes: 0,
            wrong_line_bytes: 0,
            line_changes: Vec::new(),
            hdlc_commands: Vec::new(),
            erased: Vec::new(),
            writes: Vec::new(),
            gos: Vec::new(),
            checksums: 0,
        }
    }

    /// A Northbridge running `old` (valid record), on the HDLC link.
    pub fn running(cfg: SimConfig, old: &[u8], m: &image::Manifest) -> Self {
        let mut s = SimNb::blank(cfg, Mode::App);
        let at = (image::APP_ADDR - FLASH_BASE) as usize;
        s.flash[at..at + old.len()].copy_from_slice(old);
        let r = (image::RECORD_ADDR - FLASH_BASE) as usize;
        s.flash[r..r + image::RECORD_LEN].copy_from_slice(&image::app_valid_record(m));
        assert!(s.app_valid().is_some(), "the old image is valid");
        s
    }

    /// A Northbridge sitting in the ROM bootloader: no valid app (a failed flash).
    pub fn in_bootloader(cfg: SimConfig, synced: bool) -> Self {
        let stage = if synced { Stage::Idle } else { Stage::Unsynced };
        SimNb::blank(cfg, Mode::Bootloader(stage))
    }

    pub fn region(&self, addr: u32, len: usize) -> &[u8] {
        let at = (addr - FLASH_BASE) as usize;
        &self.flash[at..at + len]
    }

    pub fn in_rom_bootloader(&self) -> bool {
        matches!(self.mode, Mode::Bootloader(_))
    }

    /// The stub's check (§3.1, §10).
    fn app_valid(&self) -> Option<(u32, u32)> {
        let rec = self.region(image::RECORD_ADDR, image::RECORD_LEN);
        let (len, crc) = parse_record(rec)?;
        (crc32(self.region(image::APP_ADDR, len as usize)) == crc).then_some((len, crc))
    }

    /// The running app's identity, from its record.
    fn app_identity(&self) -> ([u8; 3], [u8; 4], u32, u32) {
        let rec = self.region(image::RECORD_ADDR, image::RECORD_LEN);
        let fw = self.cfg.report_fw.unwrap_or([rec[16], rec[17], rec[18]]);
        let sha = u32::from_le_bytes([rec[20], rec[21], rec[22], rec[23]]).to_be_bytes();
        let (len, crc) = self.app_valid().unwrap_or((0, 0));
        (fw, sha, len, crc)
    }

    fn send_frame(&mut self, frame_type: u8, seq: u8, body: &[u8]) {
        self.out.extend(hdlc::encode(frame_type, seq, body));
    }

    fn app_feed(&mut self, bytes: &[u8]) {
        for f in self.decoder.push(bytes) {
            self.hdlc_commands.push(f.frame_type);
            let rsp = modem::RSP_FLAG | f.frame_type;
            match f.frame_type {
                modem::cmd::ENTER_BOOTLOADER => match self.cfg.enter_bl {
                    EnterBl::Unsupported => self.send_frame(rsp, f.seq, &[8]),
                    EnterBl::Supported if f.body != ENTER_BOOTLOADER_MAGIC => {
                        self.send_frame(rsp, f.seq, &[1])
                    }
                    EnterBl::Supported => {
                        self.send_frame(rsp, f.seq, &[0]);
                        // Request word set, cold reboot, the stub jumps to system memory.
                        self.mode = Mode::Bootloader(Stage::Unsynced);
                        self.decoder = hdlc::Decoder::new();
                        return;
                    }
                },
                modem::cmd::GET_INFO => {
                    let (fw, git_sha, len, crc) = self.app_identity();
                    let info = ModemInfo {
                        fw,
                        git_sha,
                        evt_head_seq: Some(1),
                        ack_ctrl: Some(AckCtrlCounters::default()),
                        ..ModemInfo::default()
                    };
                    let mut body = vec![0u8];
                    body.extend(info.encode());
                    if self.cfg.info_len >= super::GET_INFO_LEN_APP {
                        body.extend_from_slice(&len.to_le_bytes());
                        body.extend_from_slice(&crc.to_le_bytes());
                    }
                    self.send_frame(rsp, f.seq, &body);
                }
                _ => {}
            }
        }
    }

    fn ack(&mut self) {
        self.out.push_back(ACK);
    }

    fn nack(&mut self) {
        self.out.push_back(NACK);
        self.mode = Mode::Bootloader(Stage::Idle);
    }

    fn in_flash(addr: u32, len: usize) -> bool {
        addr >= FLASH_BASE && (addr - FLASH_BASE) as usize + len <= FLASH_SIZE as usize
    }

    /// Process whatever complete pieces of AN3155 the input holds.
    fn bl_feed(&mut self) {
        loop {
            let Mode::Bootloader(stage) = self.mode else {
                self.inbuf.clear();
                return;
            };
            let buf = &self.inbuf;
            let need = match stage {
                Stage::Unsynced | Stage::Idle => 1,
                Stage::Addr(_) | Stage::ChecksumSize(_) => 5,
                Stage::ReadLen(_) => 2,
                Stage::WriteData(_) => buf.first().map_or(1, |&n| n as usize + 3),
                Stage::Erase => match buf.get(..2) {
                    Some(n) => 2 + 2 * (u16::from_be_bytes([n[0], n[1]]) as usize + 1) + 1,
                    None => 2,
                },
            };
            if buf.len() < need {
                return;
            }
            match stage {
                Stage::Unsynced => {
                    if self.inbuf.remove(0) == an3155::SYNC {
                        self.ack();
                        self.mode = Mode::Bootloader(Stage::Idle);
                    }
                }
                Stage::Idle => {
                    if self.inbuf[0] == an3155::SYNC && self.cfg.synced_sync == SyncedSync::Nack {
                        self.inbuf.remove(0);
                        self.nack();
                        continue;
                    }
                    if self.inbuf.len() < 2 {
                        return;
                    }
                    let c: Vec<u8> = self.inbuf.drain(..2).collect();
                    if c[1] != !c[0] {
                        self.nack();
                        continue;
                    }
                    self.command(c[0]);
                }
                Stage::Addr(c) => {
                    let a: Vec<u8> = self.inbuf.drain(..5).collect();
                    let addr = u32::from_be_bytes([a[0], a[1], a[2], a[3]]);
                    if xor(&a[..4]) != a[4] || !SimNb::in_flash(addr, 1) {
                        self.nack();
                        continue;
                    }
                    self.ack();
                    self.mode = Mode::Bootloader(match c {
                        cmd::READ_MEMORY => Stage::ReadLen(addr),
                        cmd::WRITE_MEMORY => Stage::WriteData(addr),
                        cmd::GET_CHECKSUM => Stage::ChecksumSize(addr),
                        _ => {
                            self.go(addr);
                            continue;
                        }
                    });
                }
                Stage::ReadLen(addr) => {
                    let n: Vec<u8> = self.inbuf.drain(..2).collect();
                    let len = n[0] as usize + 1;
                    if n[1] != !n[0] || !SimNb::in_flash(addr, len) {
                        self.nack();
                        continue;
                    }
                    self.ack();
                    let data = self.region(addr, len).to_vec();
                    self.out.extend(data);
                    self.mode = Mode::Bootloader(Stage::Idle);
                }
                Stage::WriteData(addr) => {
                    let frame: Vec<u8> = self.inbuf.drain(..need).collect();
                    let (body, sum) = frame.split_at(frame.len() - 1);
                    let data = &body[1..];
                    let at = (addr - FLASH_BASE) as usize;
                    let ok = xor(body) == sum[0]
                        && addr.is_multiple_of(8)
                        && data.len().is_multiple_of(8)
                        && SimNb::in_flash(addr, data.len())
                        // STM32WL: a double word is programmed only when erased.
                        && self.flash[at..at + data.len()].iter().all(|&b| b == 0xFF);
                    if !ok {
                        self.nack();
                        continue;
                    }
                    if self.cfg.nack_writes > 0 {
                        self.cfg.nack_writes -= 1;
                        self.nack();
                        continue;
                    }
                    self.flash[at..at + data.len()].copy_from_slice(data);
                    if let Some(c) = self.cfg.corrupt_at {
                        if (addr..addr + data.len() as u32).contains(&c) {
                            self.flash[(c - FLASH_BASE) as usize] ^= 0x01;
                        }
                    }
                    self.writes.push((addr, data.len()));
                    self.ack();
                    self.mode = Mode::Bootloader(Stage::Idle);
                }
                Stage::Erase => {
                    let frame: Vec<u8> = self.inbuf.drain(..need).collect();
                    let (body, sum) = frame.split_at(frame.len() - 1);
                    let pages: Vec<u16> = body[2..]
                        .chunks(2)
                        .map(|p| u16::from_be_bytes([p[0], p[1]]))
                        .collect();
                    let max_page = (FLASH_SIZE / PAGE_SIZE) as u16;
                    if xor(body) != sum[0] || pages.iter().any(|&p| p >= max_page) {
                        self.nack();
                        continue;
                    }
                    if self.cfg.nack_erases > 0 {
                        self.cfg.nack_erases -= 1;
                        self.nack();
                        continue;
                    }
                    for p in pages {
                        let at = p as usize * PAGE_SIZE as usize;
                        self.flash[at..at + PAGE_SIZE as usize].fill(0xFF);
                        self.erased.push(p);
                    }
                    self.ack();
                    self.mode = Mode::Bootloader(Stage::Idle);
                }
                Stage::ChecksumSize(addr) => {
                    let s: Vec<u8> = self.inbuf.drain(..5).collect();
                    let size = u32::from_be_bytes([s[0], s[1], s[2], s[3]]);
                    if xor(&s[..4]) != s[4]
                        || self.cfg.checksum == ChecksumSupport::NackSize
                        || !size.is_multiple_of(4)
                        || !SimNb::in_flash(addr, size as usize)
                    {
                        self.nack();
                        continue;
                    }
                    self.checksums += 1;
                    self.ack();
                    let crc = stm32_crc32(self.region(addr, size as usize)).to_be_bytes();
                    self.out.extend(crc);
                    self.out.push_back(xor(&crc));
                    self.mode = Mode::Bootloader(Stage::Idle);
                }
            }
        }
    }

    fn command(&mut self, c: u8) {
        match c {
            cmd::GET => {
                let mut cmds = vec![
                    0x00, 0x01, 0x02, 0x11, 0x21, 0x31, 0x44, 0x63, 0x73, 0x82, 0x92,
                ];
                if self.cfg.checksum != ChecksumSupport::Absent {
                    cmds.push(cmd::GET_CHECKSUM);
                }
                self.ack();
                self.out.push_back(cmds.len() as u8);
                self.out.push_back(0x33);
                self.out.extend(cmds);
                self.ack();
            }
            cmd::GET_ID => {
                self.ack();
                self.out.push_back(1);
                self.out.extend(self.cfg.pid.to_be_bytes());
                self.ack();
            }
            cmd::READ_MEMORY | cmd::WRITE_MEMORY | cmd::GO => {
                self.ack();
                self.mode = Mode::Bootloader(Stage::Addr(c));
            }
            cmd::GET_CHECKSUM if self.cfg.checksum == ChecksumSupport::Yes => {
                self.ack();
                self.mode = Mode::Bootloader(Stage::Addr(c));
            }
            cmd::EXTENDED_ERASE => {
                self.ack();
                self.mode = Mode::Bootloader(Stage::Erase);
            }
            _ => self.nack(),
        }
    }

    /// `GO`: the stub at the flash base checks the record and the image CRC; a valid app
    /// boots, anything else lands in a fresh ROM bootloader.
    fn go(&mut self, addr: u32) {
        self.gos.push(addr);
        if addr == image::STUB_ADDR && self.app_valid().is_some() {
            self.mode = Mode::App;
            self.decoder = hdlc::Decoder::new();
            self.booting = true;
        } else {
            self.mode = Mode::Bootloader(Stage::Unsynced);
        }
    }
}

#[async_trait::async_trait]
impl FlashPort for SimNb {
    async fn set_line(&mut self, line: Line) -> Result<()> {
        self.line = line;
        self.line_changes.push(line);
        Ok(())
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.host_bytes += bytes.len();
        match (self.mode, self.line) {
            (Mode::App, Line::Hdlc) => self.app_feed(bytes),
            (Mode::Bootloader(_), Line::Bootloader) => {
                self.inbuf.extend_from_slice(bytes);
                self.bl_feed();
            }
            _ => self.wrong_line_bytes += bytes.len(),
        }
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        if self.booting && self.line == Line::Hdlc {
            self.booting = false;
            let (fw, git_sha, _, _) = self.app_identity();
            let boot = BootEvent {
                fw_ver: fw,
                git_sha,
                reset_cause: 0x02,
            };
            self.send_frame(modem::evt::BOOT, 0, &boot.encode());
        }
        let n = buf.len().min(self.out.len());
        for (b, o) in buf.iter_mut().zip(self.out.drain(..n)) {
            *b = o;
        }
        Ok(n)
    }

    async fn discard_input(&mut self) -> Result<()> {
        self.out.clear();
        Ok(())
    }
}
