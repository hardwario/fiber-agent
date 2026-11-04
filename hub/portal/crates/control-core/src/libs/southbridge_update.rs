//! Southbridge (SB) firmware update through the Hub — plan/system/southbridge_update.md.
//!
//! The SB (STM32C031C6) sits on `ttyAMA4`; CM4 GPIO7 drives its NRST and GPIO26 its BOOT0.
//! The image and its manifest ride in the rootfs (`/usr/share/proximos/southbridge/`,
//! recipes-bsp/southbridge-firmware). At every start of `fiber_app` the SB is asked for
//! `VERSION`; when the answer does not name the bundled image, BOOT0 is raised, the SB reset
//! into the ST ROM bootloader and the image written over AN3155, reusing the Northbridge
//! flasher's `RomBootloader` (`control_radio::p2p::nb_flash`). A failure never stops
//! `fiber_app`: it starts with whatever SB answers.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use control_radio::p2p::nb_flash::an3155::{cmd, RomBootloader, MAX_BLOCK};
use control_radio::p2p::nb_flash::{FlashPort, Line, SerialFlashPort, Timeouts};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::drivers::StmBridge;

pub const PKG_DIR: &str = "/usr/share/proximos/southbridge";
pub const MANIFEST_NAME: &str = "southbridge.json";
pub const IMAGE_NAME: &str = "southbridge.bin";
pub const STATE_PATH: &str = "/data/fiber/southbridge/update-state.json";
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// CM4 GPIO driving the SB's NRST (the pulse `StmBridge` has always given).
pub const NRST_GPIO: u8 = 7;
/// CM4 GPIO driving the SB's BOOT0; needs option byte nBOOT_SEL = 0 (§2).
pub const BOOT0_GPIO: u8 = 26;

pub const MANIFEST_FORMAT: u32 = 1;
pub const BOARD: &str = "fiber_southbridge";
pub const FLASH_BASE: u32 = 0x0800_0000;
pub const FLASH_SIZE: usize = 32 * 1024;
pub const PAGE_SIZE: usize = 2048;
/// STM32C031 product ID, as GET_ID reports it (§3.3 step 2).
pub const PID_STM32C031: u16 = 0x0453;
/// Attempts per (manifest sha256, Hub boot) — §3.4.
pub const MAX_ATTEMPTS: u32 = 3;

/// The port the SB was brought up on, for the signed update (§3.7).
static PORT: OnceLock<(String, u32)> = OnceLock::new();
/// The SB's last `VERSION` answer without the prefix, for `system/info.southbridge` (§3.5).
static RUNNING: Mutex<Option<String>> = Mutex::new(None);
/// A signed update is flashing right now.
static BUSY: AtomicBool = AtomicBool::new(false);

const RESET_PULSE: Duration = Duration::from_millis(50);
const BOOTLOADER_SETTLE: Duration = Duration::from_millis(100);
const SYNC_ATTEMPTS: u32 = 5;
/// Reads of one block during verify before the attempt fails.
const VERIFY_READ_TRIES: u32 = 3;
const VERIFY_RETRY_PAUSE: Duration = Duration::from_millis(100);

/// `southbridge.json`, written by firmware/fiber-southbridge/tools/sb_package.py (§3.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub board: String,
    pub load_addr: u64,
    pub fw_version: String,
    pub git_sha: String,
    pub image_len: usize,
    pub crc32: String,
    pub sha256: String,
}

impl Manifest {
    /// What the SB answers to `VERSION` when it runs this image (§3.1), without the
    /// `VERSION: ` prefix.
    pub fn version_string(&self) -> String {
        format!("south-zephyr {} g{}", self.fw_version, self.git_sha)
    }
}

/// A checked image and its manifest.
#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: Manifest,
    pub image: Vec<u8>,
}

fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn is_sha8(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Check `image` against `manifest`: everything §3.2 promises, before BOOT0 is touched.
pub fn check_package(manifest: Manifest, image: Vec<u8>) -> Result<Package, String> {
    let mut errors = Vec::new();
    if manifest.format != MANIFEST_FORMAT {
        errors.push(format!(
            "format {} (want {MANIFEST_FORMAT})",
            manifest.format
        ));
    }
    if manifest.board != BOARD {
        errors.push(format!("board {:?} (want {BOARD:?})", manifest.board));
    }
    if manifest.load_addr != u64::from(FLASH_BASE) {
        errors.push(format!(
            "load_addr {:#x} (want {FLASH_BASE:#x})",
            manifest.load_addr
        ));
    }
    if !is_version(&manifest.fw_version) {
        errors.push(format!("fw_version {:?} is not X.Y.Z", manifest.fw_version));
    }
    if !is_sha8(&manifest.git_sha) {
        errors.push(format!(
            "git_sha {:?} is not 8 lowercase hex digits",
            manifest.git_sha
        ));
    }
    if manifest.image_len != image.len() {
        errors.push(format!(
            "image_len {} but the image has {} B",
            manifest.image_len,
            image.len()
        ));
    }
    if image.is_empty() || image.len() > FLASH_SIZE || image.len() % 8 != 0 {
        errors.push(format!(
            "image is {} B; it must be in (0, {FLASH_SIZE}] and a multiple of 8",
            image.len()
        ));
    }
    let sha = hex::encode(Sha256::digest(&image));
    if manifest.sha256.to_ascii_lowercase() != sha {
        errors.push(format!(
            "sha256 {} but the image hashes to {sha}",
            manifest.sha256
        ));
    }
    let crc = control_radio::p2p::nb_flash::image::crc32(&image);
    let want_crc = manifest
        .crc32
        .strip_prefix("0x")
        .and_then(|h| u32::from_str_radix(h, 16).ok());
    if want_crc != Some(crc) {
        errors.push(format!(
            "crc32 {} but the image has {crc:#010x}",
            manifest.crc32
        ));
    }
    if errors.is_empty() {
        Ok(Package { manifest, image })
    } else {
        Err(errors.join("; "))
    }
}

/// Read and check a package. `Ok(None)`: no manifest, so this image carries no SB firmware.
pub fn load_package(manifest_path: &Path, image_path: &Path) -> Result<Option<Package>, String> {
    let text = match std::fs::read_to_string(manifest_path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", manifest_path.display())),
    };
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let image = std::fs::read(image_path)
        .map_err(|e| format!("cannot read {}: {e}", image_path.display()))?;
    check_package(manifest, image)
        .map(Some)
        .map_err(|e| format!("{} does not match its manifest: {e}", image_path.display()))
}

/// Does the SB's answer to `VERSION` name exactly this image? `None` (no answer, e.g. an SB
/// left in the ROM bootloader or without an app) never does.
pub fn version_matches(reply: Option<&str>, manifest: &Manifest) -> bool {
    reply
        .map(|r| r.trim().strip_prefix("VERSION:").unwrap_or(r).trim())
        .is_some_and(|v| v == manifest.version_string())
}

/// `VERSION: south-zephyr 1.1.0 g…` → `south-zephyr 1.1.0 g…`.
fn strip_version(reply: &str) -> String {
    let r = reply.trim();
    r.strip_prefix("VERSION:").unwrap_or(r).trim().to_string()
}

fn note_running(reply: Option<&str>) {
    *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) =
        reply.map(strip_version).filter(|v| !v.is_empty());
}

/// The two control lines of the SB.
pub trait BootLines {
    fn set_boot0(&mut self, high: bool) -> Result<(), String>;
    fn pulse_reset(&mut self) -> Result<(), String>;
}

/// The real lines on the CM4. Both pins are held only while the update runs; `StmBridge`
/// takes NRST again afterwards.
pub struct GpioLines {
    boot0: rppal::gpio::OutputPin,
    nrst: rppal::gpio::OutputPin,
}

