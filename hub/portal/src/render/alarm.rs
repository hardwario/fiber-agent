use super::*;

// ---------------------------------------------------------------------
// M6-F: alarm list/detail context builders.
//
// Contracts: templates/partials/alarm_table.html (row shape) and
// templates/partials/alarm_detail_body.html (`a`/`notes`/`can_ack`/
// `duration`/`error`) header comments. Timestamps arrive as RFC3339 strings
// (the alarm engine's `ts_iso` convention) and are re-formatted for display
// here; `from_state`/`to_state` arrive firmware-cased ("WARNING") and are
// normalized to the templates' mixed-case labels ("Warning") via the same
// mapping the radio cards use.
// ---------------------------------------------------------------------

/// `partials/alarm_table.html`'s row shape — also the `a` object of
/// `partials/alarm_detail_body.html` (which additionally reads
/// `from_state`; the table simply ignores it).
#[derive(Debug, Clone, Serialize)]
pub struct AlarmCtx {
    pub id: String,
    pub hostname: String,
    pub sensor_id: String,
    pub sensor_name: Option<String>,
    pub sensor_location: Option<String>,
    pub alarm_type: String,
    pub severity: String,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub message: Option<String>,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub triggered_at: String,
    pub cleared_at: Option<String>,
    pub acknowledged_at: Option<String>,
    pub acknowledged_by_username: Option<String>,
    /// The acknowledger as a reader sees them ([`person_label`]). Plain
    /// username from [`alarm_ctx`]; [`alarm_detail_body_ctx`] resolves the
    /// full name.
    pub acknowledged_by: Option<String>,
    /// Same instants as `triggered_at`/`cleared_at`, kept as raw RFC3339
    /// (not display-formatted) so `partials/alarm_table.html` can render
    /// them through the `short_dt` filter — same "kept verbatim from the
    /// wire" contract as node/beacon `last_seen` (render.rs's `short_dt`
    /// doc comment). `alarm_detail_body.html` keeps using the formatted
    /// fields above; these two exist only for the table's narrower columns.
    pub triggered_at_raw: String,
    pub cleared_at_raw: Option<String>,
    /// Humanized span between trigger and clear, `None` while the alarm is
    /// still active — same rule as `alarm_detail_body_ctx`'s `duration`, so
    /// the table's "Length" column and the detail agree.
    pub duration: Option<String>,
    /// The page of the device that raised it, and the chart window of the
    /// probe it concerns; `None` where there is none (a Hub `sys:*` alarm
    /// has no chart). See [`alarm_links`].
    pub device_href: Option<String>,
    pub chart_href: Option<String>,
    /// What the alarm measures, as a registry field name (`common.field.*`
    /// labels it) and its displayed unit — `None` for an alarm about a device
    /// rather than a reading. See [`crate::alarms::alarm_field`]. `value`,
    /// `threshold` and `peak` are in the same displayed unit ([`alarm_shown`]).
    pub field: Option<String>,
    pub unit: Option<String>,
    /// The Node's or Beacon's own name from the live snapshot. A field
    /// alarm's `sensor_name` is the field's label, not the device's name.
    pub device_name: Option<String>,
    /// The worst severity the alarm reached (decisions.md #24) — what the
    /// list's badge shows. `severity` is where it sits now.
    pub max_severity: String,
    /// Why the portal closed the alarm itself (V37), as the i18n key suffix
    /// `alarms.end_reason.<reason>`; `None` for an ordinary end.
    pub end_reason: Option<String>,
    /// Closed from the Hub's reported state, not from a transition
    /// (decisions.md #27); the template says so in the reader's language
    /// instead of showing the stored English `message`.
    pub reconciled: bool,
    /// The last moment a reconciled row's probe is known NORMAL (RFC 3339,
    /// decisions.md #28b), read back from the row message; `None` on rows
    /// closed before #28b wrote it there.
    pub reconciled_last_normal: Option<String>,
    /// The registry does not know the alarm's Hub (decisions.md #30d): the
    /// row reads "state unknown", and its device and chart links are
    /// dropped, since both pages resolve the device in the registry.
    pub hub_unknown: bool,
    /// The alarm's peak value (decisions.md #24) for the list, beside the
    /// worst-severity badge (UX-025); the routes fill it, `None` otherwise.
    pub peak: Option<f64>,
}

