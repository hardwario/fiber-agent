use super::*;

/// Everything the monitoring page needs that the in-memory store does not
/// hold: the database side (groups, the rendered sections) and this
/// operator's own choices. A struct rather than five more parameters —
/// `dashboard_ctx` is the one context builder that reads from both halves of
/// the portal, and a positional call with six trailing arguments is a
/// mis-ordering waiting to happen.
pub struct MonitoringInputs<'a> {
    /// M4-D: gates the wired rows' threshold pencil and the group dialogs.
    pub can_edit: bool,
    /// One of `crate::monitoring::VIEW_MODES`.
    pub view_mode: &'a str,
    /// Already translated, from a mutation's redirect.
    pub error: Option<String>,
    /// Built by `monitoring::sections`, passed in rather than built here
    /// because the detailed card layout then fills each card's trend line
    /// from stored history, which is async.
    pub sections: Vec<crate::monitoring::GroupSectionCtx>,
}

impl Default for MonitoringInputs<'_> {
    fn default() -> Self {
        Self {
            can_edit: false,
            view_mode: crate::monitoring::DEFAULT_VIEW_MODE,
            error: None,
            sections: Vec::new(),
        }
    }
}

/// Full `pages/dashboard.html` context — the monitoring page.
///
/// The page spans every Hub: `sections` are built from monitoring groups,
/// which are cross-Hub by design (monitoring.backend.md), and `picker`
/// walks every device in the store. `device`/`hostname` below are the first
/// entry in the `BTreeMap` and are used for one thing only — the "waiting
/// for a device" empty state, which asks whether the store holds any Hub at
/// all (devices.hub.md §12 E; the device picker itself is #52, phase 2).
///
/// The page is v1's `/monitoring`: one section per group, and nothing an
/// operator has not grouped.
/// Everything below `sections` is built in [`crate::monitoring`], which owns
/// the card/row model; this function is the assembly point.
pub fn dashboard_ctx(state: &PortalState, input: MonitoringInputs<'_>) -> Value {
    let device = state.devices.values().next();
    context! {
        nav_active => "dashboard",
        mqtt => mqtt_ctx(&state.mqtt),
        device => device.map(|d| device_status_ctx(d, input.can_edit)),
        hostname => device.map(|d| d.hostname.clone()),
        can_edit => input.can_edit,
        view_mode => input.view_mode,
        view_modes => crate::monitoring::VIEW_MODES,
        // The two axes the layout partials actually branch on. Decided here
        // rather than by the template picking the mode string apart.
        view_cards => input.view_mode.starts_with("card"),
        view_simple => input.view_mode.ends_with("simplified"),
        sections => input.sections,
        error => input.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::{beacon, node};

    #[test]
    fn dashboard_ctx_none_device_when_store_empty() {
        let state = PortalState::default();
        let ctx = dashboard_ctx(&state, MonitoringInputs::default());
        assert!(ctx.get_attr("device").unwrap().is_none());
        assert_eq!(ctx.get_attr("nav_active").unwrap().to_string(), "dashboard");
        assert!(!ctx
            .get_attr("mqtt")
            .unwrap()
            .get_attr("connected")
            .unwrap()
            .is_true());
    }

    #[test]
    fn dashboard_ctx_picks_first_device_in_btreemap_order() {
        let mut state = PortalState::default();
        state
            .devices
            .insert("zeta".into(), DeviceState::new("zeta".into()));
        state
            .devices
            .insert("alpha".into(), DeviceState::new("alpha".into()));
        state.mqtt.connected = true;

        let ctx = dashboard_ctx(&state, MonitoringInputs::default());
        let device = ctx.get_attr("device").unwrap();
        assert!(!device.is_none());
        assert_eq!(
            device.get_attr("hostname").unwrap().to_string(),
            "alpha" // BTreeMap order: "alpha" < "zeta"
        );

        let sensors = device.get_attr("sensors").unwrap();
        assert_eq!(sensors.len().unwrap(), 8);
        let first_sensor = sensors.get_item(&Value::from(0)).unwrap();
        assert_eq!(
            first_sensor
                .get_attr("dominant_alarm_state")
                .unwrap()
                .to_string(),
            "NeverConnected"
        );
    }

    // ---- M5-G: Node/Beacon context builders ----

    /// The dashboard's one table must actually render all three row
    /// partials. A template error here is invisible in production — the
    /// handler logs a warning and serves a placeholder — so it needs a test
    /// that renders, not one that only builds the context.
    #[test]
    fn dashboard_renders_probe_node_and_registered_beacon_rows() {
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        device.sensors[0].connected = true;
        device.sensors[0].name = Some("Zone A".into());
        let mut n = node("aabbccdd00000001");
        n.name = Some("Cold room Node".into());
        n.alarm_state = "NORMAL".into();
        device.nodes.insert(n.node_key.clone(), n);
        let mut registered = beacon("AA:BB:CC:DD:EE:01");
        registered.alarm_state = "NORMAL".into();
        let mut candidate = beacon("AA:BB:CC:DD:EE:02");
        candidate.discovered = Some(true);

        device
            .ble_sensors
            .insert(registered.mac.clone(), registered);
        device.ble_sensors.insert(candidate.mac.clone(), candidate);

        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let templates = Templates::new("templates", "static");
        let ctx = page_ctx(
            dashboard_ctx(
                &state,
                MonitoringInputs {
                    can_edit: true,
                    sections: crate::monitoring::sections(
                        &state,
                        &[],
                        &[],
                        &Default::default(),
                        &Default::default(),
                    ),
                    ..Default::default()
                },
            ),
            None,
        );
        let html = templates
            .env()
            .get_template("pages/dashboard.html")
            .expect("template loads")
            .render(context! { lang => "en", ..ctx })
            .expect("dashboard renders");

        // Nothing is grouped, so the page is its empty state — but the
        // picker still offers every entity to the New Group dialog.
        assert!(html.contains("No groups created yet."), "the empty state");
        assert!(
            html.contains("New Group"),
            "the create button, for a manager"
        );
        assert!(
            html.contains(r#"data-list-filter="kind""#),
            "the type filter"
        );
        // The picker is on /monitoring/groups/new now; the form partial is
        // rendered from the same context the page route hands it.
        let html = templates
            .env()
            .get_template("partials/group_form.html")
            .expect("template loads")
            .render(
                context! { lang => "en", form_id => "g", action => "/monitoring/groups", g => (),
                rows => crate::sensor_picker::group_rows(
                    &state,
                    &crate::monitoring::picker(&state, &[], &[], &Default::default(), None),
                    &[],
                    &[],
                ) },
            )
            .expect("form renders");
        assert!(html.contains("Zone A"), "the probe, offered in the picker");
        assert!(html.contains("Cold room Node"), "the Node, in the picker");
        assert!(
            html.contains("AA:BB:CC:DD:EE:01"),
            "the registered Beacon, in the picker"
        );
        assert!(
            !html.contains("AA:BB:CC:DD:EE:02"),
            "a discovered candidate is not groupable — it belongs on /beacons"
        );
    }

    /// Nothing grouped means nothing shown, as in v1 — but the picker still
    /// offers every entity, or the first group could never be made.
    #[test]
    fn nothing_is_shown_until_something_is_grouped() {
        let mut state = PortalState::default();
        let mut device = DeviceState::new("hub".into());
        let n = node("aabbccdd00000001");
        device.nodes.insert(n.node_key.clone(), n);
        state.devices.insert("hub".into(), device);

        let ctx = dashboard_ctx(
            &state,
            MonitoringInputs {
                can_edit: true,
                sections: crate::monitoring::sections(
                    &state,
                    &[],
                    &[],
                    &Default::default(),
                    &Default::default(),
                ),
                ..Default::default()
            },
        );
        assert_eq!(ctx.get_attr("hostname").unwrap().to_string(), "hub");
        assert!(ctx.get_attr("can_edit").unwrap().is_true());

        assert_eq!(ctx.get_attr("sections").unwrap().len().unwrap(), 0);

        let picker = Value::from_serialize(crate::monitoring::picker(
            &state,
            &[],
            &[],
            &Default::default(),
            None,
        ));
        let hub = picker.get_item(&Value::from(0)).unwrap();
        assert_eq!(hub.get_attr("probes").unwrap().len().unwrap(), 8);
        assert_eq!(hub.get_attr("nodes").unwrap().len().unwrap(), 1);
    }

    /// Four layouts means four template paths through two partials, and a
    /// mode nobody renders in a test is a mode that breaks in the browser.
    #[test]
    fn every_view_mode_renders_a_grouped_page() {
        use portal_storage::{SensorGroup, SensorGroupMember};

        let mut device = DeviceState::new("hub".into());
        device.online = true;
        device.sensors[0].connected = true;
        device.sensors[0].name = Some("Zone A".into());
        device.sensors[0].temperature_avg_celsius = Some(4.2);
        // Thresholds so the detailed card actually renders its bounds block
        // and the gauge, rather than skipping both branches.
        device.sensors[0].thresholds = Some(crate::state::SensorThresholds {
            critical_low: Some(-40.0),
            warning_low: Some(0.0),
            warning_high: Some(30.0),
            critical_high: Some(50.0),
            alarm_low: None,
            alarm_high: None,
            hysteresis: None,
            enabled: None,
        });
        let mut n = node("aabbccdd00000001");
        n.name = Some("Cold room Node".into());
        n.alarm_state = "NORMAL".into();
        n.fields.insert("temperature".into(), 4.5);
        device.nodes.insert(n.node_key.clone(), n);

        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let groups = vec![SensorGroup {
            id: 1,
            name: "Cold Storage".into(),
            description: Some("the freezers".into()),
            location: None,
            sort_order: 0,
            created_at: "2026-08-27".into(),
        }];
        let members = vec![SensorGroupMember {
            id: 1,
            group_id: 1,
            hostname: "hub".into(),
            sensor_line: Some(0),
            node_key: None,
            mac: None,
            fields: None,
            sort_order: 0,
        }];

        let templates = Templates::new("templates", "static");
        let mut columns: BTreeMap<&str, usize> = BTreeMap::new();
        for mode in crate::monitoring::VIEW_MODES {
            let ctx = page_ctx(
                dashboard_ctx(
                    &state,
                    MonitoringInputs {
                        can_edit: true,
                        view_mode: mode,
                        sections: crate::monitoring::sections(
                            &state,
                            &groups,
                            &members,
                            &Default::default(),
                            &Default::default(),
                        ),
                        ..Default::default()
                    },
                ),
                None,
            );
            let html = templates
                .env()
                .get_template("pages/dashboard.html")
                .expect("template loads")
                .render(context! { lang => "en", ..ctx })
                // `{err:#?}` rather than Display: minijinja's Display stops at
                // the outer BadInclude and the actual cause is in `source`.
                .unwrap_or_else(|err| panic!("mode {mode} failed to render: {err:#?}"));

            assert!(html.contains("Cold Storage"), "{mode}: the group heading");
            assert!(html.contains("the freezers"), "{mode}: its description");
            assert!(html.contains("Zone A"), "{mode}: the grouped probe");
            assert!(
                !html.contains("No groups created yet."),
                "{mode}: a group exists, so not the empty state"
            );
            if mode == "card-detailed" {
                // Variant B: the limits bar draws its zones, no numbers.
                assert!(html.contains("bg-critical"), "{mode}: the limits bar");
                assert!(!html.contains("-40.0"), "{mode}: no bound numbers");
            }
            assert!(
                html.contains("/monitoring/groups/1/edit"),
                "{mode}: the group's edit page link"
            );

            // The shared toolbar's contract, in all four layouts — two of
            // them are cards, so there is no table for the script to hold on
            // to and the section is the only handle it has.
            assert!(
                html.contains(r#"data-list-key="monitoring""#),
                "{mode}: the script's root"
            );
            assert!(
                html.contains(r#"data-section-name="cold storage""#),
                "{mode}: the group name, which §2.2 puts in the haystack"
            );
            assert!(html.contains("data-oor="), "{mode}: the row's value class");
            // A group the filter empties keeps its header and swaps its body
            // for a hidden-by-default message (amendment 2026-10-15), in
            // every layout, so the script never hides the section itself.
            assert!(
                html.contains("data-filter-content"),
                "{mode}: the body the filter can hide"
            );
            assert!(
                html.contains(r#"data-filter-empty class="hidden"#),
                "{mode}: the filter-empty message starts hidden"
            );
            assert!(
                html.contains("No channels match the filter."),
                "{mode}: the filter-empty message text"
            );
            // Monitoring's field set: Druh yes, and the three that belong to
            // the device lists no.
            assert!(
                html.contains(r#"data-list-filter="kind""#),
                "{mode}: the Druh field"
            );
            for absent in ["location", "proto"] {
                assert!(
                    !html.contains(&format!(r#"data-list-filter="{absent}""#)),
                    "{mode}: {absent} belongs to the device lists, not here"
                );
            }
            assert!(
                !html.contains("data-list-gwlist"),
                "{mode}: no gateway checklist on Monitoring"
            );
            // The tiles deep-link `?status=alarming`; both halves of that have
            // to survive or the tile points at a filter nothing offers.
            assert!(
                html.contains(r#"data-list-oor-param="status""#),
                "{mode}: the value filter keeps the tiles' param name"
            );
            assert!(
                html.contains(r#"value="alarming""#),
                "{mode}: and the option spanning two classes"
            );
            assert!(
                html.contains("/monitoring/view-mode"),
                "{mode}: the layout picker rides in the toolbar row"
            );
            // …and so does the primary action, for a manager. It used to sit
            // on a line of its own above the bar, where it read as a button
            // belonging to nothing. Asserted on the handle, not the onclick:
            // minijinja escapes `'` in an attribute, so the literal is
            // `group-new&#39;`.
            assert!(
                html.contains("data-list-action"),
                "{mode}: the primary action is in the toolbar"
            );

            columns.insert(mode, html.matches("<th ").count());
        }

        // The layouts have to actually DIFFER. Asserting only what they share
        // is how both table modes shipped rendering the detailed column set:
        // the partial read a `simple` local that never reached it through the
        // enclosing `call` block, and an undefined name is falsy.
        assert_eq!(
            columns["table-detailed"], 7,
            "stripe, Sensor, Channel, Value, Limits, Min/max, Data"
        );
        assert_eq!(columns["card-simplified"], 0, "cards render no table");
        assert_eq!(columns["card-detailed"], 0);
    }

    // ---- MB3: the probe-detail window, and what it replaced ----------

    /// The interim `r.href` is gone (monitoring.md §5, decision 5): a click
    /// on a probe opens the window, it does not navigate to `/nodes/{key}`.
    ///
    /// Asserted **per layout**, because the four shapes are four separate
    /// pieces of markup in one partial and the interim link lived in three
    /// of them. The field was removed from `GroupRowCtx` rather than merely
    /// left unset, so a layout cannot quietly grow the link back — but a
    /// hand-written `<a href="/nodes/…">` still could, which is what the
    /// negative half of this test is for.
    #[test]
    fn every_row_layout_links_to_trends_and_none_links_to_nodes() {
        use portal_storage::{SensorGroup, SensorGroupMember};

        let mut device = DeviceState::new("hub".into());
        device.online = true;
        device.sensors[0].connected = true;
        device.sensors[0].name = Some("Zone A".into());
        device.sensors[0].temperature_avg_celsius = Some(4.2);
        let mut n = node("aabbccdd00000001");
        n.name = Some("Cold room Node".into());
        n.alarm_state = "NORMAL".into();
        n.fields.insert("temperature".into(), 4.5);
        device.nodes.insert(n.node_key.clone(), n);
        let mut b = beacon("AA:BB:CC:DD:EE:01");
        b.alarm_state = "NORMAL".into();
        b.name = Some("Freezer".into());
        b.temperature = Some(-18.0);
        // A fault, so every layout has to say it in words (UX-024).
        b.low_battery = Some(true);
        device.ble_sensors.insert(b.mac.clone(), b);

        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let groups = vec![SensorGroup {
            id: 1,
            name: "Cold Storage".into(),
            description: Some("the freezers".into()),
            location: None,
            sort_order: 0,
            created_at: "2026-09-09".into(),
        }];
        let members = vec![
            SensorGroupMember {
                id: 1,
                group_id: 1,
                hostname: "hub".into(),
                sensor_line: Some(0),
                node_key: None,
                mac: None,
                fields: None,
                sort_order: 0,
            },
            SensorGroupMember {
                id: 2,
                group_id: 1,
                hostname: "hub".into(),
                sensor_line: None,
                node_key: Some("aabbccdd00000001".into()),
                mac: None,
                fields: None,
                sort_order: 1,
            },
            SensorGroupMember {
                id: 3,
                group_id: 1,
                hostname: "hub".into(),
                sensor_line: None,
                node_key: None,
                mac: Some("AA:BB:CC:DD:EE:01".into()),
                fields: None,
                sort_order: 2,
            },
        ];

        let templates = Templates::new("templates", "static");
        for mode in crate::monitoring::VIEW_MODES {
            let ctx = page_ctx(
                dashboard_ctx(
                    &state,
                    MonitoringInputs {
                        can_edit: true,
                        view_mode: mode,
                        sections: crate::monitoring::sections(
                            &state,
                            &groups,
                            &members,
                            &Default::default(),
                            &Default::default(),
                        ),
                        ..Default::default()
                    },
                ),
                None,
            );
            let html = templates
                .env()
                .get_template("pages/dashboard.html")
                .expect("template loads")
                .render(context! { lang => "en", ..ctx })
                .unwrap_or_else(|err| panic!("mode {mode} failed to render: {err:#?}"));

            // Every layout opens Trends with the sensor's channels for 24 h
            // (monitoring.md, amendments 2026-10-15).
            let hrefs = [
                "/trends?s=line:hub:0&range=24h",
                "/trends?s=node:aabbccdd00000001:temperature&range=24h",
                "/trends?s=eye:AA:BB:CC:DD:EE:01:temperature&range=24h",
            ];
            for href in hrefs {
                let token = href;
                assert!(
                    html.contains(&format!(r#"href="{href}""#)),
                    "{mode}: {token} does not link to its own page"
                );
            }
            // One row-level link per row, so the whole row is clickable the
            // way /nodes and /beacons rows are (layout/base.html).
            assert_eq!(
                html.matches(r#"data-row-href="/"#).count(),
                3,
                "{mode}: one row href per row, no more and no fewer"
            );
            assert!(
                !html.contains(r#"href="/nodes/aabbccdd00000001""#),
                "{mode}: the interim href is retired, not merely unset"
            );
            // The modal it used to open is gone, dialog and opener alike.
            assert!(!html.contains(r#"id="probe-detail""#), "{mode}");
            assert!(!html.contains("probe-open"), "{mode}");
            // The fault is visible text, not only the icon's `title`.
            assert!(html.contains(">Low battery</"), "{mode}");
        }
    }

    /// §3.2's markup contract, on the header partial that carries it.
    ///
    /// Collapse is client-side and per user, so the server has nothing to
    /// render it from: this asserts the **initial expanded state**, which is
    /// what every render — page and SSE fragment alike — must produce. The
    /// script does the rest, and a fragment that arrived guessing a
    /// collapsed state would be wrong for every other viewer.
    #[test]
    fn the_group_header_renders_an_expanded_collapse_control() {
        use portal_storage::{SensorGroup, SensorGroupMember};

        let mut state = PortalState::default();
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        device.sensors[0].connected = true;
        device.sensors[0].name = Some("Zone A".into());
        state.devices.insert("hub".into(), device);

        let groups = vec![SensorGroup {
            id: 7,
            name: "Cold Storage".into(),
            description: Some("the freezers".into()),
            location: None,
            sort_order: 0,
            created_at: "2026-09-09".into(),
        }];
        let members = vec![SensorGroupMember {
            id: 1,
            group_id: 7,
            hostname: "hub".into(),
            sensor_line: Some(0),
            node_key: None,
            mac: None,
            fields: None,
            sort_order: 0,
        }];
        let sections = crate::monitoring::sections(
            &state,
            &groups,
            &members,
            &Default::default(),
            &Default::default(),
        );

        let templates = Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("partials/group_header.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                s => &sections[0],
                can_edit => true,
                is_first => true,
                is_last => true,
            })
            .expect("header renders");

        assert!(
            html.contains(r#"class="group-collapse"#),
            "the name and the chevron are one button: {html}"
        );
        assert!(html.contains(r#"data-group="7""#), "{html}");
        assert!(
            html.contains(r#"aria-expanded="true""#),
            "every render is the expanded state: {html}"
        );
        assert!(
            html.contains(r#"aria-controls="group-body-7""#),
            "the button names what it toggles: {html}"
        );
        // Both chevrons ship; the script picks one, so it needs no copy of
        // the icon set.
        assert!(html.contains("chevron-expanded"), "{html}");
        assert!(html.contains("chevron-collapsed"), "{html}");
        // The description hides with the content, so the script needs a
        // handle on it.
        assert!(html.contains("group-description"), "{html}");
        // Nothing is hidden by the server.
        assert!(
            !html.contains(r#"aria-expanded="false""#),
            "a fragment must not guess a viewer's collapse state: {html}"
        );

        // And a collapsed group keeps updating: the header is still its own
        // swap target, which is the whole reason §3.1 puts the status here.
        assert!(
            html.contains("hx-get=\"/monitoring/groups/7/header\""),
            "{html}"
        );
    }

    /// #30c: a group holding a Node of an offline Hub says so in its header,
    /// with the time in the display zone, and the row reads "unknown" with
    /// no "All normal" verdict in every layout.
    #[test]
    fn an_offline_hubs_group_shows_a_banner_and_unknown_rows() {
        use portal_storage::{SensorGroup, SensorGroupMember};

        let mut state = PortalState::default();
        let mut device = DeviceState::new("hub".into());
        device.online = false;
        device.last_seen = Some(1_790_000_000);
        device.system.device_label = Some("Sklad".into());
        let mut n = crate::state::NodeState::new("n1".into(), 0);
        n.alarm_state = "NORMAL".into();
        n.fields.insert("temperature".into(), 4.0);
        device.nodes.insert("n1".into(), n);
        state.devices.insert("hub".into(), device);

        let groups = vec![SensorGroup {
            id: 7,
            name: "Cold Storage".into(),
            description: None,
            location: None,
            sort_order: 0,
            created_at: "2026-09-09".into(),
        }];
        let members = vec![SensorGroupMember {
            id: 1,
            group_id: 7,
            hostname: "hub".into(),
            sensor_line: None,
            node_key: Some("n1".into()),
            mac: None,
            fields: None,
            sort_order: 0,
        }];
        let sections = crate::monitoring::sections(
            &state,
            &groups,
            &members,
            &Default::default(),
            &Default::default(),
        );

        let templates = Templates::new("templates", "static");
        let env = templates.env();
        let header = env
            .get_template("partials/group_header.html")
            .unwrap()
            .render(context! {
                lang => "en", s => &sections[0], can_edit => false, is_first => true, is_last => true,
            })
            .unwrap();
        let since = display_dt(
            time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
            "en",
            "default",
            &crate::display_tz::current(),
        );
        assert!(
            header.contains(&format!("Hub Sklad is offline since {since}")),
            "{header}"
        );

        for (cards, simple) in [(true, true), (true, false), (false, false)] {
            let html = env
                .get_template("partials/group_row.html")
                .unwrap()
                .render(context! {
                    lang => "en", r => &sections[0].rows[0], view_cards => cards, view_simple => simple,
                })
                .unwrap();
            assert!(
                html.contains(">unknown<"),
                "cards={cards} simple={simple}: {html}"
            );
            assert!(!html.contains("All normal"), "{html}");
            assert!(html.contains(r#"data-connected="0""#), "{html}");
        }
    }

    /// The probe page renders for all three probe kinds, driven through the
    /// real template — a broken `{% if %}` in one branch is invisible in
    /// production (the handler logs and serves a placeholder).
    ///
    /// The context is built by hand rather than through the route, because
    /// `ProbeWindowCtx` is private to `routes::probe_routes` and this test
    /// is about the template. The route's own tests
    /// (`probe_routes::tests::integration`) render it end to end.
    #[test]
    fn the_probe_page_renders_for_all_three_kinds() {
        let templates = Templates::new("templates", "static");

        for (kind, device_href, history_href, operational) in [
            ("probe", None, Some("/trends?s=line:1"), false),
            (
                "node",
                Some("/nodes/aa11"),
                Some("/trends?s=node:aa11:temperature"),
                true,
            ),
            ("beacon", None, None, true),
        ] {
            let page = page_ctx(
                context! {
                    nav_active => "dashboard",
                    error => None::<String>,
                },
                None,
            );
            let html = templates
                .env()
                .get_template("pages/probe_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    ..context! {
                    w => context! {
                        token => "node:aa11",
                        kind => kind,
                        hostname => "hub",
                        device_label => "Hub 1",
                        name => "Cold box 3",
                        description => "Warehouse B",
                        range => "24h",
                        ranges => ["24h", "3d", "7d"],
                        from_iso => "2026-09-08T10:00:00Z",
                        to_iso => "2026-09-09T10:00:00Z",
                        png_filename => "probe_cold-box-3_24h_20260909.png",
                        csv_href => "/export/probe/node:aa11.csv?from=a&to=b",
                        device_href => device_href,
                        history_href => history_href,
                        operational_supported => operational,
                        can_edit => true,
                        now_local => "2026-09-09T10:00",
                        fields => vec![context! {
                            field => "temperature",
                            label => "Temperature",
                            unit => "°C",
                            color => "#10b981",
                            count => 1234,
                            min => "18.2",
                            max => "27.9",
                            avg => "22.4",
                            median => "22.1",
                        }],
                        events => vec![
                            context! { ts => 1, at => "2026-09-09T03:12:00Z", kind => "offline",
                                       severity => None::<String>, detail => Some("Node aa11 has not reported for 60 minutes"),
                                       author => None::<String>, id => None::<i64> },
                            context! { ts => 2, at => "2026-09-09T03:58:00Z", kind => "online",
                                       severity => None::<String>, detail => None::<String>,
                                       author => None::<String>, id => None::<i64> },
                            context! { ts => 3, at => "2026-09-09T09:30:00Z", kind => "manual",
                                       severity => None::<String>, detail => Some("battery replaced"),
                                       author => Some("novak"), id => Some(4_i64) },
                        ],
                        chart_config => "{\"series\":[]}",
                        empty => false,
                    },
                    ..page
                    },
                })
                .unwrap_or_else(|err| panic!("{kind} failed to render: {err:#?}"));

            assert!(html.contains("Cold box 3"), "{kind}");
            assert!(html.contains("1234 samples"), "{kind}: the aggregates");
            assert!(html.contains("med 22.1"), "{kind}: the median");
            assert!(html.contains("probe-chart-config"), "{kind}: the island");
            assert!(
                html.contains("sensor offline"),
                "{kind}: the liveness event"
            );
            assert!(
                html.contains("battery replaced"),
                "{kind}: the manual event"
            );
            assert!(html.contains("novak"), "{kind}: its author");
            assert!(html.contains("Events in chart"), "{kind}: the §6.3 toggle");
            assert!(html.contains("probe_cold-box-3_24h_20260909.png"), "{kind}");

            assert_eq!(
                html.contains("Device detail"),
                device_href.is_some(),
                "{kind}: the device link is a Node's alone"
            );
            assert_eq!(
                html.contains("Open in Trends"),
                history_href.is_some(),
                "{kind}: /trends has no Beacon selection to link to"
            );
            assert_eq!(
                html.contains("Liveness of a wired probe is the Hub"),
                !operational,
                "{kind}: only a wired probe says its liveness is the Hub's"
            );
        }
    }

    /// MB1's *Accept:* — the header counts **fields**, not probes, and the
    /// out-of-range badge is hidden at zero. Three Nodes à T+H is "6", the
    /// wording monitoring.md §3.1 uses ("6 čidel").
    #[test]
    fn a_group_header_counts_fields_and_hides_the_badge_at_zero() {
        use portal_storage::{SensorGroup, SensorGroupMember};

        let mut device = DeviceState::new("hub".into());
        device.online = true;
        for key in ["n1", "n2", "n3"] {
            let mut n = crate::state::NodeState::new(key.into(), 0);
            n.alarm_state = "NORMAL".into();
            for field in ["temperature", "humidity"] {
                n.fields.insert(field.into(), 4.0);
                n.field_alarm_states.insert(field.into(), "NORMAL".into());
            }
            // A bound to be within: an unbounded Normal group is no verdict
            // and keeps a neutral accent (UX-055).
            n.field_thresholds = Some(vec![crate::state::NodeFieldThreshold {
                field: "temperature".into(),
                critical_low: None,
                warning_low: None,
                warning_high: Some(8.0),
                critical_high: None,
            }]);
            device.nodes.insert(key.into(), n);
        }
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let groups = vec![SensorGroup {
            id: 1,
            name: "Chladicí boxy".into(),
            description: None,
            location: None,
            sort_order: 0,
            created_at: "2026-09-03".into(),
        }];
        let members: Vec<SensorGroupMember> = ["n1", "n2", "n3"]
            .iter()
            .map(|k| SensorGroupMember {
                id: 0,
                group_id: 1,
                hostname: "hub".into(),
                sensor_line: None,
                node_key: Some((*k).into()),
                mac: None,
                fields: None,
                sort_order: 0,
            })
            .collect();

        let templates = Templates::new("templates", "static");
        let render = |state: &PortalState| {
            let sections = crate::monitoring::sections(
                state,
                &groups,
                &members,
                &Default::default(),
                &Default::default(),
            );
            let ctx = page_ctx(
                dashboard_ctx(
                    state,
                    MonitoringInputs {
                        can_edit: true,
                        sections,
                        ..Default::default()
                    },
                ),
                None,
            );
            templates
                .env()
                .get_template("pages/dashboard.html")
                .expect("template loads")
                .render(context! { lang => "en", ..ctx })
                .unwrap_or_else(|err| panic!("dashboard failed to render: {err:#?}"))
        };

        // Everything from the section marker to the first row: the header
        // and nothing else, so a row's own accent strip cannot satisfy an
        // assertion meant for the header's.
        let header_of = |html: &str| -> String {
            html.split("data-section=")
                .nth(1)
                .and_then(|s| s.split("data-kind=").next())
                .expect("a section header")
                .to_string()
        };
        // The accent strip is the header's, matched with its own classes.
        let accent = "self-stretch rounded-full shrink-0";

        let html = render(&state);
        let header = header_of(&html);
        assert!(
            header.contains(">6 channels<"),
            "six cards, not three probes: {header}"
        );
        assert!(
            !header.contains("Out of range"),
            "the badge is hidden at zero: {header}"
        );
        assert!(
            header.contains(&format!("{accent} bg-success")),
            "a Normal group gets the success accent: {header}"
        );

        // One Warning field turns the accent and shows the count.
        state
            .devices
            .get_mut("hub")
            .unwrap()
            .nodes
            .get_mut("n1")
            .unwrap()
            .field_alarm_states
            .insert("temperature".into(), "WARNING".into());
        let warned = render(&state);
        let warned_header = header_of(&warned);
        assert!(warned_header.contains("Out of range: 1"), "{warned_header}");
        assert!(
            warned_header.contains(&format!("{accent} bg-warning")),
            "{warned_header}"
        );
        assert!(
            warned_header.contains(">6 channels<"),
            "the count is a group total, not a filtered one"
        );
    }

    /// MB0's *Accept:* — "the first group has no `up` and the last no
    /// `down`". They are rendered disabled rather than dropped so the header's
    /// control row keeps its width; the route refuses the move either way.
    #[test]
    fn the_reorder_arrows_stop_at_the_ends_and_are_manager_only() {
        use portal_storage::SensorGroup;

        let mut device = DeviceState::new("hub".into());
        device.online = true;
        device.sensors[0].connected = true;
        device.sensors[0].name = Some("Zone A".into());
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let groups: Vec<SensorGroup> = [(1, "Alpha"), (2, "Beta"), (3, "Gamma")]
            .into_iter()
            .map(|(id, name)| SensorGroup {
                id,
                name: name.into(),
                description: None,
                location: None,
                sort_order: 0,
                created_at: "2026-09-03".into(),
            })
            .collect();

        let templates = Templates::new("templates", "static");
        let render = |can_edit: bool| {
            let ctx = page_ctx(
                dashboard_ctx(
                    &state,
                    MonitoringInputs {
                        can_edit,
                        sections: crate::monitoring::sections(
                            &state,
                            &groups,
                            &[],
                            &Default::default(),
                            &Default::default(),
                        ),
                        ..Default::default()
                    },
                ),
                None,
            );
            templates
                .env()
                .get_template("pages/dashboard.html")
                .expect("template loads")
                .render(context! { lang => "en", ..ctx })
                .unwrap_or_else(|err| panic!("dashboard failed to render: {err:#?}"))
        };

        let html = render(true);
        // One form per direction per group, and the section markers let each
        // group's pair be read on its own.
        let sections: Vec<&str> = html.split("data-section=").skip(1).collect();
        assert_eq!(sections.len(), 3, "one section per group");

        let arrow_disabled = |section: &str, direction: &str| {
            let form = section
                .split(&format!("value=\"{direction}\""))
                .nth(1)
                .unwrap_or_else(|| panic!("no {direction} arrow in {section}"));
            // The bare `disabled` attribute, not the `disabled:opacity-30`
            // class that styles it — both live on the same button.
            form.split("</form>")
                .next()
                .expect("form closes")
                .split("disabled")
                .skip(1)
                .any(|rest| !rest.starts_with(':'))
        };
        assert!(arrow_disabled(sections[0], "up"), "first group: no up");
        assert!(!arrow_disabled(sections[0], "down"));
        assert!(
            !arrow_disabled(sections[1], "up"),
            "the middle moves both ways"
        );
        assert!(!arrow_disabled(sections[1], "down"));
        assert!(arrow_disabled(sections[2], "down"), "last group: no down");
        assert!(!arrow_disabled(sections[2], "up"));

        assert!(
            !render(false).contains("/move"),
            "a viewer is offered no reorder control at all"
        );
    }
}
