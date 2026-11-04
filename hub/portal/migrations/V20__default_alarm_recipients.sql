-- The installation's default alarm recipients (plan/decisions.md #25).
--
-- `default_alarm_recipients` — a JSON array of e-mail addresses that gets
-- the sensor-alarm e-mails of every Hub without its own
-- `device_report_config` row. A Hub with a row uses only that row. The
-- defaults send what a new Hub row sends: activation and escalation to
-- critical, not clear or acknowledgement. Empty by default, so nothing
-- changes until someone fills it in.
ALTER TABLE notify_global_config ADD COLUMN default_alarm_recipients TEXT NOT NULL DEFAULT '[]';