/// `(device page, chart window)` for an alarm. A wired probe's device is
/// its Hub, and a Hub alarm (`sys:*`) has no chart.
///
/// A wired line's chart is its own page on its Hub,
/// `/hubs/{hub}/sensors/line-{n}` (devices.hub.md §3.2, amendment
/// 2026-10-14): every Hub has lines 0–7, and `/monitoring/probe/probe:{line}`
/// names no Hub.
pub fn alarm_links(hostname: &str, sensor_id: &str) -> (Option<String>, Option<String>) {
    use crate::alarms::AlarmTarget;
    match crate::alarms::alarm_target(sensor_id) {
        AlarmTarget::Line(line) => (
            Some(format!("/hubs/{hostname}")),
            u8::try_from(line)
                .ok()
                .map(|line| crate::monitoring::probe_href(hostname, line)),
        ),
        AlarmTarget::Node(key) => (
            Some(format!("/nodes/{key}/overview")),
            Some(format!("/monitoring/probe/node:{key}")),
        ),
        AlarmTarget::Beacon(mac) => (
            Some(format!("/beacons/{mac}/overview")),
            Some(format!("/monitoring/probe/beacon:{mac}")),
        ),
        AlarmTarget::Hub => (Some(format!("/hubs/{hostname}")), None),
    }
}

/// A stored value of `e` (reading, limit, peak) as the portal shows it:
/// a Node's Pa in hPa (UX-038). Machine data (webhook, CSV) keeps Pa.
pub fn alarm_shown(e: &portal_storage::AlarmEvent, stored: f64) -> f64 {
    crate::alarms::alarm_field(&e.sensor_id, e.sensor_name.as_deref())
        .map_or(stored, |f| f.to_display(stored))
}

/// `partials/alarm_detail_body.html`'s notes entry shape.
#[derive(Debug, Clone, Serialize)]
pub struct AlarmNoteCtx {
    pub id: String,
    pub username: String,
    /// `username` as a reader sees it ([`person_label`]).
    pub author: String,
    pub content: String,
    pub created_at: String,
}

/// One [`portal_storage::AlarmEvent`] -> the shared template row/`a` shape:
/// display-formatted timestamps, lowercase `severity` (the templates
/// compare `== "critical"` verbatim), mixed-case states.
pub fn alarm_ctx(e: &portal_storage::AlarmEvent, state: &PortalState) -> AlarmCtx {
    let hub_unknown = !crate::alarms::hub_known(state, &e.hostname);
    let (device_href, chart_href) = match hub_unknown {
        true => (None, None),
        false => alarm_links(&e.hostname, &e.sensor_id),
    };
    let field = crate::alarms::alarm_field(&e.sensor_id, e.sensor_name.as_deref());
    let shown = |v: f64| field.map_or(v, |f| f.to_display(v));
    AlarmCtx {
        id: e.id.clone(),
        hostname: e.hostname.clone(),
        sensor_id: e.sensor_id.clone(),
        sensor_name: e.sensor_name.clone(),
        sensor_location: e.sensor_location.clone(),
        alarm_type: e.alarm_type.clone(),
        severity: e.severity.to_ascii_lowercase(),
        from_state: e.from_state.as_deref().map(normalize_radio_alarm_state),
        to_state: e.to_state.as_deref().map(normalize_radio_alarm_state),
        message: e.message.clone(),
        value: e.value.map(shown),
        threshold: e.threshold.map(shown),
        triggered_at: e.triggered_at.clone(),
        cleared_at: e.cleared_at.clone(),
        acknowledged_at: e.acknowledged_at.clone(),
        acknowledged_by_username: e.acknowledged_by_username.clone(),
        acknowledged_by: e.acknowledged_by_username.clone(),
        triggered_at_raw: e.triggered_at.clone(),
        cleared_at_raw: e.cleared_at.clone(),
        duration: e
            .cleared_at
            .as_deref()
            .and_then(|cleared| alarm_duration(&e.triggered_at, cleared)),
        device_href,
        chart_href,
        field: field.map(|f| f.name.to_string()),
        unit: field.and_then(|f| f.display_unit()).map(str::to_string),
        device_name: crate::alarms::alarm_device_name(&e.sensor_id, state),
        max_severity: e.max_severity.to_ascii_lowercase(),
        end_reason: e.end_reason.clone(),
        reconciled: e
            .message
            .as_deref()
            .is_some_and(portal_domain::alarm_engine::is_wired_reconcile),
        reconciled_last_normal: e
            .message
            .as_deref()
            .and_then(portal_domain::alarm_engine::wired_reconcile_last_normal)
            .map(str::to_string),
        hub_unknown,
        peak: None,
    }
}

