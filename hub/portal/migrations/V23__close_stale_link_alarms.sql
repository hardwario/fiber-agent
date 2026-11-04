-- Closes the Wi-Fi and Ethernet alarms still open at the upgrade (MR !103
-- review; plan/portal/status.md, 2026-09-28).
--
-- A Hub publishes both link flags every time, so a Hub on one uplink reports
-- the other one down forever. The old rule opened `sys:wifi` / `sys:ethernet`
-- on that steady `false`. UX-058's edge rule no longer does, but it only
-- clears a link alarm on `true`, which an unused link never reports, so the
-- rows the old rule opened would stay open, and counted, for good. A real
-- drop still open at the upgrade is closed as well; the Hub page still shows
-- the link's state. No notification goes out for these clears.
UPDATE alarm_events
SET cleared_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
    message = 'Closed on the portal upgrade: link alarms now open only on a drop the portal sees (UX-058).'
WHERE sensor_id IN ('sys:wifi', 'sys:ethernet') AND cleared_at IS NULL;
