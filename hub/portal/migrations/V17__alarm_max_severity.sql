-- An alarm keeps the worst severity it reached (plan/decisions.md #18).
--
-- `severity` is where an open alarm sits now: an escalation raises it and a
-- fall back to warning lowers it again, in place. Until this column nothing
-- recorded that the row had ever been critical, so a probe that went over its
-- critical bound and came back read afterwards as a plain warning.
--
-- `max_severity` only ever rises. The repo writes it on insert and raises it on
-- every severity change; the alarm list, its severity filter and the metric
-- row read it. Existing rows start from their current severity, which is all
-- that is known about them.
ALTER TABLE alarm_events ADD COLUMN max_severity TEXT;
UPDATE alarm_events SET max_severity = severity;
