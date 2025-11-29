// src/runtime.rs
use crate::alarms::actions::AlarmActionSink;
use crate::alarms::engine::AlarmSeverity;
use crate::audit::{AuditEntry, AuditSink};
use crate::logging::{LogEntry, LogLevel, LogSink};
use crate::storage::TimeSeriesStore;
use crate::system::SensorSystem;
use crate::ui::UiSink;
use chrono::{DateTime, Utc};

/// Result of one runtime tick.
#[derive(Debug)]
pub struct RuntimeTickResult {
    pub readings_stored: usize,
    pub alarm_events_logged: usize,
    pub alarm_events_dispatched: usize,
    pub alarm_events: Vec<crate::alarms::engine::AlarmEvent>,
}

pub struct Runtime<S: TimeSeriesStore, L: LogSink, A: AlarmActionSink, U: UiSink> {
    system: SensorSystem,
    store: S,
    logger: L,
    actions: A,
    ui: U,
}

impl<S: TimeSeriesStore, L: LogSink, A: AlarmActionSink, U: UiSink> Runtime<S, L, A, U> {
    pub fn new(system: SensorSystem, store: S, logger: L, actions: A, ui: U) -> Self {
        Self {
            system,
            store,
            logger,
            actions,
            ui,
        }
    }

    /// One logical "cycle" of the system.
    pub fn tick(&mut self, now: DateTime<Utc>) -> RuntimeTickResult {
        let system_result = self.system.tick(now);

        let num_readings = system_result.readings.len();
        if num_readings > 0 {
            self.store.insert_batch(&system_result.readings);
        }

        let mut logged = 0usize;
        let mut dispatched = 0usize;

        for ev in system_result.alarm_events.iter() {
            let level = match ev.severity {
                AlarmSeverity::P1Critical => LogLevel::Critical,
                AlarmSeverity::P2Warning => LogLevel::Warn,
                AlarmSeverity::P3Info => LogLevel::Info,
            };

            let msg = format!(
                "Alarm {:?} -> {:?} ({:?}) value={:.3}",
                ev.from, ev.to, ev.reason, ev.value
            );

            self.logger.log(LogEntry::new(ev.ts_utc, level, "alarm", msg));
            logged += 1;

            self.actions.on_alarm(ev);
            dispatched += 1;
        }

        self.ui
            .on_update(now, &system_result.readings, &system_result.alarm_events);

        RuntimeTickResult {
            readings_stored: num_readings,
            alarm_events_logged: logged,
            alarm_events_dispatched: dispatched,
            alarm_events: system_result.alarm_events,
        }
    }