impl GpioLines {
    pub fn open() -> Result<Self, String> {
        let gpio = rppal::gpio::Gpio::new().map_err(|e| format!("GPIO: {e}"))?;
        let mut boot0 = gpio
            .get(BOOT0_GPIO)
            .map_err(|e| format!("GPIO{BOOT0_GPIO} (BOOT0): {e}"))?
            .into_output_low();
        // BOOT0 stays driven low after the update; a floating BOOT0 is what used to leave
        // the SB in the ROM bootloader after a power cycle (SB README).
        boot0.set_reset_on_drop(false);
        let nrst = gpio
            .get(NRST_GPIO)
            .map_err(|e| format!("GPIO{NRST_GPIO} (NRST): {e}"))?
            .into_output_high();
        Ok(GpioLines { boot0, nrst })
    }
}

impl BootLines for GpioLines {
    fn set_boot0(&mut self, high: bool) -> Result<(), String> {
        if high {
            self.boot0.set_high();
        } else {
            self.boot0.set_low();
        }
        Ok(())
    }

    fn pulse_reset(&mut self) -> Result<(), String> {
        self.nrst.set_low();
        std::thread::sleep(RESET_PULSE);
        self.nrst.set_high();
        Ok(())
    }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[sb-update] {}", msg.as_ref());
}

/// §3.3 step 4.1–4.6: put the SB into its ROM bootloader, write `image` at the flash base,
/// read it back, and give the SB back to its app. BOOT0 is low and the SB reset afterwards,
/// whatever happened. Nothing is erased before sync and GET_ID have answered.
pub async fn flash<P, L>(
    port: &mut P,
    lines: &mut L,
    image: &[u8],
    t: Timeouts,
) -> Result<(), String>
where
    P: FlashPort + ?Sized,
    L: BootLines,
{
    let result = flash_inner(port, lines, image, t).await;
    let _ = port.set_line(Line::Hdlc).await;
    let back = lines.set_boot0(false).and_then(|()| lines.pulse_reset());
    match (result, back) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(format!("returning the SB to its app: {e}")),
    }
}