/// An alarm's peak over its window (decisions.md #24), as the detail shows
/// it. `at` is known only when a stored reading is the peak;
/// `minute_average` says that reading is a wired line's stored minute
/// average (`readings_minute.avg_c`), not a sampled value, so the label
/// does not pass it off as a raw peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlarmPeak {
    pub value: f64,
    pub at: Option<i64>,
    pub minute_average: bool,
}

/// Everything `partials/alarm_detail_body.html` needs, minus `current_user`
/// (the route's `page_ctx` job). `duration` is triggered->cleared for a
/// cleared alarm and triggered->now for an active one (the partial marks the
/// latter "so far"); see [`detail_duration`]. `full_names` comes from
/// [`alarm_detail_usernames`] resolved in one lookup.
pub fn alarm_detail_body_ctx(
    state: &PortalState,
    a: &portal_storage::AlarmEvent,
    notes: &[portal_storage::AlarmNote],
    full_names: &HashMap<String, String>,
    peak: Option<AlarmPeak>,
    error: Option<String>,
) -> Value {
    // An ended alarm is acknowledged too, to complete the trail (#28d).
    let can_ack = a.acknowledged_at.is_none();
    let duration = detail_duration(&a.triggered_at, a.cleared_at.as_deref(), now_unix());
    let notes: Vec<AlarmNoteCtx> = notes
        .iter()
        .map(|n| AlarmNoteCtx {
            id: n.id.clone(),
            username: n.username.clone(),
            author: person_label(&n.username, full_names),
            content: n.content.clone(),
            created_at: n.created_at.clone(),
        })
        .collect();
    let mut alarm = alarm_ctx(a, state);
    alarm.acknowledged_by = a
        .acknowledged_by_username
        .as_deref()
        .map(|u| person_label(u, full_names));
    context! {
        a => alarm,
        peak => peak.map(|p| context! {
            value => alarm_shown(a, p.value),
            at => p.at.and_then(unix_to_rfc3339),
            minute_average => p.minute_average,
        }),
        notes => notes,
        can_ack => can_ack,
        duration => duration,
        error => error,
    }
}

/// The detail's Length fact: the whole span of a cleared alarm, the time
/// elapsed until `now` (unix seconds) of an active one.
fn detail_duration(triggered_at: &str, cleared_at: Option<&str>, now: i64) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    match cleared_at {
        Some(cleared) => alarm_duration(triggered_at, cleared),
        None => {
            let start = time::OffsetDateTime::parse(triggered_at, &Rfc3339).ok()?;
            Some(humanize_duration_secs(
                (now - start.unix_timestamp()).max(0),
            ))
        }
    }
}

/// Every username the alarm detail shows: the acknowledger and the note
/// authors, for one [`crate::db::Db::full_names`] lookup.
pub fn alarm_detail_usernames(
    a: &portal_storage::AlarmEvent,
    notes: &[portal_storage::AlarmNote],
) -> Vec<String> {
    a.acknowledged_by_username
        .iter()
        .chain(notes.iter().map(|n| &n.username))
        .cloned()
        .collect()
}

