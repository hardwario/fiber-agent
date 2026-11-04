//! The running factory test: one session at a time, driven by the front-panel
//! buttons (via [`on_press`]) and by the control socket (start/status/fail/stop).
//! The display and LED threads ask [`frame`] and [`owns_leds`] whether the test
//! has the hardware.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::machine::{Action, Btn, Machine, Outcome, Step};
use super::render::Frame;
use super::scan::{self, Report, ScanResults};
use crate::drivers::StmBridge;
use crate::libs::buzzer::{BuzzerController, BuzzerPattern};
use crate::libs::config::BuzzerTiming;
use crate::libs::display::supervise::lock_recover;
use crate::libs::power::SharedPowerStatus;

/// The handles the test drives, registered once by `main`.
pub struct Hardware {
    pub stm: Arc<Mutex<StmBridge>>,
    pub buzzer: Arc<Mutex<BuzzerController>>,
    pub power: SharedPowerStatus,
    /// The accelerometer's bus, as the accelerometer monitor is configured.
    pub accel_i2c: String,
}

static HW: OnceLock<Hardware> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LEDS_OWNED: AtomicBool = AtomicBool::new(false);
static LEDS_DIRTY: AtomicBool = AtomicBool::new(false);
/// Bumped to cancel a running LED sequence.
static LED_GEN: AtomicU64 = AtomicU64::new(0);
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// No press and no poll from the app for this long ends the test.
const IDLE_ABORT: Duration = Duration::from_secs(10 * 60);
/// The app polls about every 300 ms; silence this long means it went away.
const APP_GONE_ABORT: Duration = Duration::from_secs(90);
const BLINK_PERIOD_MS: u128 = 500;

struct Session {
    machine: Machine,
    started: Instant,
    last_activity: Instant,
    last_poll: Instant,
    scan: Arc<Mutex<ScanResults>>,
    report: Arc<Mutex<Option<Report>>>,
    saved_volume: Option<u8>,
}

pub fn register(hw: Hardware) {
    let _ = HW.set(hw);
}

/// True while a factory test owns the display and the buttons.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// True while the LED step drives the LEDs; the LED monitor stands aside.
pub fn owns_leds() -> bool {
    LEDS_OWNED.load(Ordering::Relaxed)
}

/// True once after the test released the LEDs: the LED monitor must resend
/// every LED, since the test left them in an unknown state.
pub fn take_leds_dirty() -> bool {
    LEDS_DIRTY.swap(false, Ordering::Relaxed)
}

/// A front-panel press while the test is active.
pub fn on_press(b: Btn) {
    let actions = {
        let mut guard = lock_recover(&SESSION);
        let Some(s) = guard.as_mut() else { return };
        s.last_activity = Instant::now();
        // The result screen is not finished until the report is in.
        if s.machine.step == Step::Report
            && !lock_recover(&s.report)
                .as_ref()
                .is_some_and(Report::complete)
        {
            return;
        }
        s.machine.press(b)
    };
    execute(actions);
}

