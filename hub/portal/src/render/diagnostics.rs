use super::*;

// ---------------------------------------------------------------------
// Diagnostics tab cards (plan/portal/ui_components.md, amendment
// 2026-10-15): the "Alarm messages" and "Message history" rows shared by
// the Node, Beacon and Hub pages. Templates: partials/diag_alarms_card.html
// and partials/diag_messages_card.html.
// ---------------------------------------------------------------------

/// One alarm-or-clear line of the "Alarm messages" card. An `alarm_events`
/// row is one alarm with its own end, so a cleared alarm gives two lines.
#[derive(Debug, Clone, Serialize)]
pub struct DiagAlarmCtx {
    /// RFC 3339, for the `short_dt` filter.
    pub at: String,
    /// `alarm` | `clear`.
    pub kind: &'static str,
    /// The quantity the alarm is on: the `sensor_id` tail after the last `:`.
    pub field: Option<String>,
    pub alarm_type: String,
    pub severity: String,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
}

/// Alarm and clear lines, newest first, at most `limit`.
pub fn diag_alarm_events(alarms: &[portal_storage::AlarmEvent], limit: usize) -> Vec<DiagAlarmCtx> {
    let mut out = Vec::new();
    for a in alarms {
        let field = a
            .sensor_id
            .rsplit_once(':')
            .map(|(_, f)| f.to_string())
            .filter(|f| !f.is_empty());
        let line = |at: &str, kind| DiagAlarmCtx {
            at: at.to_string(),
            kind,
            field: field.clone(),
            alarm_type: a.alarm_type.clone(),
            severity: a.max_severity.clone(),
            value: a.value,
            threshold: a.threshold,
        };
        out.push(line(&a.triggered_at, "alarm"));
        if let Some(cleared) = &a.cleared_at {
            out.push(line(cleared, "clear"));
        }
    }
    // RFC 3339 stamps of one writer sort as text; parse anyway, the
    // fractional digits differ between writers.
    let key = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .map(|t| t.unix_timestamp_nanos())
            .unwrap_or(0)
    };
    out.sort_by_key(|l| std::cmp::Reverse(key(&l.at)));
    out.truncate(limit);
    out
}

/// One row of the "Message history" card.
#[derive(Debug, Clone, Serialize)]
pub struct DiagMessageCtx {
    /// `up` | `down`.
    pub dir: &'static str,
    /// Unix seconds, for ordering.
    pub ts: i64,
    pub at: Option<String>,
    pub port: Option<i64>,
    /// Bytes on air, when the stored message carries its body.
    pub size: Option<usize>,
    pub kind: String,
    /// `k=v · k=v`, the decoded values.
    pub summary: String,
    pub hex: Option<String>,
    /// The stored message, pretty-printed.
    pub json: String,
    /// A downlink's outcome as audited; `None` for an uplink.
    pub ok: Option<bool>,
}

const HEX_KEYS: [&str; 4] = ["body_hex", "payload_hex", "raw_hex", "data_hex"];

fn pretty(text: &str) -> (Option<serde_json::Value>, String) {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => {
            let s = serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string());
            (Some(v), s)
        }
        Err(_) => (None, text.to_string()),
    }
}

/// A `node_raw_messages` row (always an uplink: the ring stores what arrived).
pub fn diag_uplink_ctx(row: &portal_storage::NodeRawMessage) -> DiagMessageCtx {
    let m = node_message_ctx(row);
    let (value, json) = pretty(&row.payload);
    let hex = value.as_ref().and_then(|v| {
        HEX_KEYS
            .iter()
            .find_map(|k| v.get(*k).and_then(|h| h.as_str()))
            .map(|h| h.to_ascii_lowercase())
    });
    let summary = m
        .values
        .iter()
        .map(|(k, v)| format!("{k}={}", v.to_string().trim_matches('"')))
        .collect::<Vec<_>>()
        .join(" · ");
    DiagMessageCtx {
        dir: "up",
        ts: row.received_at,
        at: m.received_at,
        port: m.port,
        size: hex.as_ref().map(|h| h.len() / 2),
        kind: m.kind,
        summary,
        hex,
        json,
        ok: None,
    }
}

