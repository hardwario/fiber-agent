//! The Northbridge firmware update — `plan/control/radio/northbridge_update.md` §3.3, §3.6
//! and §10 (Contracts).
//!
//! The Northbridge has no reset or BOOT0 line from the Hub, so it enters the STM32 ROM
//! bootloader on request: `ENTER_BOOTLOADER` over HDLC, after which its boot stub (`nbboot`)
//! jumps to system memory. From there [`update_northbridge`] drives AN3155 ([`an3155`]) on the
//! same port at 8E1:
//!
//! 1. `ENTER_BOOTLOADER` (skipped, or falling back to a direct sync, when the Northbridge does
//!    not answer HDLC — it may already sit in the ROM bootloader after a failed flash);
//! 2. 8E1, sync, `GET`, `GET_ID` = 0x497;
//! 3. Extended Erase of the record page 119, then the app pages 1..=118 — never the stub
//!    (page 0) or `storage` (120..);
//! 4. Write Memory of the image from 0x0800_0800 in 256 B blocks;
//! 5. verification: Get Checksum, or a full read-back where the bootloader has none;
//! 6. the 32 B `app_valid` record at 0x0803_B800, last;
//! 7. `GO` 0x0800_0000, the stub, which checks the record and starts the app;
//! 8. back to 8N1: `EVT_BOOT`, then `GET_INFO` must report the manifest's image.
//!
//! Any failure before step 6 leaves an invalid app, so the stub keeps the Northbridge in the
//! ROM bootloader across resets and the next attempt finds it there (§3.6). This module does
//! not decide *when* to update, how often to retry, or what to publish: that is the P2P
//! task's job (U4).

pub mod an3155;
pub mod image;
pub mod port;

#[cfg(test)]
pub(crate) mod sim;
#[cfg(test)]
mod tests;

use std::time::Duration;

use tokio::time::Instant;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::hdlc::{self, Frame};
use crate::p2p::modem::{self, BootEvent, Status};
use crate::p2p::modem_io::ModemInfo;

use an3155::{BootloaderInfo, RomBootloader};
pub use an3155::{Synced, Timeouts};
pub use image::{app_valid_record, load_files, Manifest};
pub use port::{FlashPort, Line, SerialFlashPort};

/// `ENTER_BOOTLOADER`'s body: exactly "NBBL" (§10).
pub const ENTER_BOOTLOADER_MAGIC: [u8; 4] = *b"NBBL";

/// `GET_INFO` with the `app_valid` tail (§10): 188 B after the status byte.
pub const GET_INFO_LEN_APP: usize = 188;

/// Pages per Extended Erase command, so each `ACK` comes within a bounded time.
const ERASE_CHUNK: usize = 32;

/// The image identity the `GET_INFO` tail reports, copied from the `app_valid` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppImageId {
    pub image_len: u32,
    pub crc32: u32,
}

/// The `GET_INFO` tail (bytes 180..188 after the status byte), `None` from an image that
/// does not send it.
pub fn app_image_from_get_info(payload: &[u8]) -> Option<AppImageId> {
    let t = payload.get(ModemInfo::LEN_ACK_CTRL..GET_INFO_LEN_APP)?;
    Some(AppImageId {
        image_len: u32::from_le_bytes([t[0], t[1], t[2], t[3]]),
        crc32: u32::from_le_bytes([t[4], t[5], t[6], t[7]]),
    })
}

/// The steps of §3.3, in order. One journal line each (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    EnterBootloader,
    Sync,
    Erase,
    Write,
    Verify,
    Record,
    Go,
    BootCheck,
}

impl Step {
    pub const fn as_str(self) -> &'static str {
        match self {
            Step::EnterBootloader => "enter-bootloader",
            Step::Sync => "sync",
            Step::Erase => "erase",
            Step::Write => "write",
            Step::Verify => "verify",
            Step::Record => "record",
            Step::Go => "go",
            Step::BootCheck => "boot-check",
        }
    }
}

/// What the procedure reports as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Started(Step),
    /// `done` of `total` units: pages for the erase, bytes for the write and a read-back.
    Advanced {
        step: Step,
        done: u32,
        total: u32,
    },
    /// Something the journal should say: a fallback taken, a retry.
    Note {
        step: Step,
        message: String,
    },
    Finished {
        step: Step,
        elapsed: Duration,
    },
    Failed {
        step: Step,
        error: String,
    },
}

