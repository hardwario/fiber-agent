//! The whole update procedure against the simulated Northbridge ([`super::sim`]).

use std::time::Duration;

use super::image::tests::{manifest_for, test_image};
use super::image::{
    self, app_valid_record, Manifest, APP_ADDR, PAGE_SIZE, RECORD_ADDR, RECORD_LEN,
};
use super::sim::{ChecksumSupport, EnterBl, SimConfig, SimNb, SyncedSync, STORAGE_FILL, STUB_FILL};
use super::*;

fn fast() -> Options {
    let t = Duration::from_millis(20);
    Options {
        timing: Timing {
            hdlc_rsp: t,
            reboot_settle: Duration::ZERO,
            boot: t,
            retry_pause: Duration::ZERO,
            bootloader: Timeouts {
                sync: t,
                reply: t,
                write: t,
                erase_base: t,
                erase_per_page: Duration::ZERO,
                checksum: t,
            },
        },
        ..Options::default()
    }
}

fn old() -> (Vec<u8>, Manifest) {
    let img = test_image(4096, 9);
    let m = manifest_for(&img, "0.3.9", "0a0b0c0d");
    (img, m)
}

fn new() -> (Vec<u8>, Manifest) {
    // Not a multiple of 256: the last Write Memory block is short (16 B).
    let img = test_image(10_000, 3);
    let m = manifest_for(&img, "0.4.0", "1a2b3c4d");
    (img, m)
}

fn running(cfg: SimConfig) -> SimNb {
    let (img, m) = old();
    SimNb::running(cfg, &img, &m)
}

async fn run(
    sim: &mut SimNb,
    img: &[u8],
    m: &Manifest,
    opts: &Options,
) -> (Result<Outcome>, Vec<Progress>) {
    let mut seen = Vec::new();
    let r = update_northbridge(sim, img, m, opts, &mut |p: &Progress| seen.push(p.clone())).await;
    (r, seen)
}