/// A command the portal sent this device, as the audit log recorded it: only
/// what went out through the radio seam (`transport: control.radio`) or as a
/// pending-config send. `None` for any other audit row.
pub fn diag_downlink_ctx(e: &portal_storage::AuditEntry) -> Option<DiagMessageCtx> {
    let details = e.details.as_deref().unwrap_or("{}");
    let (value, json) = pretty(details);
    let radio = value
        .as_ref()
        .and_then(|v| v.get("transport"))
        .and_then(|t| t.as_str())
        == Some("control.radio");
    if !radio && e.event_type != "node_pending_send" {
        return None;
    }
    // audit_log.ts is SQLite's `YYYY-MM-DD HH:MM:SS`, in UTC.
    let at = format!("{}Z", e.ts.replacen(' ', "T", 1));
    let ts = time::OffsetDateTime::parse(&at, &time::format_description::well_known::Rfc3339)
        .map(|t| t.unix_timestamp())
        .unwrap_or(0);
    let summary = value
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter(|(k, v)| k.as_str() != "transport" && !v.is_object() && !v.is_array())
                .map(|(k, v)| format!("{k}={}", v.to_string().trim_matches('"')))
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default();
    Some(DiagMessageCtx {
        dir: "down",
        ts,
        at: Some(at),
        port: None,
        size: None,
        kind: e.event_type.clone(),
        summary,
        hex: None,
        json,
        ok: Some(e.success),
    })
}

/// A message kept only as one JSON document (a Beacon's last `eye/sensors`
/// tag): an uplink with its values, no port and no bytes.
pub fn diag_json_message(at: Option<String>, json: &str) -> DiagMessageCtx {
    let (value, json) = pretty(json);
    let summary = value
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter(|(_, v)| !v.is_object() && !v.is_array())
                .map(|(k, v)| format!("{k}={}", v.to_string().trim_matches('"')))
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default();
    DiagMessageCtx {
        dir: "up",
        ts: 0,
        at,
        port: None,
        size: None,
        kind: "eye/sensors".to_string(),
        summary,
        hex: None,
        json,
        ok: None,
    }
}

/// Uplinks and downlinks in one list, newest first.
pub fn diag_messages(
    mut up: Vec<DiagMessageCtx>,
    down: Vec<DiagMessageCtx>,
) -> Vec<DiagMessageCtx> {
    up.extend(down);
    up.sort_by_key(|m| std::cmp::Reverse(m.ts));
    up
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(sensor_id: &str, at: &str, cleared: Option<&str>) -> portal_storage::AlarmEvent {
        portal_storage::AlarmEvent {
            id: at.to_string(),
            hostname: "h".into(),
            sensor_id: sensor_id.into(),
            sensor_name: None,
            sensor_location: None,
            alarm_type: "HIGH".into(),
            severity: "warning".into(),
            from_state: None,
            to_state: None,
            message: None,
            value: Some(30.0),
            threshold: Some(28.0),
            triggered_at: at.into(),
            cleared_at: cleared.map(str::to_string),
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
            created_at: at.into(),
            max_severity: "warning".into(),
            end_reason: None,
        }
    }

    #[test]
    fn a_cleared_alarm_gives_two_lines_newest_first() {
        let rows = [
            alarm(
                "lrw:aa:temperature",
                "2026-10-14T10:00:00Z",
                Some("2026-10-14T12:00:00Z"),
            ),
            alarm("lrw:aa:humidity", "2026-10-14T11:00:00.5+00:00", None),
        ];
        let lines = diag_alarm_events(&rows, 10);
        let got: Vec<_> = lines.iter().map(|l| (l.kind, l.field.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("clear", Some("temperature")),
                ("alarm", Some("humidity")),
                ("alarm", Some("temperature")),
            ]
        );
        assert_eq!(diag_alarm_events(&rows, 2).len(), 2);
    }

    #[test]
    fn only_radio_commands_are_downlinks() {
        let entry = |event: &str, details: &str| portal_storage::AuditEntry {
            id: 1,
            ts: "2026-10-15 19:29:34".into(),
            event_type: event.into(),
            username: None,
            user_id: None,
            ip: None,
            target: Some("aa".into()),
            details: Some(details.into()),
            success: false,
        };
        let d = diag_downlink_ctx(&entry(
            "node_service_reboot",
            r#"{"confirmed":true,"transport":"control.radio"}"#,
        ))
        .expect("a radio command");
        assert_eq!(d.dir, "down");
        assert_eq!(d.at.as_deref(), Some("2026-10-15T19:29:34Z"));
        assert_eq!(d.summary, "confirmed=true");
        assert_eq!(d.ok, Some(false));
        assert!(diag_downlink_ctx(&entry("node_meta_set", r#"{"name":"x"}"#)).is_none());
    }

    #[test]
    fn an_uplink_shows_its_body_hex_and_size() {
        let row = portal_storage::NodeRawMessage {
            id: 1,
            hostname: "h".into(),
            node_key: "aa".into(),
            received_at: 1_760_000_000,
            payload: r#"{"fport":85,"kind":"info","body_hex":"0A0B0C","fields":{"t":1}}"#.into(),
        };
        let m = diag_uplink_ctx(&row);
        assert_eq!((m.dir, m.port, m.size), ("up", Some(85), Some(3)));
        assert_eq!(m.hex.as_deref(), Some("0a0b0c"));
        assert_eq!(m.summary, "t=1");
        assert!(m.json.contains("\n"), "pretty-printed");
    }
}