async fn flash_inner<P, L>(
    port: &mut P,
    lines: &mut L,
    image: &[u8],
    t: Timeouts,
) -> Result<(), String>
where
    P: FlashPort + ?Sized,
    L: BootLines,
{
    if image.is_empty() || image.len() > FLASH_SIZE || image.len() % 8 != 0 {
        return Err(format!("image of {} B refused", image.len()));
    }
    let started = Instant::now();
    lines.set_boot0(true)?;
    lines.pulse_reset()?;
    tokio::time::sleep(BOOTLOADER_SETTLE).await;
    port.set_line(Line::Bootloader)
        .await
        .map_err(|e| e.to_string())?;

    let mut rb = RomBootloader::new(port, t);
    let synced = rb
        .sync(SYNC_ATTEMPTS)
        .await
        .map_err(|e| format!("sync: {e} (nBOOT_SEL = 1, or BOOT0 not wired?)"))?;
    let pid = rb.get_id().await.map_err(|e| format!("GET_ID: {e}"))?;
    if pid != PID_STM32C031 {
        return Err(format!(
            "GET_ID {pid:#06x}, expected {PID_STM32C031:#06x} (STM32C031)"
        ));
    }
    let info = rb.get().await.map_err(|e| format!("GET: {e}"))?;
    for c in [cmd::EXTENDED_ERASE, cmd::WRITE_MEMORY, cmd::READ_MEMORY] {
        if !info.supports(c) {
            return Err(format!(
                "ROM bootloader v{:#04x} lacks command {c:#04x}",
                info.version
            ));
        }
    }
    log(format!(
        "bootloader {:?}, v{:#04x}, pid {pid:#06x} ({} ms)",
        synced,
        info.version,
        started.elapsed().as_millis()
    ));

    // From here on the SB may be left without an app until a later attempt succeeds.
    let pages: Vec<u16> = (0..image.len().div_ceil(PAGE_SIZE) as u16).collect();
    let t0 = Instant::now();
    rb.extended_erase(&pages)
        .await
        .map_err(|e| format!("erase: {e}"))?;
    log(format!(
        "erased pages 0..{} ({} ms)",
        pages.len() - 1,
        t0.elapsed().as_millis()
    ));

    let t0 = Instant::now();
    for (i, block) in image.chunks(MAX_BLOCK).enumerate() {
        let addr = FLASH_BASE + (i * MAX_BLOCK) as u32;
        rb.write_memory(addr, block)
            .await
            .map_err(|e| format!("write: {e}"))?;
    }
    log(format!(
        "wrote {} B ({} ms)",
        image.len(),
        t0.elapsed().as_millis()
    ));

    let t0 = Instant::now();
    for (i, block) in image.chunks(MAX_BLOCK).enumerate() {
        let addr = FLASH_BASE + (i * MAX_BLOCK) as u32;
        // A read can come back short (seen once on the bench Hub, 94 of 256 B while fiber_app
        // was busy): the flash is fine, the answer is not. Sync again and read the block once
        // more before the attempt counts as failed.
        let mut tries = 0;
        loop {
            tries += 1;
            let outcome = match rb.read_memory(addr, block.len()).await {
                Err(e) => Err(format!("verify: {e}")),
                Ok(got) if got != block => {
                    let off = got.iter().zip(block).position(|(a, b)| a != b).unwrap_or(0);
                    Err(format!("verify: mismatch at {:#010x}", addr + off as u32))
                }
                Ok(_) => Ok(()),
            };
            match outcome {
                Ok(()) => break,
                Err(e) if tries >= VERIFY_READ_TRIES => return Err(e),
                Err(e) => {
                    log(format!(
                        "{e}; reading the block again ({tries}/{VERIFY_READ_TRIES})"
                    ));
                    tokio::time::sleep(VERIFY_RETRY_PAUSE).await;
                    rb.sync(SYNC_ATTEMPTS)
                        .await
                        .map_err(|e| format!("verify: resync: {e}"))?;
                }
            }
        }
    }
    log(format!(
        "verified {} B ({} ms)",
        image.len(),
        t0.elapsed().as_millis()
    ));
    Ok(())
}

/// One flash over the real port and GPIOs. `ttyAMA4` must not be open elsewhere (no
/// `StmBridge` alive).
pub fn flash_hardware(port_path: &str, image: &[u8]) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let mut lines = GpioLines::open()?;
    rt.block_on(async {
        let mut port = SerialFlashPort::open(port_path).map_err(|e| e.to_string())?;
        flash(&mut port, &mut lines, image, Timeouts::default()).await
    })
}

/// `/data/fiber/southbridge/update-state.json` (§3.4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    /// `idle` or `failed`.
    pub state: String,
    /// The manifest sha256 the attempts were for.
    pub target: String,
    pub attempts: u32,
    pub boot_id: String,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
    pub updated_at: String,
}

impl UpdateState {
    /// The budget is per (target, boot): a new bundle or a reboot starts it again.
    pub fn attempts_left(&self, target: &str, boot_id: &str) -> u32 {
        if self.target == target && self.boot_id == boot_id {
            MAX_ATTEMPTS.saturating_sub(self.attempts)
        } else {
            MAX_ATTEMPTS
        }
    }

    pub fn load(path: &Path) -> UpdateState {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path) {
        self.updated_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let write = || -> io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec_pretty(self).map_err(io::Error::other)?,
            )?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            log(format!("cannot write {}: {e}", path.display()));
        }
    }
}