/// How to verify the written image (step 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Get Checksum (0xA1); a full read-back when the bootloader does not list or `NACK`s it.
    #[default]
    Checksum,
    /// Always the full read-back.
    ReadBack,
    /// Both. For confirming the CRC model on hardware (U5): a checksum mismatch with a
    /// matching read-back says [`an3155::stm32_crc32`] is wrong, not the flash.
    Both,
}

/// How verification was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    Checksum,
    ReadBack,
    Both,
}

/// How the Northbridge got into the ROM bootloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// It answered `ENTER_BOOTLOADER`.
    Hdlc,
    /// It did not answer HDLC; the ROM bootloader answered the sync byte directly.
    Direct,
    /// The caller said it was silent; no HDLC was sent.
    Silent,
}

/// Timings. The defaults suit the real link; tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// The `RSP` to `ENTER_BOOTLOADER` or `GET_INFO`.
    pub hdlc_rsp: Duration,
    /// After `ENTER_BOOTLOADER`'s `RSP`: the reboot into the stub and the ROM bootloader.
    pub reboot_settle: Duration,
    /// After `GO`: the stub's CRC over the image and the app's boot, until `EVT_BOOT`.
    pub boot: Duration,
    /// Between a failed bootloader command and its retry.
    pub retry_pause: Duration,
    pub bootloader: Timeouts,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            hdlc_rsp: Duration::from_millis(800),
            reboot_settle: Duration::from_millis(300),
            boot: Duration::from_secs(5),
            retry_pause: Duration::from_millis(50),
            bootloader: Timeouts::default(),
        }
    }
}

/// What the caller decides about one update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The Northbridge is known not to answer HDLC (e.g. the P2P task saw it silent after a
    /// failed flash): skip `ENTER_BOOTLOADER` and sync directly.
    pub nb_silent: bool,
    pub verify: VerifyMode,
    /// Extra attempts of one bootloader command (a write block, a read, an erase chunk) after
    /// a `NACK` or a silence. A whole-procedure retry is the caller's (§3.4, U4).
    pub retries: u32,
    /// Sync bytes sent before giving up on the ROM bootloader.
    pub sync_attempts: u32,
    /// `GET_INFO`s sent after `GO` before giving up.
    pub info_attempts: u32,
    pub timing: Timing,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            nb_silent: false,
            verify: VerifyMode::default(),
            retries: 2,
            sync_attempts: 6,
            info_attempts: 3,
            timing: Timing::default(),
        }
    }
}

/// What the Northbridge reports after the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub fw: [u8; 3],
    pub git_sha: [u8; 4],
    /// The `GET_INFO` tail; `None` from a 180 B reply, which is then checked by version only.
    pub app: Option<AppImageId>,
}

/// A finished update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub entry: Entry,
    pub synced: Synced,
    /// The ROM bootloader's version from `GET`, e.g. `0x33`.
    pub bootloader_version: u8,
    pub verified: Verified,
    /// `None` when the event was missed and `GET_INFO` alone proved the boot.
    pub boot_event: Option<BootEvent>,
    pub identity: Identity,
    pub steps: Vec<(Step, Duration)>,
    pub elapsed: Duration,
}

fn fw_err(kind: Kind, msg: impl Into<String>) -> Error {
    Error::new(kind, Transport::P2p, Component::NorthbridgeFirmware, msg)
}

