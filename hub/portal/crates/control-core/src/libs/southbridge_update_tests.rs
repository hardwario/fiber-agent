//! Tests for the SB update against a simulated STM32C031 ROM bootloader (AN3155).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use control_radio::p2p::nb_flash::an3155::{cmd, xor, ACK, NACK, SYNC};
use control_radio::p2p::nb_flash::{FlashPort, Line, Timeouts};
use sha2::{Digest, Sha256};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waiting for 0x7F.
    Sync,
    /// Waiting for a command byte and its complement.
    Cmd,
    ReadAddr,
    ReadLen(u32),
    WriteAddr,
    WriteData(u32),
    EraseCount,
    ErasePages(usize),
}

/// The SB as the flasher sees it: two GPIO lines and a UART.
struct Sb {
    flash: Vec<u8>,
    boot0: bool,
    resets: u32,
    in_bootloader: bool,
    line: Line,
    stage: Stage,
    rx: Vec<u8>,
    tx: VecDeque<u8>,
    pid: u16,
    /// nBOOT_SEL = 1: BOOT0 is ignored and the app always starts.
    boot0_ignored: bool,
    /// NACK the Write Memory data phase with this index (0-based), once.
    nack_write: Option<usize>,
    writes: usize,
    erased_pages: Vec<u16>,
    /// Flip a bit in what Read Memory returns.
    corrupt_reads: bool,
    /// Answer this many Read Memory requests with only part of the block, the rest lost.
    short_reads: usize,
}

impl Sb {
    fn new(old: &[u8]) -> Self {
        let mut flash = vec![0xFF; FLASH_SIZE];
        flash[..old.len()].copy_from_slice(old);
        Sb {
            flash,
            boot0: false,
            resets: 0,
            in_bootloader: false,
            line: Line::Hdlc,
            stage: Stage::Sync,
            rx: Vec::new(),
            tx: VecDeque::new(),
            pid: PID_STM32C031,
            boot0_ignored: false,
            nack_write: None,
            writes: 0,
            erased_pages: Vec::new(),
            corrupt_reads: false,
            short_reads: 0,
        }
    }

    fn reset(&mut self) {
        self.resets += 1;
        self.in_bootloader = self.boot0 && !self.boot0_ignored;
        self.stage = Stage::Sync;
        self.rx.clear();
        self.tx.clear();
    }

    fn reply(&mut self, bytes: &[u8]) {
        self.tx.extend(bytes);
    }

    fn take(&mut self, n: usize) -> Option<Vec<u8>> {
        (self.rx.len() >= n).then(|| self.rx.drain(..n).collect())
    }

    fn process(&mut self) {
        loop {
            let progressed = match self.stage {
                Stage::Sync => self.take(1).map(|b| {
                    if b[0] == SYNC {
                        self.reply(&[ACK]);
                        self.stage = Stage::Cmd;
                    }
                }),
                Stage::Cmd => self.take(2).map(|b| {
                    if b[0] != !b[1] {
                        self.reply(&[NACK]);
                        return;
                    }
                    match b[0] {
                        cmd::GET => {
                            let cmds = [
                                0x00, 0x01, 0x02, 0x11, 0x21, 0x31, 0x44, 0x63, 0x73, 0x82, 0x92,
                            ];
                            self.reply(&[ACK, cmds.len() as u8, 0x31]);
                            self.reply(&cmds);
                            self.reply(&[ACK]);
                        }
                        cmd::GET_ID => {
                            let [hi, lo] = self.pid.to_be_bytes();
                            self.reply(&[ACK, 1, hi, lo, ACK]);
                        }
                        cmd::READ_MEMORY => {
                            self.reply(&[ACK]);
                            self.stage = Stage::ReadAddr;
                        }
                        cmd::WRITE_MEMORY => {
                            self.reply(&[ACK]);
                            self.stage = Stage::WriteAddr;
                        }
                        cmd::EXTENDED_ERASE => {
                            self.reply(&[ACK]);
                            self.stage = Stage::EraseCount;
                        }
                        _ => self.reply(&[NACK]),
                    }
                }),
                Stage::ReadAddr | Stage::WriteAddr => self.take(5).map(|b| {
                    let addr = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    if xor(&b[..4]) != b[4] {
                        self.reply(&[NACK]);
                        self.stage = Stage::Cmd;
                        return;
                    }
                    self.reply(&[ACK]);
                    self.stage = if self.stage == Stage::ReadAddr {
                        Stage::ReadLen(addr)
                    } else {
                        Stage::WriteData(addr)
                    };
                }),
                Stage::ReadLen(addr) => self.take(2).map(|b| {
                    let n = b[0] as usize + 1;
                    let off = (addr - FLASH_BASE) as usize;
                    let mut data = self.flash[off..off + n].to_vec();
                    if self.corrupt_reads {
                        data[0] ^= 1;
                    }
                    if self.short_reads > 0 {
                        self.short_reads -= 1;
                        data.truncate(n / 2 + 1);
                    }
                    self.reply(&[ACK]);
                    self.reply(&data);
                    self.stage = Stage::Cmd;
                }),
                Stage::WriteData(addr) => {
                    if self.rx.is_empty() {
                        None
                    } else {
                        let n = self.rx[0] as usize + 1;
                        self.take(n + 2).map(|b| {
                            self.stage = Stage::Cmd;
                            let i = self.writes;
                            self.writes += 1;
                            if self.nack_write == Some(i) {
                                self.nack_write = None;
                                self.reply(&[NACK]);
                                return;
                            }
                            let off = (addr - FLASH_BASE) as usize;
                            for (k, v) in b[1..=n].iter().enumerate() {
                                // Flash programming can only clear bits.
                                self.flash[off + k] &= v;
                            }
                            self.reply(&[ACK]);
                        })
                    }
                }
                Stage::EraseCount => self.take(2).map(|b| {
                    let n = u16::from_be_bytes([b[0], b[1]]) as usize + 1;
                    self.stage = Stage::ErasePages(n);
                }),
                Stage::ErasePages(n) => self.take(2 * n + 1).map(|b| {
                    for p in b[..2 * n].chunks(2) {
                        let page = u16::from_be_bytes([p[0], p[1]]);
                        let off = page as usize * PAGE_SIZE;
                        self.flash[off..off + PAGE_SIZE].fill(0xFF);
                        self.erased_pages.push(page);
                    }
                    self.reply(&[ACK]);
                    self.stage = Stage::Cmd;
                }),
            };
            if progressed.is_none() {
                return;
            }
        }
    }
}

