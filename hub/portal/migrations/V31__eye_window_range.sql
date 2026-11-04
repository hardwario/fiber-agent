-- Beacon storage-window range (plan/control/storage_intervals.md, amendment
-- 2026-10-12).
--
-- An `advertising` row from `eye/aggregated` is one Hub storage window
-- (600 s by default). `temperature_c` / `humidity_pct` keep the window
-- average, which every chart, summary and export already reads; these
-- columns keep the window's min and max, so a short excursion inside a
-- window (a freezer door left open for five minutes) is not averaged away.
--
-- NULL on every other row: `live` rows of older Portals, `recording` rows
-- from the Beacon archive (one sample each) and windows from a Hub that
-- does not send the range.
--
-- V31: V30 is w1_tilt_rename (!129).
ALTER TABLE eye_readings ADD COLUMN temperature_min_c REAL;
ALTER TABLE eye_readings ADD COLUMN temperature_max_c REAL;
ALTER TABLE eye_readings ADD COLUMN humidity_min_pct INTEGER;
ALTER TABLE eye_readings ADD COLUMN humidity_max_pct INTEGER;