/// What the LCD shows now, or `None` when no test runs.
pub fn frame() -> Option<Frame> {
    if !is_active() {
        return None;
    }
    let abort = {
        let guard = lock_recover(&SESSION);
        let s = guard.as_ref()?;
        let now = Instant::now();
        if now.duration_since(s.last_activity) > IDLE_ABORT {
            Some("no activity for 10 minutes")
        } else if now.duration_since(s.last_poll) > APP_GONE_ABORT {
            Some("the provisioning app stopped polling")
        } else {
            None
        }
    };
    if let Some(why) = abort {
        let actions = {
            let mut guard = lock_recover(&SESSION);
            guard
                .as_mut()
                .map(|s| s.machine.abort(why))
                .unwrap_or_default()
        };
        execute(actions);
        return None;
    }
    let guard = lock_recover(&SESSION);
    let s = guard.as_ref()?;
    let m = &s.machine;
    Some(match m.step {
        Step::Buttons => {
            // Title, step and the button to press: a line with every button's
            // mark does not fit the 128 px LCD; the station shows the marks.
            let next = super::machine::BUTTON_ORDER
                .get(m.buttons_done)
                .map(|b| format!("Press {}", format!("{b:?}").to_uppercase()))
                .unwrap_or_default();
            Frame::Text(vec!["FACTORY TEST 1/4".into(), "Buttons".into(), next])
        }
        Step::Display => Frame::Pattern {
            pattern: m.pattern,
            phase: (s.started.elapsed().as_millis() / BLINK_PERIOD_MS) % 2 == 0,
        },
        Step::Buzzer => Frame::Text(vec![
            "FACTORY TEST 3/4".into(),
            format!("Buzzer {}%", m.volume),
            "UP louder DOWN quieter".into(),
            "MIDDLE = OK".into(),
        ]),
        Step::Leds => Frame::Text(vec![
            "FACTORY TEST 4/4".into(),
            "LED sequence".into(),
            "UP/DOWN = again".into(),
            "MIDDLE = OK".into(),
        ]),
        Step::Report => {
            let report = lock_recover(&s.report).clone();
            match report {
                None => Frame::Text(vec!["RESULT".into(), "Evaluating...".into()]),
                Some(r) if !r.complete() => {
                    let waited = s.started.elapsed().as_secs();
                    Frame::Text(vec![
                        format!("P2P TEST {waited} s"),
                        format!("WiFi {}  BLE {}", r.wifi.short(), r.ble.short()),
                        format!("LoRa {}  Bat {}%", r.lorawan.short(), r.battery.percent),
                        "Waiting for P2P".into(),
                        format!("frames: {}", r.p2p.count),
                    ])
                }
                Some(r) => {
                    let pass = m.interactive_passed() && r.passed();
                    Frame::Text(vec![
                        format!("RESULT {}", if pass { "PASS" } else { "FAIL" }),
                        format!("WiFi {}  BLE {}", r.wifi.short(), r.ble.short()),
                        format!("P2P {}  LoRa {}", r.p2p.short(), r.lorawan.short()),
                        format!("Bat {} mV {}%", r.battery.vbat_mv, r.battery.percent),
                        "MIDDLE = finish".into(),
                    ])
                }
            }
        }
        Step::Done => return None,
    })
}

/// Starts a test; refuses while one runs.
pub fn start() -> Result<Value, String> {
    if HW.get().is_none() {
        return Err("factory test hardware not registered".into());
    }
    let mut guard = lock_recover(&SESSION);
    if is_active() {
        return Err("a factory test is already running".into());
    }
    let now = Instant::now();
    let scan = Arc::new(Mutex::new(ScanResults::default()));
    scan::start(scan.clone());
    *guard = Some(Session {
        machine: Machine::new(),
        started: now,
        last_activity: now,
        last_poll: now,
        scan,
        report: Arc::new(Mutex::new(None)),
        saved_volume: None,
    });
    ACTIVE.store(true, Ordering::Relaxed);
    let s = guard.as_ref().expect("just set");
    Ok(snapshot(s, 0))
}

/// The current state plus the events after `since`. Also the app's heartbeat.
pub fn status(since: u64) -> Result<Value, String> {
    let mut guard = lock_recover(&SESSION);
    let s = guard.as_mut().ok_or("no factory test has run since boot")?;
    s.last_poll = Instant::now();
    Ok(snapshot(s, since))
}

/// The operator fails the current step from the app.
pub fn fail(note: &str) -> Result<Value, String> {
    let actions = {
        let mut guard = lock_recover(&SESSION);
        let s = guard
            .as_mut()
            .filter(|_| is_active())
            .ok_or("no factory test is running")?;
        s.last_activity = Instant::now();
        s.machine.fail(note)
    };
    execute(actions);
    status(0)
}

pub fn stop() -> Result<Value, String> {
    let actions = {
        let mut guard = lock_recover(&SESSION);
        let s = guard.as_mut().ok_or("no factory test has run since boot")?;
        s.machine.abort("stopped from the app")
    };
    execute(actions);
    status(0)
}

fn snapshot(s: &Session, since: u64) -> Value {
    let m = &s.machine;
    let results: serde_json::Map<String, Value> = Step::INTERACTIVE
        .iter()
        .map(|step| {
            let v = match m.outcome(*step) {
                Some(Outcome::Pass) => "pass",
                Some(Outcome::Fail) => "fail",
                None => "pending",
            };
            (
                serde_json::to_value(step)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                json!(v),
            )
        })
        .collect();
    let report = lock_recover(&s.report).clone();
    let passed = report
        .as_ref()
        .filter(|r| r.complete())
        .map(|r| m.interactive_passed() && r.passed());
    json!({
        "active": is_active(),
        "step": m.step,
        "prompt": m.prompt(),
        "pattern": m.pattern,
        "volume": m.volume,
        "buttons_done": m.buttons_done,
        "results": results,
        "scan": *lock_recover(&s.scan),
        "report": report,
        "passed": passed,
        "elapsed_s": s.started.elapsed().as_secs(),
        "events": m.events_since(since),
    })
}