#[derive(Clone)]
struct SimPort(Arc<Mutex<Sb>>);
struct SimLines(Arc<Mutex<Sb>>);

#[async_trait::async_trait]
impl FlashPort for SimPort {
    async fn set_line(&mut self, line: Line) -> control_radio::Result<()> {
        self.0.lock().unwrap().line = line;
        Ok(())
    }
    async fn write_all(&mut self, bytes: &[u8]) -> control_radio::Result<()> {
        let mut sb = self.0.lock().unwrap();
        // The ROM bootloader only hears 8E1; the app's protocol is not part of the flash.
        if sb.in_bootloader && sb.line == Line::Bootloader {
            sb.rx.extend_from_slice(bytes);
            sb.process();
        }
        Ok(())
    }
    async fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> control_radio::Result<usize> {
        let mut sb = self.0.lock().unwrap();
        let n = buf.len().min(sb.tx.len());
        for b in buf.iter_mut().take(n) {
            *b = sb.tx.pop_front().unwrap();
        }
        Ok(n)
    }
    async fn discard_input(&mut self) -> control_radio::Result<()> {
        self.0.lock().unwrap().tx.clear();
        Ok(())
    }
}

impl BootLines for SimLines {
    fn set_boot0(&mut self, high: bool) -> Result<(), String> {
        self.0.lock().unwrap().boot0 = high;
        Ok(())
    }
    fn pulse_reset(&mut self) -> Result<(), String> {
        self.0.lock().unwrap().reset();
        Ok(())
    }
}

fn fast() -> Timeouts {
    Timeouts {
        sync: Duration::from_millis(5),
        reply: Duration::from_millis(5),
        write: Duration::from_millis(5),
        erase_base: Duration::from_millis(5),
        erase_per_page: Duration::from_millis(1),
        checksum: Duration::from_millis(5),
    }
}