/// Flash `image` into the Northbridge on `port` and check that it boots it (§3.3).
///
/// `image` is checked against `manifest` (length, CRC-32, sha256) before anything is sent.
/// `port` may be left at either line setting on error; on success it is back at 8N1 with the
/// new application running. The caller owns the port: no other task may use it meanwhile.
pub async fn update_northbridge<P: FlashPort + ?Sized>(
    port: &mut P,
    image: &[u8],
    manifest: &Manifest,
    opts: &Options,
    progress: &mut (dyn FnMut(&Progress) + Send),
) -> Result<Outcome> {
    manifest.check_image(image)?;
    let start = Instant::now();
    let mut f = Flasher {
        port,
        opts,
        progress,
        steps: Vec::new(),
        seq: 0,
    };

    let t = f.begin(Step::EnterBootloader);
    let r = f.enter_bootloader().await;
    let entry = f.end(Step::EnterBootloader, t, r)?;

    let t = f.begin(Step::Sync);
    let r = f.sync(entry).await;
    let (synced, info) = f.end(Step::Sync, t, r)?;

    let t = f.begin(Step::Erase);
    let r = f.erase().await;
    f.end(Step::Erase, t, r)?;

    let t = f.begin(Step::Write);
    let r = f.write(image::APP_ADDR, image, Step::Write).await;
    f.end(Step::Write, t, r)?;

    let t = f.begin(Step::Verify);
    let r = f.verify(image, &info).await;
    let verified = f.end(Step::Verify, t, r)?;

    let t = f.begin(Step::Record);
    let r = f.record(manifest).await;
    f.end(Step::Record, t, r)?;

    let t = f.begin(Step::Go);
    let r = f.go().await;
    f.end(Step::Go, t, r)?;

    let t = f.begin(Step::BootCheck);
    let r = f.boot_check(manifest).await;
    let (boot_event, identity) = f.end(Step::BootCheck, t, r)?;

    Ok(Outcome {
        entry,
        synced,
        bootloader_version: info.version,
        verified,
        boot_event,
        identity,
        steps: f.steps,
        elapsed: start.elapsed(),
    })
}

/// What a [`probe`] heard: the `GET_INFO` answer and its `app_valid` tail, `None` from a
/// 180 B image (before 0.4.0, which has no `ENTER_BOOTLOADER` either).
pub type ProbeInfo = (ModemInfo, Option<AppImageId>);

/// `GET_INFO`s a [`probe`] sends before calling the Northbridge silent. Few on purpose: a
/// Northbridge left in the ROM bootloader hears each one as garbage before its sync byte.
const PROBE_ATTEMPTS: u32 = 2;