fn execute(actions: Vec<Action>) {
    let Some(hw) = HW.get() else { return };
    for a in actions {
        match a {
            Action::BuzzerOn { volume } => {
                let b = lock_recover(&hw.buzzer);
                let mut guard = lock_recover(&SESSION);
                if let Some(s) = guard.as_mut() {
                    s.saved_volume.get_or_insert(b.get_volume());
                }
                b.set_volume(volume);
                b.set_repeating_pattern(BuzzerPattern::CriticalBeep(BuzzerTiming {
                    on_ms: 250,
                    off_ms: 500,
                }));
            }
            Action::BuzzerVolume(v) => lock_recover(&hw.buzzer).set_volume(v),
            Action::BuzzerOff => restore_buzzer(hw),
            Action::PlayLeds => play_leds(hw),
            Action::StopLeds => release_leds(),
            Action::Report => {
                let (scan, report) = {
                    let guard = lock_recover(&SESSION);
                    match guard.as_ref() {
                        Some(s) => (s.scan.clone(), s.report.clone()),
                        None => continue,
                    }
                };
                let power = hw.power.clone();
                let accel_i2c = hw.accel_i2c.clone();
                thread::spawn(move || {
                    let partial = report.clone();
                    let r = scan::report(&scan, &power, &accel_i2c, move |r| {
                        *lock_recover(&partial) = Some(r);
                    });
                    *lock_recover(&report) = Some(r);
                });
            }
            Action::Exit => {
                // An abort skips the report; the radio listeners stop here instead.
                if let Some(s) = lock_recover(&SESSION).as_ref() {
                    scan::stop(&s.scan);
                }
                restore_buzzer(hw);
                release_leds();
                ACTIVE.store(false, Ordering::Relaxed);
            }
        }
    }
}

fn restore_buzzer(hw: &Hardware) {
    let b = lock_recover(&hw.buzzer);
    b.stop();
    let saved = lock_recover(&SESSION)
        .as_mut()
        .and_then(|s| s.saved_volume.take());
    if let Some(v) = saved {
        b.set_volume(v);
    }
}

fn release_leds() {
    LED_GEN.fetch_add(1, Ordering::Relaxed);
    if LEDS_OWNED.swap(false, Ordering::Relaxed) {
        LEDS_DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Each line LED in green, red and yellow, one after another, then the power
/// LED in its three colours. A newer [`play_leds`] or [`release_leds`] cancels it.
fn play_leds(hw: &Hardware) {
    LEDS_OWNED.store(true, Ordering::Relaxed);
    let gen = LED_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let stm = hw.stm.clone();
    thread::spawn(move || {
        let live = || LED_GEN.load(Ordering::Relaxed) == gen;
        let send = |f: &dyn Fn(&mut StmBridge) -> std::io::Result<()>| {
            if let Err(e) = f(&mut lock_recover(&stm)) {
                eprintln!("[FactoryTest] LED command failed: {e}");
            }
        };
        let pause = |ms: u64| thread::sleep(Duration::from_millis(ms));
        for i in 0..8u8 {
            send(&|s| s.set_led_state(i, 'O', 'S'));
        }
        send(&|s| s.set_pwr_led_state('O', 'S'));
        for color in ['G', 'R', 'Y'] {
            for i in 0..8u8 {
                if !live() {
                    return;
                }
                send(&|s| s.set_led_state(i, color, 'S'));
                pause(150);
            }
            pause(400);
            for i in 0..8u8 {
                send(&|s| s.set_led_state(i, 'O', 'S'));
            }
        }
        for color in ['G', 'Y', 'L'] {
            if !live() {
                return;
            }
            send(&|s| s.set_pwr_led_state(color, 'S'));
            pause(700);
        }
        send(&|s| s.set_pwr_led_state('O', 'S'));
    });
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *lock_recover(&SESSION) = None;
    ACTIVE.store(false, Ordering::Relaxed);
}