fn image(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

fn run(sb: Sb, img: &[u8]) -> (Result<(), String>, Sb) {
    let shared = Arc::new(Mutex::new(sb));
    let mut port = SimPort(shared.clone());
    let mut lines = SimLines(shared.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let r = rt.block_on(flash(&mut port, &mut lines, img, fast()));
    drop((port, lines));
    let sb = Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
    (r, sb)
}

#[test]
fn flashes_erases_only_the_image_pages_and_returns_to_the_app() {
    let old = image(30 * 1024, 0x11);
    let new = image(26_152, 0x5A);
    let (r, sb) = run(Sb::new(&old), &new);
    r.unwrap();
    assert_eq!(&sb.flash[..new.len()], &new[..]);
    // 26152 B = 13 pages; page 13 onwards keeps the old bytes.
    assert_eq!(sb.erased_pages, (0..13).collect::<Vec<u16>>());
    assert_eq!(&sb.flash[13 * PAGE_SIZE..30 * 1024], &old[13 * PAGE_SIZE..]);
    assert!(!sb.boot0, "BOOT0 must end low");
    assert!(!sb.in_bootloader, "the SB must end in its app");
    assert_eq!(sb.resets, 2);
}

#[test]
fn no_bootloader_answer_erases_nothing() {
    let old = image(1024, 0x11);
    let mut sb = Sb::new(&old);
    sb.boot0_ignored = true;
    let (r, sb) = run(sb, &image(2048, 0x22));
    assert!(r.unwrap_err().starts_with("sync:"));
    assert!(sb.erased_pages.is_empty());
    assert_eq!(&sb.flash[..1024], &old[..]);
    assert!(!sb.boot0);
    assert_eq!(sb.resets, 2);
}

#[test]
fn a_wrong_chip_erases_nothing() {
    let mut sb = Sb::new(&[]);
    sb.pid = 0x0497;
    let (r, sb) = run(sb, &image(2048, 0x22));
    assert!(r.unwrap_err().contains("GET_ID 0x0497"));
    assert!(sb.erased_pages.is_empty());
    assert!(!sb.boot0);
}

#[test]
fn a_nacked_write_fails_and_still_leaves_boot0_low() {
    let mut sb = Sb::new(&[]);
    sb.nack_write = Some(3);
    let (r, sb) = run(sb, &image(4096, 0x33));
    assert!(r.unwrap_err().starts_with("write:"));
    assert!(!sb.boot0);
    assert!(!sb.in_bootloader);
}

#[test]
fn a_bad_read_back_fails_the_attempt() {
    let mut sb = Sb::new(&[]);
    sb.corrupt_reads = true;
    let (r, _) = run(sb, &image(4096, 0x44));
    assert!(r.unwrap_err().starts_with("verify: mismatch at 0x08000000"));
}

/// A read that comes back short is read again after a resync; the flash itself was fine.
#[test]
fn a_short_read_back_is_read_again_and_the_flash_succeeds() {
    let mut sb = Sb::new(&[]);
    sb.short_reads = 1;
    let img = image(4096, 0x55);
    let (r, sb) = run(sb, &img);
    r.expect("one short read is retried");
    assert_eq!(&sb.flash[..img.len()], &img[..]);
    assert!(!sb.boot0);
}

#[test]
fn reads_that_stay_short_fail_the_attempt_after_three_tries() {
    let mut sb = Sb::new(&[]);
    sb.short_reads = usize::MAX;
    let (r, sb) = run(sb, &image(2048, 0x66));
    assert!(r.unwrap_err().starts_with("verify:"));
    assert!(!sb.boot0);
}

#[test]
fn an_image_that_is_not_a_multiple_of_8_is_refused_before_boot0() {
    let (r, sb) = run(Sb::new(&[]), &image(4097, 0));
    assert!(r.is_err());
    assert_eq!(sb.resets, 1, "only the final return-to-app reset");
    assert!(sb.erased_pages.is_empty());
}

fn manifest_for(img: &[u8]) -> Manifest {
    Manifest {
        format: 1,
        board: BOARD.into(),
        load_addr: u64::from(FLASH_BASE),
        fw_version: "1.1.0".into(),
        git_sha: "1a2b3c4d".into(),
        image_len: img.len(),
        crc32: format!("0x{:08x}", control_radio::p2p::nb_flash::image::crc32(img)),
        sha256: hex::encode(Sha256::digest(img)),
    }
}

#[test]
fn a_package_that_matches_its_manifest_is_accepted() {
    let img = image(26_152, 1);
    let pkg = check_package(manifest_for(&img), img.clone()).unwrap();
    assert_eq!(pkg.image, img);
}

#[test]
fn a_corrupt_image_is_refused() {
    let img = image(26_152, 1);
    let m = manifest_for(&img);
    let mut bad = img.clone();
    bad[100] ^= 0xFF;
    let e = check_package(m, bad).unwrap_err();
    assert!(e.contains("sha256") && e.contains("crc32"), "{e}");
}

#[test]
fn a_manifest_for_another_board_or_address_is_refused() {
    let img = image(2048, 1);
    let mut m = manifest_for(&img);
    m.board = "stm32wl5moc".into();
    m.load_addr = 0x0800_0800;
    let e = check_package(m, img).unwrap_err();
    assert!(e.contains("board") && e.contains("load_addr"), "{e}");
}

#[test]
fn an_oversized_image_is_refused() {
    let img = image(FLASH_SIZE + 8, 1);
    assert!(check_package(manifest_for(&img), img).is_err());
}

#[test]
fn load_package_without_a_manifest_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let r = load_package(
        &dir.path().join(MANIFEST_NAME),
        &dir.path().join(IMAGE_NAME),
    );
    assert!(r.unwrap().is_none());
}

#[test]
fn load_package_reads_what_sb_package_writes() {
    let dir = tempfile::tempdir().unwrap();
    let img = image(1024, 7);
    let m = manifest_for(&img);
    let json = format!(
        "{{\"format\": 1, \"board\": \"fiber_southbridge\", \"load_addr\": 134217728, \
         \"fw_version\": \"1.1.0\", \"git_sha\": \"1a2b3c4d\", \"image_len\": {}, \
         \"crc32\": \"{}\", \"sha256\": \"{}\"}}",
        img.len(),
        m.crc32,
        m.sha256
    );
    std::fs::write(dir.path().join(MANIFEST_NAME), json).unwrap();
    std::fs::write(dir.path().join(IMAGE_NAME), &img).unwrap();
    let pkg = load_package(
        &dir.path().join(MANIFEST_NAME),
        &dir.path().join(IMAGE_NAME),
    )
    .unwrap()
    .unwrap();
    assert_eq!(pkg.manifest, m);
}

#[test]
fn version_matching() {
    let m = manifest_for(&image(8, 0));
    assert!(version_matches(
        Some("VERSION: south-zephyr 1.1.0 g1a2b3c4d"),
        &m
    ));
    assert!(version_matches(
        Some("VERSION: south-zephyr 1.1.0 g1a2b3c4d\r"),
        &m
    ));
    assert!(!version_matches(Some("VERSION: south-zephyr 1.0.0"), &m));
    assert!(!version_matches(
        Some("VERSION: south-zephyr 1.1.0 g00000000"),
        &m
    ));
    assert!(!version_matches(Some("ERR"), &m));
    assert!(!version_matches(None, &m));
}

#[test]
fn the_budget_is_per_target_and_boot() {
    let st = UpdateState {
        state: "failed".into(),
        target: "aa".into(),
        attempts: 3,
        boot_id: "b1".into(),
        ..UpdateState::default()
    };
    assert_eq!(st.attempts_left("aa", "b1"), 0);
    assert_eq!(
        st.attempts_left("aa", "b2"),
        MAX_ATTEMPTS,
        "a reboot resets it"
    );
    assert_eq!(
        st.attempts_left("bb", "b1"),
        MAX_ATTEMPTS,
        "a new bundle resets it"
    );
    let st = UpdateState { attempts: 1, ..st };
    assert_eq!(st.attempts_left("aa", "b1"), 2);
}

#[test]
fn the_state_file_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("southbridge/update-state.json");
    assert_eq!(UpdateState::load(&path), UpdateState::default());
    let mut st = UpdateState {
        state: "idle".into(),
        target: "aa".into(),
        attempts: 1,
        boot_id: "b1".into(),
        last_result: Some("ok".into()),
        ..UpdateState::default()
    };
    st.save(&path);
    assert_eq!(UpdateState::load(&path), st);
}