/// Humanized triggered->cleared span ("2 h 14 min"); `None` when either
/// timestamp fails to parse (degrade to the template's "ongoing"/dash
/// rather than 500 on a malformed row).
pub(crate) fn alarm_duration(triggered_at: &str, cleared_at: &str) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    let start = time::OffsetDateTime::parse(triggered_at, &Rfc3339).ok()?;
    let end = time::OffsetDateTime::parse(cleared_at, &Rfc3339).ok()?;
    Some(humanize_duration_secs((end - start).whole_seconds().max(0)))
}

/// `"3 d 4 h"` / `"2 h 14 min"` / `"14 min"` / `"42 s"`.
pub(crate) fn humanize_duration_secs(secs: i64) -> String {
    let mins = secs / 60;
    let days = mins / 1440;
    let hours = (mins % 1440) / 60;
    let rem_mins = mins % 60;
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {rem_mins} min")
    } else if mins > 0 {
        format!("{mins} min")
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_links_follow_the_sensor_id_convention() {
        let links = |id| alarm_links("proximos-1a2b3c4d", id);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            links("line-5"),
            (
                s("/hubs/proximos-1a2b3c4d"),
                s("/hubs/proximos-1a2b3c4d/sensors/line-6")
            )
        );
        assert_eq!(
            links("lrw:70b3d50000000010:native:0"),
            (
                s("/nodes/70b3d50000000010/overview"),
                s("/monitoring/probe/node:70b3d50000000010")
            )
        );
        assert_eq!(links("p2p:aabb:liveness").0, s("/nodes/aabb/overview"));
        assert_eq!(
            links("eye:c0ffee:"),
            (
                s("/beacons/c0ffee/overview"),
                s("/monitoring/probe/beacon:c0ffee")
            )
        );
        assert_eq!(links("sys:wifi"), (s("/hubs/proximos-1a2b3c4d"), None));
    }

    /// `/alarms` wears the same toolbar as the three client-side lists but
    /// filters on the server, so its half of `ui.toolbar_shell` is the half
    /// nothing else exercises: named form controls, a rendered count, and a
    /// panel the server opens. Nothing else renders this page in a test, so a
    /// macro call gone wrong here would only show up in a browser.
    #[test]
    fn the_alarms_page_wears_the_toolbar_in_its_server_mode() {
        let templates = Templates::new("templates", "static");
        let render = |severity: Option<&str>, show: &str| {
            templates
                .env()
                .get_template("pages/alarms.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    nav_active => "alarms",
                    // One row, so the body markup is exercised too — the
                    // column contract has two ends and an empty table only
                    // shows one of them.
                    alarms => vec![alarm_ctx(&portal_storage::AlarmEvent {
                        id: "a1".into(),
                        hostname: "h1".into(),
                        sensor_id: "line-0".into(),
                        sensor_name: Some("Probe 6".into()),
                        sensor_location: Some("Zone F".into()),
                        alarm_type: "HIGH".into(),
                        severity: "WARNING".into(),
                        from_state: Some("NORMAL".into()),
                        to_state: Some("WARNING".into()),
                        message: None,
                        value: Some(30.1),
                        threshold: Some(30.0),
                        triggered_at: "2026-09-14T09:00:00Z".into(),
                        cleared_at: None,
                        acknowledged_at: None,
                        acknowledged_by_user_id: None,
                        acknowledged_by_username: None,
                        native_key: None,
                        created_at: "2026-09-14T09:00:00Z".into(),
                        max_severity: "warning".into(),
                        end_reason: None,
                    }, &PortalState::default())],
                    total => 137,
                    limit => 20,
                    offset => 0,
                    devices => vec!["hub"],
                    active_count => 6,
                    can_edit => false,
                    chip_hrefs => context! { severity => "/alarms" },
                    filter => context! {
                        hostname => None::<String>,
                        severity => severity,
                        alarm_type => None::<String>,
                        to_state => None::<String>,
                        include_acknowledged => true,
                        include_cleared => false,
                        show => show,
                    },
                })
                .unwrap_or_else(|err| panic!("alarms page failed to render: {err:#?}"))
        };

        let plain = render(None, "open");
        assert!(
            plain.contains(r#"data-list-key="alarms""#),
            "the script's root"
        );
        assert!(
            plain.contains("data-list-filters-toggle"),
            "the Filters button"
        );
        // Server mode: the fields are form controls that submit themselves.
        for name in ["severity", "alarm_type", "to_state"] {
            assert!(
                plain.contains(&format!(r#"name="{name}""#)),
                "the {name} control posts back"
            );
        }
        assert!(
            plain.contains("this.form.submit()"),
            "and does it on change, as v1's instant dropdowns did"
        );
        // …and the numbers arrive rendered, not left to a filter pass that
        // would only ever see one page of a paginated list.
        assert!(plain.contains("137"), "the total, not the page length");
        // The search is a SQL clause here, not a pass over the page: a
        // paginated list cannot be searched by hiding the rows it is not
        // showing, which is what the client-side version did until it was
        // moved.
        assert!(plain.contains(r#"name="q""#), "the search submits");
        assert!(
            !plain.contains("data-list-search"),
            "and is not handed to the client-side filter"
        );
        assert!(
            plain.contains(r#"<button type="submit" class="sr-only""#),
            "with something for Enter to hit — every visible control \
             auto-submits, so the form would otherwise have no submit at all"
        );

        // Every informational column is toggleable, and each one is addressed
        // at both ends — the picker's checkbox and the two cells. A head and a
        // body that disagree do not throw here either: the cells are hidden by
        // CSS, so the table just shears sideways.
        for col in ["loc", "type", "value", "start", "state", "end", "length"] {
            assert!(
                plain.contains(&format!(r#"data-col="{col}""#)),
                "the {col} checkbox"
            );
            assert!(
                plain.matches(&format!(r#"data-column="{col}""#)).count() >= 2,
                "the {col} head cell and its body cell"
            );
        }
        // The two that are dead in the default view start off; the rest start
        // on, or the page would open blank.
        assert_eq!(
            plain.matches(r#"data-col-default="0""#).count(),
            3,
            "Value, End and Duration, and nothing else, default to hidden"
        );

        // One filter set opens the panel, whatever this browser last chose,
        // and says so as a chip that can be taken off again.
        let filtered = render(Some("critical"), "open");
        // One filter is on: `severity`. The fixture's `include_acknowledged`
        // is the lifecycle select's DEFAULT (`show=open`) and so draws no
        // chip. Asserted on the marker rather than on the href: minijinja
        // escapes `/` in an attribute, so the raw string is `&#x2f;alarms` —
        // correct, and not something a test should be pinned to.
        assert_eq!(
            filtered.matches("data-list-chip ").count(),
            1,
            "one chip for `severity`, none for the default lifecycle view"
        );
        let panel = filtered
            .split("data-list-panel")
            .nth(1)
            .and_then(|s| s.split('>').next())
            .expect("the panel");
        assert!(
            !panel.contains("hidden"),
            "a set filter cannot sit collapsed"
        );

        // The lifecycle select shows what the URL asked for, or the next
        // change to any other filter would submit `show=open` (UX-048).
        let all = render(None, "all");
        assert!(all.contains(r#"<option value="all" selected>"#));
        assert!(!all.contains(r#"<option value="open" selected>"#));
    }

    /// An ended alarm says "Cleared" in words, and a liveness row of a Beacon
    /// the snapshot no longer knows is named by its MAC, never by the
    /// internal `eye:<mac>:liveness` key (UX-049).
    #[test]
    fn an_ended_offline_row_reads_as_ended_and_by_name() {
        let templates = Templates::new("templates", "static");
        let row = alarm_ctx(
            &portal_storage::AlarmEvent {
                id: "a1".into(),
                hostname: "h1".into(),
                sensor_id: "eye:EE:E0:00:00:00:03:liveness".into(),
                sensor_name: None,
                sensor_location: None,
                alarm_type: "OFFLINE".into(),
                severity: "WARNING".into(),
                from_state: None,
                to_state: None,
                message: None,
                value: None,
                threshold: None,
                triggered_at: "2026-09-23T05:00:00Z".into(),
                cleared_at: Some("2026-09-23T05:38:00Z".into()),
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
                created_at: "2026-09-23T05:00:00Z".into(),
                max_severity: "warning".into(),
                end_reason: None,
            },
            &PortalState::default(),
        );
        let html = templates
            .env()
            .get_template("partials/alarm_table.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                alarms => vec![row],
                total => 1, limit => 20, offset => 0,
                sort => "start", dir => "desc", sort_qs => "",
                filter => context! { include_acknowledged => true, include_cleared => true },
            })
            .expect("renders");
        assert!(html.contains(">Ended<"), "the lifecycle in words");
        assert!(html.contains("not acknowledged"), "with its ack state");
        assert!(!html.contains(">Pending<"), "an ended alarm is not pending");
        assert!(html.contains("EE:E0:00:00:00:03<"), "named by its MAC");
        assert!(!html.contains(":liveness<"), "not by the internal key");
        assert!(html.contains(">Beacon<"), "with the device kind under it");
    }

    #[test]
    fn humanize_duration_table() {
        assert_eq!(humanize_duration_secs(42), "42 s");
        assert_eq!(humanize_duration_secs(14 * 60), "14 min");
        assert_eq!(humanize_duration_secs(2 * 3600 + 14 * 60), "2 h 14 min");
        assert_eq!(humanize_duration_secs(3 * 86_400 + 4 * 3600), "3 d 4 h");
        assert_eq!(humanize_duration_secs(0), "0 s");
    }

    #[test]
    fn detail_duration_counts_an_active_alarm_up_to_now() {
        // 2026-08-18T09:00:00Z + 2 h 14 min
        let now = 1_787_043_600 + 2 * 3600 + 14 * 60;
        assert_eq!(
            detail_duration("2026-08-18T09:00:00Z", None, now).as_deref(),
            Some("2 h 14 min")
        );
        assert_eq!(
            detail_duration("2026-08-18T09:00:00Z", Some("2026-08-18T09:14:00Z"), now).as_deref(),
            Some("14 min")
        );
        assert_eq!(detail_duration("not-a-ts", None, now), None);
    }

    #[test]
    fn alarm_duration_from_rfc3339_pair() {
        assert_eq!(
            alarm_duration("2026-08-18T09:00:00Z", "2026-08-18T11:14:00Z"),
            Some("2 h 14 min".to_string())
        );
        // Malformed input degrades to None, never panics.
        assert_eq!(alarm_duration("not-a-ts", "2026-08-18T11:14:00Z"), None);
        // Clock skew (end before start) clamps to zero instead of negatives.
        assert_eq!(
            alarm_duration("2026-08-18T11:00:00Z", "2026-08-18T10:00:00Z"),
            Some("0 s".to_string())
        );
    }

    /// The worked example: a Node's humidity alarm read "63.5 °C" under the
    /// name "Temperature". It carries its own quantity, unit and Node name.
    #[test]
    fn a_node_humidity_alarm_names_its_node_and_its_unit() {
        let mut state = PortalState::default();
        let mut hub = crate::state::DeviceState::new("h1".into());
        let mut node = crate::state::NodeState::new("70b3d50000000001".into(), 0);
        node.name = Some("Sticker 1".into());
        hub.nodes.insert(node.node_key.clone(), node);
        state.devices.insert("h1".into(), hub);
        let e = portal_storage::AlarmEvent {
            id: "a2".into(),
            hostname: "h1".into(),
            sensor_id: "lrw:70b3d50000000001:Humidity".into(),
            sensor_name: Some("Humidity".into()),
            sensor_location: Some("Cold Room A".into()),
            alarm_type: "HIGH".into(),
            severity: "warning".into(),
            from_state: None,
            to_state: Some("WARNING".into()),
            message: None,
            value: Some(63.5),
            threshold: Some(60.0),
            triggered_at: "2026-08-18T09:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
            created_at: "2026-08-18T09:00:00Z".into(),
            max_severity: "warning".into(),
            end_reason: None,
        };
        let ctx = alarm_ctx(&e, &state);
        assert_eq!(ctx.field.as_deref(), Some("humidity"));
        assert_eq!(ctx.unit.as_deref(), Some("%"));
        assert_eq!(ctx.device_name.as_deref(), Some("Sticker 1"));
    }

    #[test]
    fn alarm_ctx_normalizes_severity_and_states() {
        let e = portal_storage::AlarmEvent {
            id: "a1".into(),
            hostname: "h1".into(),
            sensor_id: "line-0".into(),
            sensor_name: None,
            sensor_location: None,
            alarm_type: "HIGH".into(),
            severity: "WARNING".into(),
            from_state: Some("NORMAL".into()),
            to_state: Some("WARNING".into()),
            message: None,
            value: Some(9.0),
            threshold: Some(8.0),
            triggered_at: "2026-08-18T09:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
            created_at: "2026-08-18T09:00:00Z".into(),
            max_severity: "warning".into(),
            end_reason: None,
        };
        let ctx = alarm_ctx(&e, &PortalState::default());
        assert_eq!(ctx.severity, "warning");
        assert_eq!(ctx.from_state.as_deref(), Some("Normal"));
        assert_eq!(ctx.to_state.as_deref(), Some("Warning"));
        // Passed raw; the template formats it through `short_dt`.
        assert_eq!(ctx.triggered_at, "2026-08-18T09:00:00Z");
    }

    /// The ack line and the notes name the person the way the sidebar does,
    /// with the stored username kept beside it; a user with no name on file
    /// (or deleted) shows the username alone.
    #[test]
    fn alarm_detail_shows_full_names_beside_usernames() {
        let alarm = portal_storage::AlarmEvent {
            id: "a1".into(),
            hostname: "h1".into(),
            sensor_id: "line-0".into(),
            sensor_name: None,
            sensor_location: None,
            alarm_type: "HIGH".into(),
            severity: "WARNING".into(),
            from_state: None,
            to_state: Some("WARNING".into()),
            message: None,
            value: Some(9.0),
            threshold: Some(8.0),
            triggered_at: "2026-09-25T09:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: Some("2026-09-25T09:05:00Z".into()),
            acknowledged_by_user_id: Some(1),
            acknowledged_by_username: Some("admin".into()),
            native_key: None,
            created_at: "2026-09-25T09:00:00Z".into(),
            max_severity: "warning".into(),
            end_reason: None,
        };
        let note = |id: &str, username: &str| portal_storage::AlarmNote {
            id: id.into(),
            alarm_id: "a1".into(),
            user_id: None,
            username: username.into(),
            content: format!("note by {username}"),
            created_at: "2026-09-25T09:10:00Z".into(),
        };
        let notes = [note("n1", "admin"), note("n2", "gone")];
        assert_eq!(
            alarm_detail_usernames(&alarm, &notes),
            ["admin", "admin", "gone"]
        );
        let names = HashMap::from([("admin".to_string(), "Jan Novák".to_string())]);

        let html = Templates::new("templates", "static")
            .env()
            .get_template("partials/alarm_detail_body.html")
            .expect("template loads")
            .render(context! {
                lang => "cs",
                current_user => None::<CurrentUser>,
                ..alarm_detail_body_ctx(&PortalState::default(), &alarm, &notes, &names, None, None)
            })
            .unwrap_or_else(|err| panic!("alarm detail failed to render: {err:#?}"));

        assert!(
            html.contains("uživatel Jan Novák (admin)"),
            "the ack line names the person"
        );
        assert!(html.contains("Jan Novák (admin) · "), "the admin's note");
        assert!(html.contains(">gone · "), "a user without a name: {html}");
    }

    // ---- §19 (F4): the fault indicator finally reaches a template --------
}