fn boot_id() -> String {
    std::fs::read_to_string(BOOT_ID_PATH)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn default_package() -> Result<Option<Package>, String> {
    let dir = Path::new(PKG_DIR);
    load_package(&dir.join(MANIFEST_NAME), &dir.join(IMAGE_NAME))
}

/// The SB bring-up of `fiber_app` (§3.3): open `StmBridge` as before, and bring the SB in line
/// with the bundled image first when it is not. Returns the bridge to use; the only error is
/// the one `StmBridge::new_with_config` has always returned.
pub fn bring_up(port_path: &str, baud_rate: u32) -> io::Result<StmBridge> {
    let _ = PORT.set((port_path.to_string(), baud_rate));
    let mut stm = bring_up_inner(port_path, baud_rate)?;
    note_running(stm.version().ok().flatten().as_deref());
    Ok(stm)
}

fn bring_up_inner(port_path: &str, baud_rate: u32) -> io::Result<StmBridge> {
    let mut stm = StmBridge::new_with_config(port_path, baud_rate)?;
    let reply = stm.version().ok().flatten();

    let pkg = match default_package() {
        Ok(Some(p)) => p,
        Ok(None) => {
            log(format!(
                "no {PKG_DIR}/{MANIFEST_NAME}: this image carries no SB firmware"
            ));
            return Ok(stm);
        }
        Err(e) => {
            log(format!("bundled SB package refused: {e}"));
            return Ok(stm);
        }
    };
    let want = pkg.manifest.version_string();
    if version_matches(reply.as_deref(), &pkg.manifest) {
        log(format!("SB runs {want}, up to date"));
        return Ok(stm);
    }

    let state_path = Path::new(STATE_PATH);
    let boot = boot_id();
    let mut st = UpdateState::load(state_path);
    let mut left = st.attempts_left(&pkg.manifest.sha256, &boot);
    if st.target != pkg.manifest.sha256 || st.boot_id != boot {
        st = UpdateState {
            state: "idle".into(),
            target: pkg.manifest.sha256.clone(),
            boot_id: boot.clone(),
            ..UpdateState::default()
        };
    }
    if left == 0 {
        log(format!(
            "SB answers {:?}, bundled is {want}; no attempts left this boot (state failed)",
            reply
        ));
        return Ok(stm);
    }
    log(format!(
        "SB answers {:?}, bundled is {want}: updating",
        reply
    ));

    drop(stm);
    while left > 0 {
        st.attempts += 1;
        left -= 1;
        // Counted before flashing, so a crash mid-flash still uses up the attempt.
        st.save(state_path);
        let t0 = Instant::now();
        let flashed = flash_hardware(port_path, &pkg.image);
        let mut stm = StmBridge::new_with_config(port_path, baud_rate)?;
        let reply = stm.version().ok().flatten();
        let outcome = match flashed {
            Err(e) => Err(e),
            Ok(()) if version_matches(reply.as_deref(), &pkg.manifest) => Ok(()),
            Ok(()) => Err(format!("flashed, but the SB answers {reply:?}")),
        };
        match outcome {
            Ok(()) => {
                log(format!(
                    "attempt {}: SB now runs {want} ({} ms)",
                    st.attempts,
                    t0.elapsed().as_millis()
                ));
                // The budget only stops repeated failures: a success gives it back, so a
                // later manual flash in the same boot does not eat into it.
                st.state = "idle".into();
                st.attempts = 0;
                st.last_error = None;
                st.last_result = Some(format!("ok {want}"));
                st.save(state_path);
                return Ok(stm);
            }
            Err(e) => {
                log(format!("attempt {} failed: {e}", st.attempts));
                st.last_error = Some(e);
                st.last_result = Some("failed".into());
                if left == 0 {
                    st.state = "failed".into();
                    st.save(state_path);
                    log("no attempts left this boot; continuing with the SB as it is");
                    return Ok(stm);
                }
                st.save(state_path);
                drop(stm);
            }
        }
    }
    unreachable!("the loop returns on its last attempt")
}

/// The signed `southbridge-update` (§3.7): check, mark the update `running`, and flash on a
/// thread of its own, so the MQTT command loop answers at once. The thread holds the bridge's
/// lock for the whole flash (~15 s): LED and power commands wait, and nothing else opens
/// `ttyAMA4`. Bundled image only. Without `force` a SB that already runs it is refused. The
/// retry budget of the start (§3.4) does not apply: an operator asked for this one.
pub fn request_update(stm: Arc<Mutex<StmBridge>>, force: bool) -> Result<(), String> {
    let Some((port_path, baud)) = PORT.get().cloned() else {
        return Err("the Southbridge was never brought up on this Hub".into());
    };
    let pkg = default_package()
        .map_err(|e| format!("the bundled Southbridge firmware is refused: {e}"))?
        .ok_or("this Hub's image bundles no Southbridge firmware")?;
    let want = pkg.manifest.version_string();
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !force && running.as_deref() == Some(want.as_str()) {
        return Err(format!(
            "the Southbridge already runs {want}; force flashes it again"
        ));
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        return Err("a Southbridge update is already running".into());
    }
    let state_path = Path::new(STATE_PATH);
    let mut st = UpdateState::load(state_path);
    st.state = "running".into();
    st.save(state_path);
    crate::libs::mqtt::monitor::request_system_info_publish();
    log(format!(
        "signed update: SB answers {running:?}, flashing {want}{}",
        if force { " (forced)" } else { "" }
    ));
    let spawned = std::thread::Builder::new()
        .name("sb-update".into())
        .spawn(move || {
            let mut guard = stm.lock().unwrap_or_else(|e| e.into_inner());
            let t0 = Instant::now();
            guard.close();
            let flashed = flash_hardware(&port_path, &pkg.image);
            // The bridge comes back in any case; a second try covers an SB still booting.
            let reopened = StmBridge::new_with_config(&port_path, baud).or_else(|e| {
                log(format!("reopen failed ({e}), retrying"));
                std::thread::sleep(Duration::from_secs(1));
                StmBridge::new_with_config(&port_path, baud)
            });
            let reply = match reopened {
                Ok(mut b) => {
                    let reply = b.version().ok().flatten();
                    *guard = b;
                    reply
                }
                Err(e) => {
                    log(format!(
                        "cannot reopen {port_path}: {e}; SB commands fail until restart"
                    ));
                    None
                }
            };
            drop(guard);
            note_running(reply.as_deref());
            let outcome = match flashed {
                Err(e) => Err(e),
                Ok(()) if version_matches(reply.as_deref(), &pkg.manifest) => Ok(()),
                Ok(()) => Err(format!("flashed, but the SB answers {reply:?}")),
            };
            let mut st = UpdateState::load(state_path);
            match outcome {
                Ok(()) => {
                    log(format!(
                        "signed update: SB now runs {want} ({} ms)",
                        t0.elapsed().as_millis()
                    ));
                    st.state = "idle".into();
                    st.last_error = None;
                    st.last_result = Some(format!("ok {want}"));
                }
                Err(e) => {
                    log(format!("signed update failed: {e}"));
                    st.state = "failed".into();
                    st.last_error = Some(e);
                    st.last_result = Some("failed".into());
                }
            }
            st.save(state_path);
            BUSY.store(false, Ordering::SeqCst);
            crate::libs::mqtt::monitor::request_system_info_publish();
        });
    if let Err(e) = spawned {
        BUSY.store(false, Ordering::SeqCst);
        let mut st = UpdateState::load(state_path);
        st.state = "idle".into();
        st.save(state_path);
        return Err(format!("cannot start the update thread: {e}"));
    }
    Ok(())
}

/// `system/info.southbridge` (§3.5): `None` when there is nothing to say — no SB answer, no
/// bundled image and no update ever recorded (a dev host).
pub fn status_json() -> Option<serde_json::Value> {
    status_from(
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        default_package()
            .ok()
            .flatten()
            .map(|p| p.manifest.version_string()),
        std::fs::read_to_string(STATE_PATH)
            .ok()
            .and_then(|t| serde_json::from_str::<UpdateState>(&t).ok()),
        BUSY.load(Ordering::SeqCst),
    )
}

fn status_from(
    running: Option<String>,
    bundled: Option<String>,
    state: Option<UpdateState>,
    busy: bool,
) -> Option<serde_json::Value> {
    if running.is_none() && bundled.is_none() && state.is_none() && !busy {
        return None;
    }
    let mut sb = serde_json::Map::new();
    if let Some(v) = running {
        sb.insert("version".into(), v.into());
    }
    if let Some(v) = bundled {
        sb.insert("bundled".into(), v.into());
    }
    let st = state.unwrap_or_default();
    let mut up = serde_json::Map::new();
    let name = if busy {
        "running".to_string()
    } else if st.state.is_empty() || st.state == "running" {
        // `running` left behind by a killed process: the start re-checks the SB anyway.
        "idle".to_string()
    } else {
        st.state
    };
    up.insert("state".into(), name.into());
    up.insert("attempts".into(), st.attempts.into());
    if let Some(e) = st.last_error {
        up.insert("last_error".into(), e.into());
    }
    if let Some(r) = st.last_result {
        up.insert("last_result".into(), r.into());
    }
    if !st.updated_at.is_empty() {
        up.insert("updated_at".into(), st.updated_at.into());
    }
    sb.insert("update".into(), up.into());
    Some(sb.into())
}

/// `fiber_app southbridge-update [--file <bin> --manifest <json>] [--force]` (§3.4), with
/// `fiber.service` stopped. Returns the process exit code.
pub fn run_cli(args: &[String], port_path: &str, baud_rate: u32) -> i32 {
    let mut file = None;
    let mut manifest = None;
    let mut force = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--file" => file = it.next().cloned(),
            "--manifest" => manifest = it.next().cloned(),
            "--force" => force = true,
            other => {
                eprintln!("unknown argument {other:?}");
                eprintln!("usage: fiber_app southbridge-update [--file <bin> --manifest <json>] [--force]");
                return 2;
            }
        }
    }
    let pkg = match (file, manifest) {
        (None, None) => default_package(),
        (Some(f), Some(m)) => load_package(Path::new(&m), Path::new(&f)),
        _ => {
            eprintln!("--file and --manifest go together");
            return 2;
        }
    };
    let pkg = match pkg {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!("no SB package (manifest missing)");
            return 1;
        }
        Err(e) => {
            eprintln!("package refused: {e}");
            return 1;
        }
    };
    let want = pkg.manifest.version_string();
    let reply = match StmBridge::new_with_config(port_path, baud_rate) {
        Ok(mut stm) => stm.version().ok().flatten(),
        Err(e) => {
            eprintln!("cannot open {port_path}: {e} (is fiber.service still running?)");
            return 1;
        }
    };
    if !force && version_matches(reply.as_deref(), &pkg.manifest) {
        println!("SB already runs {want}");
        return 0;
    }
    log(format!("SB answers {reply:?}, flashing {want}"));
    if let Err(e) = flash_hardware(port_path, &pkg.image) {
        eprintln!("flash failed: {e}");
        return 1;
    }
    let reply = StmBridge::new_with_config(port_path, baud_rate)
        .ok()
        .and_then(|mut s| s.version().ok().flatten());
    if version_matches(reply.as_deref(), &pkg.manifest) {
        println!("SB now runs {want}");
        0
    } else {
        eprintln!("flashed, but the SB answers {reply:?}");
        1
    }
}

#[cfg(test)]
#[path = "southbridge_update_tests.rs"]
mod tests;