/// Ask the Northbridge on `port` what it runs (`northbridge_update.md` §3.4, U4): one HDLC
/// `GET_INFO` at 8N1, retried once.
///
/// `Ok(None)` when nothing answers: the Northbridge is silent, and may sit in the ROM
/// bootloader after a failed flash (§3.6), so the caller should flash with
/// [`Options::nb_silent`] rather than send it more HDLC. A refusal or a malformed answer is
/// an error. The port is left at 8N1.
pub async fn probe<P: FlashPort + ?Sized>(
    port: &mut P,
    opts: &Options,
) -> Result<Option<ProbeInfo>> {
    let mut quiet = |_: &Progress| {};
    let mut f = Flasher {
        port,
        opts,
        progress: &mut quiet,
        steps: Vec::new(),
        seq: 0,
    };
    f.port.set_line(Line::Hdlc).await?;
    f.port.discard_input().await?;
    for _ in 0..PROBE_ATTEMPTS {
        match f.hdlc_command(modem::cmd::GET_INFO, &[], &mut |_| {}).await {
            Ok(body) => {
                let status = Status::from_byte(body.first().copied().unwrap_or(5));
                if status != Status::Ok {
                    return Err(fw_err(
                        Kind::Transport,
                        format!("GET_INFO: the Northbridge answered {status:?}"),
                    ));
                }
                let payload = body.get(1..).unwrap_or(&[]);
                let info = ModemInfo::parse(payload)?;
                return Ok(Some((info, app_image_from_get_info(payload))));
            }
            Err(e) if e.kind() == Kind::Timeout => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

struct Flasher<'a, P: FlashPort + ?Sized> {
    port: &'a mut P,
    opts: &'a Options,
    progress: &'a mut (dyn FnMut(&Progress) + Send),
    steps: Vec<(Step, Duration)>,
    seq: u8,
}

impl<P: FlashPort + ?Sized> Flasher<'_, P> {
    fn begin(&mut self, step: Step) -> Instant {
        (self.progress)(&Progress::Started(step));
        Instant::now()
    }

    fn end<T>(&mut self, step: Step, t: Instant, r: Result<T>) -> Result<T> {
        match &r {
            Ok(_) => {
                let elapsed = t.elapsed();
                self.steps.push((step, elapsed));
                (self.progress)(&Progress::Finished { step, elapsed });
            }
            Err(e) => (self.progress)(&Progress::Failed {
                step,
                error: e.to_string(),
            }),
        }
        r
    }

    fn note(&mut self, step: Step, message: impl Into<String>) {
        (self.progress)(&Progress::Note {
            step,
            message: message.into(),
        });
    }

    fn bootloader(&mut self) -> RomBootloader<'_, P> {
        RomBootloader::new(&mut *self.port, self.opts.timing.bootloader)
    }

    /// Send one HDLC command and return its `RSP` body (status byte included). Frames that are
    /// not the answer are passed to `other` (an `EVT_BOOT` during the boot check).
    async fn hdlc_command(
        &mut self,
        command: u8,
        body: &[u8],
        other: &mut (dyn FnMut(&Frame) + Send),
    ) -> Result<Vec<u8>> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        self.port
            .write_all(&hdlc::encode(command, seq, body))
            .await?;
        let want = modem::RSP_FLAG | command;
        let timeout = self.opts.timing.hdlc_rsp;
        let mut decoder = hdlc::Decoder::new();
        let found = wait_frame(&mut *self.port, &mut decoder, timeout, &mut |f: &Frame| {
            let hit = f.frame_type == want && f.seq == seq;
            if !hit {
                other(f);
            }
            hit
        })
        .await?;
        found.map(|f| f.body).ok_or_else(|| {
            Error::new(
                Kind::Timeout,
                Transport::P2p,
                Component::NorthbridgeFirmware,
                format!(
                    "no RSP to HDLC command {command:#04x} within {} ms",
                    timeout.as_millis()
                ),
            )
        })
    }

    /// Step 1.
    async fn enter_bootloader(&mut self) -> Result<Entry> {
        if self.opts.nb_silent {
            self.note(
                Step::EnterBootloader,
                "the Northbridge is silent: no ENTER_BOOTLOADER, syncing directly",
            );
            return Ok(Entry::Silent);
        }
        self.port.set_line(Line::Hdlc).await?;
        self.port.discard_input().await?;
        match self
            .hdlc_command(
                modem::cmd::ENTER_BOOTLOADER,
                &ENTER_BOOTLOADER_MAGIC,
                &mut |_| {},
            )
            .await
        {
            Ok(body) => {
                let status = Status::from_byte(body.first().copied().unwrap_or(5));
                if status == Status::Unsupported {
                    return Err(fw_err(
                        Kind::Transport,
                        "the Northbridge image has no ENTER_BOOTLOADER: it needs the one-time \
                         J-Link migration to nbboot + 0.4.0 (northbridge_update.md §4)",
                    ));
                }
                // Any other refusal is a mismatch between this flasher and the image, never
                // the caller's argument: Transport, whatever the status.
                if status != Status::Ok {
                    return Err(fw_err(
                        Kind::Transport,
                        format!("ENTER_BOOTLOADER: the Northbridge answered {status:?}"),
                    ));
                }
                tokio::time::sleep(self.opts.timing.reboot_settle).await;
                Ok(Entry::Hdlc)
            }
            Err(e) if e.kind() == Kind::Timeout => {
                self.note(
                    Step::EnterBootloader,
                    "no HDLC answer: trying the ROM bootloader directly",
                );
                Ok(Entry::Direct)
            }
            Err(e) => Err(e),
        }
    }

    /// Step 2.
    async fn sync(&mut self, entry: Entry) -> Result<(Synced, BootloaderInfo)> {
        self.port.set_line(Line::Bootloader).await?;
        self.port.discard_input().await?;
        let attempts = self.opts.sync_attempts;
        let synced = match self.bootloader().sync(attempts).await {
            Ok(s) => s,
            Err(e) if entry == Entry::Hdlc => return Err(e),
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    Transport::P2p,
                    Component::NorthbridgeUart,
                    format!(
                        "the Northbridge answers neither HDLC nor the ROM bootloader: {}",
                        e.message()
                    ),
                ))
            }
        };
        if synced == Synced::Already {
            self.note(Step::Sync, "the ROM bootloader was already synced");
        }
        let info = self.retry(Step::Sync, "GET", |b| Box::pin(b.get())).await?;
        for (c, name) in [
            (an3155::cmd::READ_MEMORY, "Read Memory"),
            (an3155::cmd::GO, "Go"),
            (an3155::cmd::WRITE_MEMORY, "Write Memory"),
            (an3155::cmd::EXTENDED_ERASE, "Extended Erase"),
        ] {
            if !info.supports(c) {
                return Err(fw_err(
                    Kind::Transport,
                    format!("the ROM bootloader does not list {name} ({c:#04x})"),
                ));
            }
        }
        let pid = self
            .retry(Step::Sync, "GET_ID", |b| Box::pin(b.get_id()))
            .await?;
        if pid != an3155::PID_STM32WL5X {
            return Err(fw_err(
                Kind::Transport,
                format!(
                    "GET_ID answered {pid:#06x}, a Northbridge is {:#06x} (STM32WL5x)",
                    an3155::PID_STM32WL5X
                ),
            ));
        }
        Ok((synced, info))
    }

    /// Run one bootloader command with [`Options::retries`] extra attempts after a `NACK` or
    /// a silence. A `NACK` returns the bootloader to waiting for a command, and stray input
    /// is discarded first, so the command can be sent again as it was.
    async fn retry<T, F>(&mut self, step: Step, doing: &str, mut op: F) -> Result<T>
    where
        F: for<'b> FnMut(
            &'b mut RomBootloader<'_, P>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T>> + Send + 'b>,
        >,
    {
        let mut attempt = 0;
        loop {
            let r = {
                let mut b = RomBootloader::new(&mut *self.port, self.opts.timing.bootloader);
                op(&mut b).await
            };
            match r {
                Ok(v) => return Ok(v),
                Err(e)
                    if attempt < self.opts.retries
                        && (an3155::is_nack(&e) || e.kind() == Kind::Timeout) =>
                {
                    attempt += 1;
                    self.note(step, format!("{doing}: {}; retry {attempt}", e.message()));
                    tokio::time::sleep(self.opts.timing.retry_pause).await;
                    self.port.discard_input().await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Step 3: the record page first, so a half-erased Northbridge never holds a record
    /// (the stub would refuse the image by its CRC anyway), then the app pages.
    async fn erase(&mut self) -> Result<()> {
        let mut pages = vec![image::RECORD_PAGE];
        pages.extend(image::APP_FIRST_PAGE..=image::APP_LAST_PAGE);
        if let Some(p) = pages.iter().find(|&&p| !image::erasable(p)) {
            return Err(fw_err(
                Kind::InvalidArgument,
                format!("refusing to erase page {p}: only slot0 and the record page"),
            ));
        }
        let total = pages.len() as u32;
        let mut done = 0;
        // The record page alone, then the app pages in chunks.
        let chunks = std::iter::once(&pages[..1]).chain(pages[1..].chunks(ERASE_CHUNK));
        for chunk in chunks {
            let chunk = chunk.to_vec();
            self.retry(Step::Erase, "Extended Erase", |b| {
                let chunk = chunk.clone();
                Box::pin(async move { b.extended_erase(&chunk).await })
            })
            .await?;
            done += chunk.len() as u32;
            (self.progress)(&Progress::Advanced {
                step: Step::Erase,
                done,
                total,
            });
        }
        Ok(())
    }

    /// Step 4 (and the record of step 6): `data` at `addr` in 256 B blocks.
    async fn write(&mut self, addr: u32, data: &[u8], step: Step) -> Result<()> {
        let total = data.len() as u32;
        for (i, block) in data.chunks(an3155::MAX_BLOCK).enumerate() {
            let at = addr + (i * an3155::MAX_BLOCK) as u32;
            let block = block.to_vec();
            self.retry(step, "Write Memory", |b| {
                let block = block.clone();
                Box::pin(async move { b.write_memory(at, &block).await })
            })
            .await?;
            let done = (i * an3155::MAX_BLOCK + block.len()) as u32;
            (self.progress)(&Progress::Advanced { step, done, total });
        }
        Ok(())
    }

    /// Read `expect.len()` bytes back from `addr` and compare.
    async fn read_back(&mut self, addr: u32, expect: &[u8], step: Step) -> Result<()> {
        let total = expect.len() as u32;
        for (i, want) in expect.chunks(an3155::MAX_BLOCK).enumerate() {
            let at = addr + (i * an3155::MAX_BLOCK) as u32;
            let len = want.len();
            let got = self
                .retry(step, "Read Memory", |b| Box::pin(b.read_memory(at, len)))
                .await?;
            if let Some(k) = got.iter().zip(want).position(|(g, w)| g != w) {
                return Err(fw_err(
                    Kind::Transport,
                    format!(
                        "read-back differs at {:#010x}: flash {:#04x}, image {:#04x}",
                        at + k as u32,
                        got[k],
                        want[k]
                    ),
                ));
            }
            let done = (i * an3155::MAX_BLOCK + len) as u32;
            (self.progress)(&Progress::Advanced { step, done, total });
        }
        Ok(())
    }

    /// The Get Checksum CRC of the written app, `None` when the bootloader does not do it
    /// here.
    async fn checksum(&mut self, image: &[u8], info: &BootloaderInfo) -> Result<Option<u32>> {
        if !info.supports(an3155::cmd::GET_CHECKSUM) {
            self.note(
                Step::Verify,
                "the ROM bootloader does not list Get Checksum: verifying by read-back",
            );
            return Ok(None);
        }
        let len = image.len() as u32;
        let got = self
            .retry(Step::Verify, "Get Checksum", |b| {
                Box::pin(b.get_checksum(image::APP_ADDR, len))
            })
            .await?;
        if got.is_none() {
            self.note(
                Step::Verify,
                "the ROM bootloader NACKed Get Checksum: verifying by read-back",
            );
        }
        Ok(got)
    }

    /// Step 5.
    async fn verify(&mut self, image: &[u8], info: &BootloaderInfo) -> Result<Verified> {
        let want = an3155::stm32_crc32(image);
        let mismatch = |got: u32| {
            fw_err(
                Kind::Transport,
                format!(
                    "Get Checksum over the written app: Northbridge {got:#010x}, image \
                     {want:#010x}"
                ),
            )
        };
        match self.opts.verify {
            VerifyMode::ReadBack => {
                self.read_back(image::APP_ADDR, image, Step::Verify).await?;
                Ok(Verified::ReadBack)
            }
            VerifyMode::Checksum => match self.checksum(image, info).await? {
                Some(got) if got == want => Ok(Verified::Checksum),
                Some(got) => Err(mismatch(got)),
                None => {
                    self.read_back(image::APP_ADDR, image, Step::Verify).await?;
                    Ok(Verified::ReadBack)
                }
            },
            VerifyMode::Both => {
                let crc = self.checksum(image, info).await?;
                self.read_back(image::APP_ADDR, image, Step::Verify).await?;
                match crc {
                    Some(got) if got == want => Ok(Verified::Both),
                    Some(got) => Err(fw_err(
                        Kind::Transport,
                        format!(
                            "the flash matches the image byte for byte, but Get Checksum \
                             answered {got:#010x} where stm32_crc32 gives {want:#010x}: the \
                             CRC model is wrong, not the flash"
                        ),
                    )),
                    None => Ok(Verified::ReadBack),
                }
            }
        }
    }

    /// Step 6: the record, written last and read back.
    async fn record(&mut self, manifest: &Manifest) -> Result<()> {
        let rec = app_valid_record(manifest);
        self.write(image::RECORD_ADDR, &rec, Step::Record).await?;
        self.read_back(image::RECORD_ADDR, &rec, Step::Record).await
    }

    /// Step 7. Not retried: after the address `ACK` the bootloader is gone, and a silence
    /// may mean it already jumped. The boot check tells.
    async fn go(&mut self) -> Result<()> {
        self.bootloader().go(image::STUB_ADDR).await
    }

    /// Step 8: 8N1, `EVT_BOOT`, then `GET_INFO` against the manifest.
    async fn boot_check(&mut self, m: &Manifest) -> Result<(Option<BootEvent>, Identity)> {
        // No discard here: the application may already be talking, and the HDLC decoder
        // skips whatever the line switch garbled.
        self.port.set_line(Line::Hdlc).await?;
        let mut decoder = hdlc::Decoder::new();
        let boot = wait_frame(
            &mut *self.port,
            &mut decoder,
            self.opts.timing.boot,
            &mut |f: &Frame| f.frame_type == modem::evt::BOOT,
        )
        .await?
        .and_then(|f| BootEvent::decode(&f.body));
        let mut boot_event = boot;
        if boot_event.is_none() {
            self.note(Step::BootCheck, "no EVT_BOOT: asking GET_INFO directly");
        }

        let mut last = None;
        let mut payload = None;
        for _ in 0..self.opts.info_attempts.max(1) {
            let mut seen_boot = None;
            let r = self
                .hdlc_command(modem::cmd::GET_INFO, &[], &mut |f: &Frame| {
                    if f.frame_type == modem::evt::BOOT {
                        seen_boot = BootEvent::decode(&f.body);
                    }
                })
                .await;
            boot_event = boot_event.or(seen_boot);
            match r {
                Ok(body) => {
                    let status = Status::from_byte(body.first().copied().unwrap_or(5));
                    if status != Status::Ok {
                        return Err(fw_err(
                            Kind::Transport,
                            format!(
                                "GET_INFO after the update: the Northbridge answered {status:?}"
                            ),
                        ));
                    }
                    payload = Some(body.get(1..).unwrap_or(&[]).to_vec());
                    break;
                }
                Err(e) if e.kind() == Kind::Timeout => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        let Some(payload) = payload else {
            return Err(fw_err(
                Kind::Timeout,
                format!(
                    "the Northbridge did not come back after GO (the stub may have refused \
                     the image and stayed in the ROM bootloader): {}",
                    last.map(|e| e.message().to_string()).unwrap_or_default()
                ),
            ));
        };
        let info = ModemInfo::parse(&payload)?;
        let identity = Identity {
            fw: info.fw,
            git_sha: info.git_sha,
            app: app_image_from_get_info(&payload),
        };
        let want = (m.fw_version, m.git_sha_bytes());
        let shown = |fw: [u8; 3], sha: [u8; 4]| {
            format!(
                "{}.{}.{}+{}",
                fw[0],
                fw[1],
                fw[2],
                sha.iter().map(|b| format!("{b:02x}")).collect::<String>()
            )
        };
        if (identity.fw, identity.git_sha) != want {
            return Err(fw_err(
                Kind::Transport,
                format!(
                    "the Northbridge boots {} but the manifest is {}",
                    shown(identity.fw, identity.git_sha),
                    m.version_string()
                ),
            ));
        }
        if let Some(b) = boot_event {
            if (b.fw_ver, b.git_sha) != want {
                return Err(fw_err(
                    Kind::Transport,
                    format!(
                        "EVT_BOOT reports {} but the manifest is {}",
                        shown(b.fw_ver, b.git_sha),
                        m.version_string()
                    ),
                ));
            }
        }
        if let Some(app) = identity.app {
            if app.image_len != m.image_len || app.crc32 != m.crc32 {
                return Err(fw_err(
                    Kind::Transport,
                    format!(
                        "GET_INFO reports image {} B crc32 {:#010x}, the manifest {} B \
                         {:#010x}",
                        app.image_len, app.crc32, m.image_len, m.crc32
                    ),
                ));
            }
        } else {
            self.note(
                Step::BootCheck,
                "GET_INFO has no app_valid tail: checked by fw_ver + git_sha only",
            );
        }
        Ok((boot_event, identity))
    }
}

/// Read HDLC frames until `want` accepts one, or `timeout` passes (`Ok(None)`).
async fn wait_frame<P: FlashPort + ?Sized>(
    port: &mut P,
    decoder: &mut hdlc::Decoder,
    timeout: Duration,
    want: &mut (dyn FnMut(&Frame) -> bool + Send),
) -> Result<Option<Frame>> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 512];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let n = port.read(&mut buf, remaining).await?;
        if n == 0 {
            return Ok(None);
        }
        for f in decoder.push(&buf[..n]) {
            if want(&f) {
                return Ok(Some(f));
            }
        }
    }
}