/// §3.5: nothing known, nothing published; a stale `running` reads as idle unless busy.
#[test]
fn the_status_block_says_only_what_is_known() {
    assert_eq!(status_from(None, None, None, false), None);
    let v = status_from(
        Some("south-zephyr 1.1.0 g73f39ed5".into()),
        Some("south-zephyr 1.1.0 g73f39ed5".into()),
        None,
        false,
    )
    .unwrap();
    assert_eq!(v["version"], "south-zephyr 1.1.0 g73f39ed5");
    assert_eq!(v["update"]["state"], "idle");
    assert!(v["update"].get("last_error").is_none());

    let stale = UpdateState {
        state: "running".into(),
        ..UpdateState::default()
    };
    let v = status_from(None, None, Some(stale.clone()), false).unwrap();
    assert_eq!(v["update"]["state"], "idle");
    assert!(v.get("version").is_none());
    let v = status_from(None, None, Some(stale), true).unwrap();
    assert_eq!(v["update"]["state"], "running");

    let failed = UpdateState {
        state: "failed".into(),
        attempts: 3,
        last_error: Some("sync: no ACK".into()),
        last_result: Some("failed".into()),
        updated_at: "2026-10-09T12:00:00Z".into(),
        ..UpdateState::default()
    };
    let v = status_from(None, Some("b".into()), Some(failed), false).unwrap();
    assert_eq!(v["update"]["state"], "failed");
    assert_eq!(v["update"]["attempts"], 3);
    assert_eq!(v["update"]["last_error"], "sync: no ACK");
}

#[test]
fn the_version_prefix_is_stripped() {
    assert_eq!(
        strip_version("VERSION: south-zephyr 1.1.0 g1"),
        "south-zephyr 1.1.0 g1"
    );
    assert_eq!(strip_version(" south-zephyr 1.0.0 "), "south-zephyr 1.0.0");
}