    /// Record all alarm events to audit log
    pub fn record_alarms_to_audit(
        &self,
        alarm_events: &[crate::alarms::engine::AlarmEvent],
        audit: &mut dyn AuditSink,
    ) {
        for ev in alarm_events.iter() {
            let audit_entry = AuditEntry::from(ev);
            if let Err(e) = audit.record_event(audit_entry) {
                eprintln!("[audit] Failed to record alarm event: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquisition::{AcquisitionConfig, SensorBackend};
    use crate::alarms::actions::{NoopAlarmActionSink, RecordingAlarmActionSink};
    use crate::alarms::engine::{AlarmConfig, AlarmState};
    use crate::logging::{InMemoryLogger, LogLevel};
    use crate::model::{ReadingQuality, SensorId};
    use crate::storage::{InMemoryTimeSeriesStore, SqliteTimeSeriesStore};
    use crate::system::{GenericSensorNode, SensorSystem};
    use crate::ui::NoopUiSink;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};

    fn t(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    #[derive(Debug)]
    struct FakeBackend {
        id: SensorId,
        values: Vec<f32>,
        index: usize,
        quality: ReadingQuality,
    }

    impl FakeBackend {
        fn new(id: SensorId, values: Vec<f32>) -> Self {
            Self {
                id,
                values,
                index: 0,
                quality: ReadingQuality::Ok,
            }
        }
    }

    impl SensorBackend for FakeBackend {
        fn sensor_id(&self) -> SensorId {
            self.id
        }

        fn read(&mut self, _now: DateTime<Utc>) -> (f32, ReadingQuality) {
            let v = if self.index < self.values.len() {
                let v = self.values[self.index];
                self.index += 1;
                v
            } else {
                self.values.last().copied().unwrap_or(0.0)
            };
            (v, self.quality)
        }
    }

    #[test]
    fn runtime_stores_readings_logs_and_dispatches_alarm_events() {
        let acq_cfg = AcquisitionConfig::hundred_per_minute();

        // Values: normal -> warning -> critical
        let backend = FakeBackend::new(SensorId(1), vec![25.0, 32.0, 45.0]);
        let alarm_cfg = AlarmConfig {
            warning_low: None,
            warning_high: Some(30.0),
            critical_low: None,
            critical_high: Some(40.0),
            hysteresis: 0.5,
        };

        let node = Box::new(GenericSensorNode::new(backend, acq_cfg, alarm_cfg));

        let mut system = SensorSystem::new();
        system.add_node(node);

        // Hot retention 60 seconds
        let store = InMemoryTimeSeriesStore::new(ChronoDuration::seconds(60));
        let logger = InMemoryLogger::new(LogLevel::Trace);
        let actions = RecordingAlarmActionSink::new();
        let ui = NoopUiSink;

        let mut runtime = Runtime::new(system, store, logger, actions, ui);

        // Tick at 0, 600, 1200 ms so we get 3 samples.
        let mut now = t(0);
        for _ in 0..3 {
            let _res = runtime.tick(now);
            now = now + acq_cfg.sample_interval;
        }

        // Pull out the inner pieces for inspection.
        let Runtime {
            system: _,
            store,
            logger,
            actions,
            ui: _,
        } = runtime;

        // 3 readings stored
        let readings = store.query_range(SensorId(1), t(0), t(5000));
        assert_eq!(readings.len(), 3);
        assert_eq!(readings[0].value, 25.0);
        assert_eq!(readings[1].value, 32.0);
        assert_eq!(readings[2].value, 45.0);

        // At least two alarm transitions:
        // NORMAL -> WARNING (cross 30)
        // WARNING -> CRITICAL (cross 40)
        let entries = logger.entries();
        assert!(
            entries.iter().any(|e| e.message.contains("Normal") && e.message.contains("Warning"))
                || entries.iter().any(|e| e.message.contains("WARNING")),
            "Expected some warning-related alarm log; got {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.message.contains("Critical") || e.message.contains("CRITICAL")),
            "Expected some critical-related alarm log; got {entries:?}"
        );

        // Actions sink should have received the same number of events as logs.
        let act_events = actions.events();
        assert!(!act_events.is_empty());

        // Confirm at least one WARNING and one CRITICAL transition recorded.
        assert!(act_events.iter().any(|e| e.from == AlarmState::Normal && e.to == AlarmState::Warning));
        assert!(act_events.iter().any(|e| e.from == AlarmState::Warning && e.to == AlarmState::Critical));
    }

    #[test]
    fn runtime_generic_compiles_with_sqlite_components_and_noop_actions() {
        // This test exists only to prove the generic Runtime can be
        // instantiated with SQLite-based components and a no-op action sink.
        let system = SensorSystem::new();
        let store =
            SqliteTimeSeriesStore::open_in_memory(ChronoDuration::seconds(60)).expect("open sqlite");
        let logger =
            crate::logging::SqliteLogSink::open_in_memory().expect("open sqlite log");
        let actions = NoopAlarmActionSink;
        let ui = NoopUiSink;

        let _runtime: Runtime<
            SqliteTimeSeriesStore,
            crate::logging::SqliteLogSink,
            NoopAlarmActionSink,
            NoopUiSink,
        > = Runtime::new(system, store, logger, actions, ui);
    }
}
