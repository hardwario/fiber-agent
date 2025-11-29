// src/storage.rs
use crate::model::{SensorId, SensorReading};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rusqlite::Connection;

/// Abstract interface for storing/querying sensor readings.
///
/// Later, we'll implement this with SQLite (hot tier),
/// and additional layers for warm/cold archives.
pub trait TimeSeriesStore {
    /// Insert a single reading.
    fn insert(&mut self, reading: SensorReading);

    /// Insert a batch of readings (default just calls insert).
    fn insert_batch(&mut self, readings: &[SensorReading]) {
        for r in readings {
            self.insert(*r);
        }
    }

    /// Query readings for a given sensor_id and time range [start, end).
    fn query_range(
        &self,
        sensor_id: SensorId,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<SensorReading>;
}

/// Simple in-memory implementation representing a "hot" store with retention.
pub struct InMemoryTimeSeriesStore {
    retention: ChronoDuration,
    data: Vec<SensorReading>,
}

impl InMemoryTimeSeriesStore {
    pub fn new(retention: ChronoDuration) -> Self {
        Self {
            retention,
            data: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Remove rows older than (latest_ts - retention).
    fn prune(&mut self) {
        if self.data.is_empty() {
            return;
        }

        // Latest timestamp in data
        let latest_ts = self
            .data
            .iter()
            .map(|r| r.ts_utc)
            .max()
            .unwrap_or_else(|| Utc::now());

        let cutoff = latest_ts - self.retention;

        self.data.retain(|r| r.ts_utc >= cutoff);
    }
}

impl TimeSeriesStore for InMemoryTimeSeriesStore {
    fn insert(&mut self, reading: SensorReading) {
        self.data.push(reading);
        self.prune();
    }

    fn query_range(
        &self,
        sensor_id: SensorId,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<SensorReading> {
        self.data
            .iter()
            .cloned()
            .filter(|r| r.sensor_id == sensor_id && r.ts_utc >= start && r.ts_utc < end)
            .collect()
    }
}

/// SQLite-backed implementation of TimeSeriesStore.
///
/// Schema:
///   sensor_readings(
///     sensor_id   INTEGER NOT NULL,
///     ts_utc_ms   INTEGER NOT NULL,
///     value       REAL NOT NULL,
///     quality     INTEGER NOT NULL
///   )
pub struct SqliteTimeSeriesStore {
    conn: Connection,
    retention: ChronoDuration,
}

impl SqliteTimeSeriesStore {
    /// Open a file-backed DB, enabling WAL.
    pub fn open_file(
        path: impl AsRef<std::path::Path>,
        retention: ChronoDuration,
    ) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_conn(&conn)?;
        Ok(Self { conn, retention })
    }

    /// Open an in-memory DB (for tests / demos).
    pub fn open_in_memory(retention: ChronoDuration) -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_conn(&conn)?;
        Ok(Self { conn, retention })
    }

    fn init_conn(conn: &Connection) -> rusqlite::Result<()> {
        // Same pragmas as log sink.
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        conn.pragma_update(None, "synchronous", &"NORMAL")?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sensor_readings (
                sensor_id   INTEGER NOT NULL,
                ts_utc_ms   INTEGER NOT NULL,
                value       REAL NOT NULL,
                quality     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sensor_readings_sid_ts
                ON sensor_readings (sensor_id, ts_utc_ms);
            "#,
        )?;
        Ok(())
    }

    fn prune(&self) -> rusqlite::Result<()> {
        // Get latest timestamp (ms).
        let latest_ts_opt: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(ts_utc_ms) FROM sensor_readings",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten();

        let Some(latest_ts) = latest_ts_opt else {
            return Ok(()); // no data yet
        };

        let cutoff_ms = latest_ts - self.retention.num_milliseconds();

        self.conn
            .execute(
                "DELETE FROM sensor_readings WHERE ts_utc_ms < ?1",
                (&cutoff_ms,),
            )
            .map(|_| ())
    }

    #[cfg(test)]
    pub fn count_rows(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM sensor_readings", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }
}

fn quality_to_i32(q: crate::model::ReadingQuality) -> i32 {
    use crate::model::ReadingQuality::*;
    match q {
        Ok => 0,
        CrcError => 1,
        Timeout => 2,
        Disconnected => 3,
        Other => 4,
    }
}

fn quality_from_i32(v: i32) -> crate::model::ReadingQuality {
    use crate::model::ReadingQuality::*;
    match v {
        0 => Ok,
        1 => CrcError,
        2 => Timeout,
        3 => Disconnected,
        _ => Other,
    }
}

impl TimeSeriesStore for SqliteTimeSeriesStore {
    fn insert(&mut self, reading: SensorReading) {
        let sid = reading.sensor_id.0 as i64;
        let ts_ms = reading.ts_utc.timestamp_millis();
        let quality = quality_to_i32(reading.quality);

        self.conn
            .execute(
                "INSERT INTO sensor_readings (sensor_id, ts_utc_ms, value, quality)
                 VALUES (?1, ?2, ?3, ?4)",
                (sid, ts_ms, reading.value as f64, quality),
            )
            .expect("failed to insert sensor reading into SQLite");

        self.prune().expect("failed to prune sensor_readings");
    }

    fn query_range(
        &self,
        sensor_id: SensorId,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<SensorReading> {
        use chrono::TimeZone;

        let sid = sensor_id.0 as i64;
        let start_ms = start.timestamp_millis();
        let end_ms = end.timestamp_millis();

        let mut stmt = self
            .conn
            .prepare(
                "SELECT sensor_id, ts_utc_ms, value, quality
                 FROM sensor_readings
                 WHERE sensor_id = ?1
                   AND ts_utc_ms >= ?2
                   AND ts_utc_ms < ?3
                 ORDER BY ts_utc_ms",
            )
            .expect("failed to prepare query_range statement");

        let mut rows = stmt
            .query((sid, start_ms, end_ms))
            .expect("failed to query sensor_readings");

        let mut out = Vec::new();
        while let Some(row) = rows.next().expect("row iteration failed") {
            let sid_i: i64 = row.get(0).expect("sid");
            let ts_ms: i64 = row.get(1).expect("ts");
            let value: f64 = row.get(2).expect("value");
            let quality_i: i32 = row.get(3).expect("quality");

            let ts_utc = Utc.timestamp_millis_opt(ts_ms).unwrap();
            out.push(SensorReading {
                sensor_id: SensorId(sid_i as u64),
                ts_utc,
                value: value as f32,
                quality: quality_from_i32(quality_i),
            });
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReadingQuality, SensorId, SensorReading};
    use chrono::{TimeZone, Utc};

    fn t(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    fn reading(sensor_id: u64, value: f32, ts_ms: i64) -> SensorReading {
        SensorReading {
            sensor_id: SensorId(sensor_id),
            ts_utc: t(ts_ms),
            value,
            quality: ReadingQuality::Ok,
        }
    }

    #[test]
    fn insert_and_query_range_in_memory_store() {
        let retention = ChronoDuration::seconds(60); // 1 minute
        let mut store = InMemoryTimeSeriesStore::new(retention);

        store.insert(reading(1, 10.0, 0));
        store.insert(reading(1, 11.0, 1000));
        store.insert(reading(2, 20.0, 1000)); // different sensor

        let res = store.query_range(SensorId(1), t(0), t(2000));
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].value, 10.0);
        assert_eq!(res[1].value, 11.0);

        let res2 = store.query_range(SensorId(2), t(0), t(2000));
        assert_eq!(res2.len(), 1);
        assert_eq!(res2[0].value, 20.0);
    }

    #[test]
    fn retention_prunes_old_data_in_memory() {
        let retention = ChronoDuration::milliseconds(1500); // 1.5s
        let mut store = InMemoryTimeSeriesStore::new(retention);

        store.insert(reading(1, 10.0, 0)); // t=0
        store.insert(reading(1, 11.0, 1000)); // t=1000
        store.insert(reading(1, 12.0, 3000)); // t=3000, now retention applied

        // latest_ts = 3000 => cutoff = 3000 - 1500 = 1500
        // r@0 and r@1000 should be pruned; only r@3000 remains.
        assert_eq!(store.len(), 1);

        let res = store.query_range(SensorId(1), t(0), t(5000));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].value, 12.0);
        assert_eq!(res[0].ts_utc, t(3000));
    }

    #[test]
    fn sqlite_store_insert_query_and_retention() {
        let retention = ChronoDuration::milliseconds(1500); // 1.5s
        let mut store =
            SqliteTimeSeriesStore::open_in_memory(retention).expect("open in-memory sqlite");

        store.insert(reading(1, 10.0, 0)); // t=0
        store.insert(reading(1, 11.0, 1000)); // t=1000
        store.insert(reading(1, 12.0, 3000)); // t=3000

        // latest_ts = 3000 => cutoff = 1500 => only last row should remain.
        assert_eq!(store.count_rows(), 1);

        let res = store.query_range(SensorId(1), t(0), t(5000));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].value, 12.0);
        assert_eq!(res[0].ts_utc, t(3000));
    }
}
