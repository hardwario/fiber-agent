-- Why an alarm ended when nothing measured its end (GitLab: orphaned alarms).
--
-- An alarm whose device was removed, whose Hub is gone or whose bound was
-- removed or switched off never sees the reading that would clear it, so it
-- stayed Active for good. The portal now closes such rows itself and records
-- why here: 'device_removed', 'hub_removed' or 'rule_removed'. NULL is the
-- ordinary end (the value came back, the device answered again).
ALTER TABLE alarm_events ADD COLUMN end_reason TEXT;
