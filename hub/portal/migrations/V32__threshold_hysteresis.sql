-- Threshold grading in the portal, with hysteresis (plan/control/alarm_hysteresis.md,
-- decision #41, GitLab #110).
--
-- 1. `hysteresis`: how far back inside a bound a value has to come before its
--    alarm steps down, in the field's unit. NULL = the default of the field's
--    quantity (`thresholds.defaults.<quantity>.hysteresis`, 0.5 °C / 3 % RH),
--    so every existing row gets the default and follows it until someone sets
--    a value of their own.
-- 2. `scope` admits 'hub': a Hub's 1-Wire line is stored here too, as
--    (hostname, 'hub', line, 'temperature'), so the portal holds and grades the
--    bounds of every source. SQLite cannot widen a CHECK in place; this is the
--    table rebuild V16 and V25 are the precedent for. `field_thresholds` has no
--    index, view or trigger and nothing references it by foreign key.
CREATE TABLE field_thresholds_v32 (
    hostname      TEXT    NOT NULL,
    scope         TEXT    NOT NULL CHECK (scope IN ('node', 'eye', 'hub')),
    entity        TEXT    NOT NULL,
    field         TEXT    NOT NULL,
    critical_low  REAL,
    warning_low   REAL,
    warning_high  REAL,
    critical_high REAL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    updated_at    TEXT,
    hysteresis    REAL CHECK (hysteresis IS NULL OR hysteresis >= 0),
    PRIMARY KEY (hostname, scope, entity, field)
);

INSERT INTO field_thresholds_v32
    (hostname, scope, entity, field,
     critical_low, warning_low, warning_high, critical_high, enabled, updated_at)
SELECT hostname, scope, entity, field,
       critical_low, warning_low, warning_high, critical_high, enabled, updated_at
FROM field_thresholds;

DROP TABLE field_thresholds;
ALTER TABLE field_thresholds_v32 RENAME TO field_thresholds;