fn notes(seen: &[Progress]) -> Vec<String> {
    seen.iter()
        .filter_map(|p| match p {
            Progress::Note { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// The flash holds the new image and its record, and nothing outside slot0 and the record
/// page was touched.
fn assert_flashed(sim: &SimNb, img: &[u8], m: &Manifest) {
    assert_eq!(sim.region(APP_ADDR, img.len()), img, "the image");
    assert_eq!(
        sim.region(RECORD_ADDR, RECORD_LEN),
        &app_valid_record(m),
        "the record"
    );
    assert!(
        sim.region(
            RECORD_ADDR + RECORD_LEN as u32,
            PAGE_SIZE as usize - RECORD_LEN
        )
        .iter()
        .all(|&b| b == 0xFF),
        "the rest of the record page is erased"
    );
    assert_untouched(sim);
}

/// The stub page and `storage` hold what they held; the erase covered exactly pages
/// 1..=119, the record page first.
fn assert_untouched(sim: &SimNb) {
    assert!(
        sim.region(image::FLASH_BASE, PAGE_SIZE as usize)
            .iter()
            .all(|&b| b == STUB_FILL),
        "the stub page"
    );
    let storage = image::FLASH_BASE + image::STORAGE_FIRST_PAGE as u32 * PAGE_SIZE;
    assert!(
        sim.region(storage, 8 * PAGE_SIZE as usize)
            .iter()
            .all(|&b| b == STORAGE_FILL),
        "storage"
    );
    assert!(
        sim.erased.iter().all(|&p| (1..=119).contains(&p)),
        "erased only slot0 and the record page: {:?}",
        sim.erased
    );
}

fn assert_erased_exactly_slot0_and_record(sim: &SimNb) {
    assert_eq!(
        sim.erased.first(),
        Some(&image::RECORD_PAGE),
        "record page first"
    );
    let mut pages = sim.erased.clone();
    pages.sort_unstable();
    pages.dedup();
    assert_eq!(pages, (1..=119).collect::<Vec<u16>>());
}

fn record_page_blank(sim: &SimNb) -> bool {
    sim.region(RECORD_ADDR, PAGE_SIZE as usize)
        .iter()
        .all(|&b| b == 0xFF)
}

#[test]
fn the_update_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let (img, m) = new();
    let mut sim = running(SimConfig::default());
    let opts = fast();
    let mut cb = |_: &Progress| {};
    let fut = update_northbridge(&mut sim, &img, &m, &opts, &mut cb);
    assert_send(&fut);
}

#[tokio::test]
async fn happy_path_from_a_running_northbridge() {
    let (img, m) = new();
    let mut sim = running(SimConfig::default());
    let (r, seen) = run(&mut sim, &img, &m, &fast()).await;
    let out = r.expect("the update succeeds");

    assert_eq!(out.entry, Entry::Hdlc);
    assert_eq!(out.synced, an3155::Synced::Fresh);
    assert_eq!(out.bootloader_version, 0x33);
    assert_eq!(out.verified, Verified::Checksum);
    assert_eq!(
        out.boot_event,
        Some(BootEvent {
            fw_ver: [0, 4, 0],
            git_sha: [0x1a, 0x2b, 0x3c, 0x4d],
            reset_cause: 0x02,
        })
    );
    assert_eq!(
        out.identity,
        Identity {
            fw: [0, 4, 0],
            git_sha: [0x1a, 0x2b, 0x3c, 0x4d],
            app: Some(AppImageId {
                image_len: 10_000,
                crc32: m.crc32,
            }),
        }
    );
    let order: Vec<Step> = out.steps.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        order,
        [
            Step::EnterBootloader,
            Step::Sync,
            Step::Erase,
            Step::Write,
            Step::Verify,
            Step::Record,
            Step::Go,
            Step::BootCheck,
        ]
    );

    assert_flashed(&sim, &img, &m);
    assert_erased_exactly_slot0_and_record(&sim);
    assert_eq!(sim.gos, [image::STUB_ADDR], "GO to the stub, once");
    assert_eq!(
        sim.hdlc_commands,
        [modem::cmd::ENTER_BOOTLOADER, modem::cmd::GET_INFO]
    );
    assert_eq!(sim.wrong_line_bytes, 0, "every byte at the right parity");
    assert_eq!(sim.checksums, 1);
    assert!(!sim.in_rom_bootloader(), "the new app runs");
    // The record is the last thing written; every block is 8-byte aligned and sized.
    assert_eq!(sim.writes.last(), Some(&(RECORD_ADDR, RECORD_LEN)));
    assert!(sim
        .writes
        .iter()
        .all(|&(a, l)| a.is_multiple_of(8) && l.is_multiple_of(8) && l <= 256));
    assert_eq!(sim.writes.len(), 10_000usize.div_ceil(256) + 1);

    // Progress: every step started and finished in order, the write counted to the end.
    assert_eq!(
        seen.first(),
        Some(&Progress::Started(Step::EnterBootloader))
    );
    assert!(matches!(
        seen.last(),
        Some(Progress::Finished {
            step: Step::BootCheck,
            ..
        })
    ));
    assert!(seen.contains(&Progress::Advanced {
        step: Step::Write,
        done: 10_000,
        total: 10_000
    }));
    assert!(seen.contains(&Progress::Advanced {
        step: Step::Erase,
        done: 119,
        total: 119
    }));
    assert!(!seen.iter().any(|p| matches!(p, Progress::Failed { .. })));
}

#[tokio::test]
async fn the_largest_image_fills_slot0_and_verifies_by_read_back() {
    let img = test_image(image::APP_MAX_LEN as usize, 5);
    let m = manifest_for(&img, "0.4.1", "deadbeef");
    let mut sim = running(SimConfig::default());
    let opts = Options {
        verify: VerifyMode::ReadBack,
        ..fast()
    };
    let out = run(&mut sim, &img, &m, &opts).await.0.expect("succeeds");
    assert_eq!(out.verified, Verified::ReadBack);
    assert_eq!(sim.checksums, 0);
    assert_flashed(&sim, &img, &m);
    assert_erased_exactly_slot0_and_record(&sim);
}

#[tokio::test]
async fn a_northbridge_already_in_the_rom_bootloader_is_found_by_a_direct_sync() {
    let (img, m) = new();
    let mut sim = SimNb::in_bootloader(SimConfig::default(), false);
    let (r, seen) = run(&mut sim, &img, &m, &fast()).await;
    let out = r.expect("succeeds");
    assert_eq!(out.entry, Entry::Direct);
    assert_eq!(out.synced, an3155::Synced::Fresh);
    assert!(sim.wrong_line_bytes > 0, "the HDLC frame went nowhere");
    assert!(notes(&seen).iter().any(|n| n.contains("no HDLC answer")));
    assert_flashed(&sim, &img, &m);
}

#[tokio::test]
async fn an_already_synced_bootloader_that_nacks_the_sync_byte() {
    let (img, m) = new();
    let mut sim = SimNb::in_bootloader(SimConfig::default(), true);
    let out = run(&mut sim, &img, &m, &fast()).await.0.expect("succeeds");
    assert_eq!(out.synced, an3155::Synced::Already);
    assert_flashed(&sim, &img, &m);
}

#[tokio::test]
async fn an_already_synced_bootloader_that_waits_for_the_complement() {
    let (img, m) = new();
    let cfg = SimConfig {
        synced_sync: SyncedSync::WaitComplement,
        ..SimConfig::default()
    };
    let mut sim = SimNb::in_bootloader(cfg, true);
    let out = run(&mut sim, &img, &m, &fast()).await.0.expect("succeeds");
    assert_eq!(out.synced, an3155::Synced::Already);
    assert_flashed(&sim, &img, &m);
}

#[tokio::test]
async fn a_silent_northbridge_gets_no_hdlc_at_all() {
    let (img, m) = new();
    let mut sim = SimNb::in_bootloader(SimConfig::default(), false);
    let opts = Options {
        nb_silent: true,
        ..fast()
    };
    let out = run(&mut sim, &img, &m, &opts).await.0.expect("succeeds");
    assert_eq!(out.entry, Entry::Silent);
    assert_eq!(sim.wrong_line_bytes, 0);
    assert_eq!(sim.line_changes.first(), Some(&Line::Bootloader));
    assert_flashed(&sim, &img, &m);
}

#[tokio::test]
async fn a_northbridge_answering_neither_link_is_a_timeout() {
    let (img, m) = new();
    // Running the app, but the caller says it is silent: the 8E1 sync goes unheard.
    let mut sim = running(SimConfig::default());
    let opts = Options {
        nb_silent: true,
        ..fast()
    };
    let (r, seen) = run(&mut sim, &img, &m, &opts).await;
    let e = r.expect_err("nothing answers");
    assert_eq!(e.kind(), Kind::Timeout);
    assert_eq!(e.component(), Component::NorthbridgeUart);
    assert!(e.message().contains("neither HDLC nor"), "{}", e.message());
    assert!(seen.contains(&Progress::Failed {
        step: Step::Sync,
        error: e.to_string()
    }));
    assert!(sim.erased.is_empty());
}

#[tokio::test]
async fn a_nacked_write_is_retried() {
    let (img, m) = new();
    let cfg = SimConfig {
        nack_writes: 1,
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let (r, seen) = run(&mut sim, &img, &m, &fast()).await;
    r.expect("the retry succeeds");
    assert!(
        notes(&seen)
            .iter()
            .any(|n| n.contains("Write Memory") && n.contains("NACK") && n.contains("retry 1")),
        "{:?}",
        notes(&seen)
    );
    assert_flashed(&sim, &img, &m);
}

#[tokio::test]
async fn a_write_that_keeps_failing_aborts_before_the_record_and_a_rerun_recovers() {
    let (img, m) = new();
    let cfg = SimConfig {
        nack_writes: 100,
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let (r, seen) = run(&mut sim, &img, &m, &fast()).await;
    let e = r.expect_err("aborts");
    assert_eq!(e.kind(), Kind::Transport);
    assert_eq!(e.component(), Component::NorthbridgeFirmware);
    assert!(e.message().contains("NACK"), "{}", e.message());
    assert!(seen.iter().any(|p| matches!(
        p,
        Progress::Failed {
            step: Step::Write,
            ..
        }
    )));
    assert!(record_page_blank(&sim), "no record written");
    assert!(sim.gos.is_empty(), "no GO");
    assert!(sim.in_rom_bootloader(), "left in the ROM bootloader");
    assert_untouched(&sim);

    // §3.6: the next attempt finds it in the (already synced) ROM bootloader.
    sim.cfg.nack_writes = 0;
    let out = run(&mut sim, &img, &m, &fast()).await.0.expect("recovers");
    assert_eq!(out.entry, Entry::Direct);
    assert_eq!(out.synced, an3155::Synced::Already);
    assert_flashed(&sim, &img, &m);
}

#[tokio::test]
async fn a_nacked_erase_is_retried() {
    let (img, m) = new();
    let cfg = SimConfig {
        nack_erases: 2,
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    run(&mut sim, &img, &m, &fast())
        .await
        .0
        .expect("two retries are allowed");
    assert_flashed(&sim, &img, &m);
    assert_erased_exactly_slot0_and_record(&sim);
}

#[tokio::test]
async fn a_checksum_mismatch_fails_and_writes_no_record() {
    let (img, m) = new();
    let cfg = SimConfig {
        corrupt_at: Some(APP_ADDR + 5000),
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let e = run(&mut sim, &img, &m, &fast())
        .await
        .0
        .expect_err("mismatch");
    assert_eq!(e.kind(), Kind::Transport);
    assert!(e.message().contains("Get Checksum"), "{}", e.message());
    assert!(record_page_blank(&sim), "no record written");
    assert!(sim.gos.is_empty(), "no GO");
    assert!(sim.in_rom_bootloader());
}

#[tokio::test]
async fn a_read_back_mismatch_fails_and_writes_no_record() {
    let (img, m) = new();
    let cfg = SimConfig {
        corrupt_at: Some(APP_ADDR + 9999),
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let opts = Options {
        verify: VerifyMode::ReadBack,
        ..fast()
    };
    let e = run(&mut sim, &img, &m, &opts)
        .await
        .0
        .expect_err("mismatch");
    assert!(
        e.message().contains("read-back differs at 0x08002f0f"),
        "{}",
        e.message()
    );
    assert!(record_page_blank(&sim));
    assert!(sim.gos.is_empty());
}

#[tokio::test]
async fn without_get_checksum_the_image_is_read_back() {
    for support in [
        ChecksumSupport::Absent,
        ChecksumSupport::Nack,
        ChecksumSupport::NackSize,
    ] {
        let (img, m) = new();
        let cfg = SimConfig {
            checksum: support,
            ..SimConfig::default()
        };
        let mut sim = running(cfg);
        let (r, seen) = run(&mut sim, &img, &m, &fast()).await;
        let out = r.expect("succeeds");
        assert_eq!(out.verified, Verified::ReadBack, "{support:?}");
        assert_eq!(sim.checksums, 0, "{support:?}");
        assert!(
            notes(&seen)
                .iter()
                .any(|n| n.contains("verifying by read-back")),
            "{support:?}: {:?}",
            notes(&seen)
        );
        assert_flashed(&sim, &img, &m);
    }
}

#[tokio::test]
async fn verify_both_runs_the_checksum_and_the_read_back() {
    let (img, m) = new();
    let mut sim = running(SimConfig::default());
    let opts = Options {
        verify: VerifyMode::Both,
        ..fast()
    };
    let out = run(&mut sim, &img, &m, &opts).await.0.expect("succeeds");
    assert_eq!(out.verified, Verified::Both);
    assert_eq!(sim.checksums, 1);
}

#[tokio::test]
async fn a_corrupt_image_or_manifest_is_refused_before_any_byte_is_sent() {
    let (img, m) = new();
    let mut flipped = img.clone();
    flipped[123] ^= 0x80;
    let mut wrong_len = m.clone();
    wrong_len.image_len += 8;
    let mut wrong_crc = m.clone();
    wrong_crc.crc32 ^= 1;
    for (image, manifest, why) in [
        (&flipped, &m, "sha256"),
        (&img, &wrong_len, "length"),
        (&img, &wrong_crc, "crc32"),
    ] {
        let mut sim = running(SimConfig::default());
        let (r, seen) = run(&mut sim, image, manifest, &fast()).await;
        let e = r.expect_err(why);
        assert_eq!(e.kind(), Kind::InvalidArgument, "{why}");
        assert_eq!(e.component(), Component::NorthbridgeFirmware, "{why}");
        assert_eq!(sim.host_bytes, 0, "{why}: not a byte sent");
        assert!(sim.line_changes.is_empty(), "{why}: the line untouched");
        assert!(seen.is_empty(), "{why}: no step started");
    }
}

#[tokio::test]
async fn an_image_without_enter_bootloader_needs_the_j_link_migration() {
    let (img, m) = new();
    let cfg = SimConfig {
        enter_bl: EnterBl::Unsupported,
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let e = run(&mut sim, &img, &m, &fast())
        .await
        .0
        .expect_err("refused");
    assert_eq!(e.kind(), Kind::Transport);
    assert!(e.message().contains("J-Link migration"), "{}", e.message());
    assert!(
        !sim.line_changes.contains(&Line::Bootloader),
        "no 8E1 at all"
    );
    assert!(!sim.in_rom_bootloader());
}

#[tokio::test]
async fn a_chip_that_is_not_an_stm32wl5x_is_left_alone() {
    let (img, m) = new();
    let cfg = SimConfig {
        pid: 0x0443,
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let e = run(&mut sim, &img, &m, &fast())
        .await
        .0
        .expect_err("refused");
    assert!(e.message().contains("0x0443"), "{}", e.message());
    assert!(sim.erased.is_empty() && sim.writes.is_empty());
}

#[tokio::test]
async fn a_180_byte_get_info_is_checked_by_version_only() {
    let (img, m) = new();
    let cfg = SimConfig {
        info_len: 180,
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let (r, seen) = run(&mut sim, &img, &m, &fast()).await;
    let out = r.expect("succeeds");
    assert_eq!(out.identity.app, None);
    assert!(notes(&seen).iter().any(|n| n.contains("no app_valid tail")));
}

#[tokio::test]
async fn a_northbridge_that_boots_the_wrong_version_fails_the_update() {
    let (img, m) = new();
    let cfg = SimConfig {
        report_fw: Some([0, 3, 9]),
        ..SimConfig::default()
    };
    let mut sim = running(cfg);
    let e = run(&mut sim, &img, &m, &fast())
        .await
        .0
        .expect_err("wrong version");
    assert_eq!(e.kind(), Kind::Transport);
    assert!(
        e.message()
            .contains("boots 0.3.9+1a2b3c4d but the manifest is 0.4.0+1a2b3c4d"),
        "{}",
        e.message()
    );
}

#[test]
fn the_get_info_tail_is_read_from_bytes_180_to_188() {
    let mut p = vec![0u8; 180];
    assert_eq!(app_image_from_get_info(&p), None);
    p.extend_from_slice(&0x0001_2340u32.to_le_bytes());
    p.extend_from_slice(&0xCAFE_F00Du32.to_le_bytes());
    assert_eq!(
        app_image_from_get_info(&p),
        Some(AppImageId {
            image_len: 0x0001_2340,
            crc32: 0xCAFE_F00D
        })
    );
    assert_eq!(app_image_from_get_info(&p[..187]), None);
}

#[tokio::test]
async fn a_probe_reads_the_running_image_and_its_tail() {
    let (img, m) = old();
    let mut sim = running(SimConfig::default());
    let (info, app) = probe(&mut sim, &fast())
        .await
        .expect("probe")
        .expect("answers");
    assert_eq!(info.fw, [0, 3, 9]);
    assert_eq!(info.git_sha, [0x0a, 0x0b, 0x0c, 0x0d]);
    assert_eq!(
        app,
        Some(AppImageId {
            image_len: img.len() as u32,
            crc32: m.crc32
        })
    );
    assert_eq!(sim.hdlc_commands, vec![modem::cmd::GET_INFO]);
}

#[tokio::test]
async fn a_probe_of_a_180_byte_image_has_no_tail() {
    let mut sim = running(SimConfig {
        info_len: 180,
        ..SimConfig::default()
    });
    let (_, app) = probe(&mut sim, &fast())
        .await
        .expect("probe")
        .expect("answers");
    assert_eq!(app, None);
}

#[tokio::test]
async fn a_northbridge_in_the_rom_bootloader_probes_silent() {
    let mut sim = SimNb::in_bootloader(SimConfig::default(), false);
    assert_eq!(probe(&mut sim, &fast()).await.expect("probe"), None);
    assert!(sim.in_rom_bootloader());
}

/// The simulator, with the booted app's `GET_INFO` tail (image_len, crc32) misreported: an app
/// that runs and names the right version, but whose `app_valid` record is not the one flashed.
struct TailLiar {
    sim: SimNb,
    decoder: hdlc::Decoder,
    pending: std::collections::VecDeque<u8>,
}

#[async_trait::async_trait]
impl FlashPort for TailLiar {
    async fn set_line(&mut self, line: Line) -> Result<()> {
        self.sim.set_line(line).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.sim.write_all(bytes).await
    }

    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        // Only the booted app's HDLC is rewritten; the bootloader's bytes pass untouched.
        let booted = !self.sim.gos.is_empty() && self.sim.line_changes.last() == Some(&Line::Hdlc);
        if !booted {
            return self.sim.read(buf, timeout).await;
        }
        let mut raw = [0u8; 512];
        let n = self.sim.read(&mut raw, timeout).await?;
        for mut f in self.decoder.push(&raw[..n]) {
            let get_info = modem::RSP_FLAG | modem::cmd::GET_INFO;
            if f.frame_type == get_info && f.body.len() >= 8 {
                let at = f.body.len() - 4;
                f.body[at] ^= 0x01;
            }
            self.pending
                .extend(hdlc::encode(f.frame_type, f.seq, &f.body));
        }
        let n = buf.len().min(self.pending.len());
        for (b, o) in buf.iter_mut().zip(self.pending.drain(..n)) {
            *b = o;
        }
        Ok(n)
    }

    async fn discard_input(&mut self) -> Result<()> {
        self.pending.clear();
        self.sim.discard_input().await
    }
}

#[tokio::test]
async fn a_booted_app_whose_get_info_tail_mismatches_fails_the_boot_check() {
    let (img, m) = new();
    let mut port = TailLiar {
        sim: running(SimConfig::default()),
        decoder: hdlc::Decoder::new(),
        pending: Default::default(),
    };
    let mut seen = Vec::new();
    let e = update_northbridge(&mut port, &img, &m, &fast(), &mut |p: &Progress| {
        seen.push(p.clone())
    })
    .await
    .expect_err("the tail does not match the manifest");
    assert_eq!(e.kind(), Kind::Transport);
    let want = format!(
        "GET_INFO reports image {} B crc32 {:#010x}, the manifest {} B {:#010x}",
        m.image_len,
        m.crc32 ^ 0x01,
        m.image_len,
        m.crc32
    );
    assert!(e.message().contains(&want), "{}", e.message());
    assert!(matches!(
        seen.last(),
        Some(Progress::Failed {
            step: Step::BootCheck,
            ..
        })
    ));
    // The flash itself is right: only the report was wrong.
    assert_flashed(&port.sim, &img, &m);
}

/// U5 (2026-10-09): `VerifyMode::Both` on a bootloader that `NACK`s Get Checksum's size used
/// to fail the verify and leave the Northbridge in the ROM bootloader; it reads back instead.
#[tokio::test]
async fn verify_both_reads_back_when_the_checksum_size_is_nacked() {
    let (img, m) = new();
    let mut sim = running(SimConfig {
        checksum: ChecksumSupport::NackSize,
        ..SimConfig::default()
    });
    let opts = Options {
        verify: VerifyMode::Both,
        ..fast()
    };
    let (r, _) = run(&mut sim, &img, &m, &opts).await;
    assert_eq!(r.expect("succeeds").verified, Verified::ReadBack);
    assert_flashed(&sim, &img, &m);
}
