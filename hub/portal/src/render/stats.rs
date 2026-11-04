use super::*;

/// `partials/page_stats.html`'s `stats` context: the connection pill the top
/// bar reads on every authenticated page, plus the metric tiles for THIS
/// page — see [`StatsScope`]. The tiles are built here rather than chosen in
/// the template because each page counts a different population, and the
/// tone and link follow from the count.
#[derive(Debug, Clone, Serialize)]
pub struct PageStatsCtx {
    pub connected: bool,
    pub tiles: Vec<StatTile>,
    /// The fragment URL the row refetches itself from on the SSE
    /// `page-stats` nudge (monitoring.md §2.1 "Živě přes SSE"). `None` for a
    /// row built from the portal's own tables (`/alarms`, `/audit`,
    /// `/settings/users`): no store change moves those numbers.
    pub refresh: Option<&'static str>,
}

/// One tile. `label` is an i18n key — the template applies `|t`, so a new
/// scope adds keys the static-key sweep cannot see; `page_stats_keys_exist`
/// below is what checks them.
#[derive(Debug, Clone, Serialize)]
pub struct StatTile {
    pub label: &'static str,
    pub value: i64,
    pub icon: &'static str,
    pub tone: &'static str,
    pub href: Option<&'static str>,
    /// i18n key of the tile's tooltip, for a count whose population the label
    /// alone leaves open.
    pub title: Option<&'static str>,
}

/// What the metric row above a page counts.
///
/// Chosen from `nav_active` in `routes::render`, so a page declares it once,
/// where it already declares its nav entry. Until 2026-09-14 every page got
/// the fleet row: on `/nodes` that meant "Total probes" counting Beacons and
/// wired lines too, a number that disagreed with the list right below it.
///
/// A detail page shares its list's nav entry and so its scope: the Node,
/// Beacon and Hub detail pages all carry the row of the list they came from,
/// which is the fleet context an operator reads one device against. The
/// Monitoring entry ("dashboard") is the exception: its page and the probe
/// detail page both build [`monitoring_stats_ctx`] in their handlers, so the
/// `Fleet` fallback never labels fleet numbers as the Monitoring strip.
///
/// `None` is for pages this row says nothing about: History is a chart over a
/// range, and the settings area and the audit log count the portal, not the
/// fleet. Three of those pages do carry a row, built by their own handler
/// from their own numbers — `/alarms`, `/audit` and `/settings/users`, see
/// `alarm_stats_ctx`, `audit_stats_ctx` and `user_stats_ctx`. The connection
/// pill renders for all of them either way — that is `connected`, not a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsScope {
    Fleet,
    Nodes,
    Beacons,
    Hubs,
    None,
}

impl StatsScope {
    /// `/alarms`, `/audit` and `/settings/users` are `None` here and build
    /// their own rows: they count rows of the portal's own tables, which needs
    /// a database read, and `routes::render` is synchronous.
    pub fn for_nav(nav: Option<&str>) -> Self {
        match nav {
            Some("nodes") => Self::Nodes,
            Some("beacons") => Self::Beacons,
            Some("hubs") => Self::Hubs,
            Some("history" | "settings" | "users" | "audit" | "alarms" | "docs" | "limits") => {
                Self::None
            }
            _ => Self::Fleet,
        }
    }

    /// The path segment of `GET /stats/{scope}`, the row's live refetch
    /// ([`PageStatsCtx::refresh`]); [`Self::from_slug`] reads it back.
    fn refresh_url(self) -> Option<&'static str> {
        match self {
            Self::Fleet => Some("/stats/fleet"),
            Self::Nodes => Some("/stats/nodes"),
            Self::Beacons => Some("/stats/beacons"),
            Self::Hubs => Some("/stats/hubs"),
            Self::None => Option::None,
        }
    }

    /// The scope a `GET /stats/{scope}` segment names. `monitoring` is not
    /// one: that row is [`monitoring_stats_ctx`], counted over the groups.
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "fleet" => Some(Self::Fleet),
            "nodes" => Some(Self::Nodes),
            "beacons" => Some(Self::Beacons),
            "hubs" => Some(Self::Hubs),
            _ => Option::None,
        }
    }

    /// The first tile names the population it counts; the other three read
    /// the same in every scope (#64).
    fn total_label(self) -> &'static str {
        match self {
            Self::Nodes => "common.metrics.total_nodes",
            Self::Beacons => "common.metrics.total_beacons",
            Self::Hubs => "common.metrics.total_hubs",
            _ => "common.metrics.total_sensors",
        }
    }

    /// The icon each list already marks its rows with, so the tile and the
    /// rows under it read as one population.
    fn total_icon(self) -> &'static str {
        match self {
            Self::Nodes => "radio",
            Self::Beacons => "eye",
            Self::Hubs => "router",
            _ => "thermometer",
        }
    }

    /// Where the tiles point, each a filter of the page the row sits on and
    /// never `/alarms` — see [`page_stats_ctx`]. Every link is the bare page
    /// plus at most one query, so a click replaces whatever filters were set
    /// rather than adding to them: the total clears them all, the others set
    /// only their own. `conn`, `oor` and `fault` are the query names
    /// `static/js/portal-list.js` reads; Monitoring renames the value one to
    /// `status`, the spelling its tiles have always emitted.
    ///
    /// `Option::None` for a scope with no list to filter, which is also what
    /// tells [`page_stats_ctx`] to build no tiles at all.
    fn filter_hrefs(self) -> Option<TileHrefs> {
        let h = |all, online, oor, fault| TileHrefs {
            all,
            online,
            oor,
            fault,
        };
        match self {
            Self::Fleet => Some(h("/", "/?conn=online", "/?status=alarming", "/?fault=any")),
            Self::Nodes => Some(h(
                "/nodes",
                "/nodes?conn=online",
                "/nodes?oor=alarming",
                "/nodes?fault=any",
            )),
            Self::Beacons => Some(h(
                "/beacons",
                "/beacons?conn=online",
                "/beacons?oor=alarming",
                "/beacons?fault=any",
            )),
            Self::Hubs => Some(h(
                "/hubs",
                "/hubs?conn=online",
                "/hubs?oor=alarming",
                "/hubs?fault=any",
            )),
            Self::None => Option::None,
        }
    }
}

/// The click-through of each tile, in row order.
#[derive(Clone, Copy)]
struct TileHrefs {
    all: &'static str,
    online: &'static str,
    oor: &'static str,
    fault: &'static str,
}

/// Counting rules started as v1's `components/MetricsBar.tsx`, which counted
/// wired lines and nothing else. Issue #64 turned the row into a per-type
/// overview strip: four tiles in every scope, counting the total of that
/// type, how many are online, how many are out of range and how many carry a
/// fault.
///
/// **The unit is the entity, in every tile and every scope** — wired lines,
/// Nodes and *registered* Beacons together on `/` (a discovered candidate is
/// not a sensor yet; it waits on `/beacons` to be added), one of those kinds
/// alone on its own list, and the Hub itself on `/hubs`. v1 counted devices
/// for "with alarms", which on a single-Hub install can only ever read 0 or 1.
///
/// The total counts what is *registered*, not what is reporting: an offline
/// Node is still a Node, the list below says so, and a Faults tile sitting
/// over a total that dropped everything faulty could only ever read 0
/// (monitoring.md §2.1 — the strip is a fleet health overview, not a row
/// count). A wired line is the one exception, and barely one: its own
/// `connected` flag is the only presence signal it has, because all eight
/// lines exist in the store whether or not a probe is plugged in.
///
/// Liveness is the detector's verdict, with the Hub's echoed `alarm_state` as
/// the fallback where it holds none, i.e. [`node_connected`] — review round
/// 2's item, these tiles were the last echo-only reader left in the portal,
/// which is how a fleet of v2-envelope Nodes could report `NeverConnected`
/// and total 0 while `/nodes` listed every one of them alive
/// (devices.backend.md §19.1, monitoring.backend.md §15.3). A Node or Beacon
/// behind an offline Hub is *not* forced offline here, because its row is not
/// either; a wired line is, because the Hub is the only voice it has.
///
/// The value class is read off a live entity only: `Warning` echoed by a Hub
/// about a Node nobody can hear is not a live excursion.
///
/// Faults are the same operational terms the rows draw their triangle from
/// ([`node_fault`], [`beacon_fault`], [`hub_fault`]), so the tile and the rows
/// it links at count the same entities — [`NodeFault::HistoryUnavailable`]
/// included, since `backfill.rs` keeps it on `NodeState::history_unavailable`
/// rather than leaving it in the jobs table where this synchronous function
/// could not read it (§19 F12, #64). A wired line has no operational fault of
/// its own at all (monitoring.md §3.1) — the Hub's liveness is the only one
/// it has.
///
/// These numbers are *live state*, not the `alarm_events` log the `/alarms`
/// page reads: an alarm row only exists if the portal was listening when the
/// Hub published the (non-retained) transition. Both value tiles therefore
/// link at a filter of the page they sit on, never at `/alarms`, so the
/// click-through always shows exactly what was counted.
pub fn page_stats_ctx(
    state: &PortalState,
    liveness: &crate::notify::offline::LivenessVerdicts,
    hub_faults: &crate::notify::HubFaults,
    scope: StatsScope,
) -> PageStatsCtx {
    use crate::notify::offline::DeviceScope;

    /// One counted entity on the axes the four tiles ask about.
    struct Counted {
        /// Registered or attached — what the total tile counts.
        present: bool,
        /// Reporting right now.
        live: bool,
        /// Value class `Warning` or `Critical`.
        out_of_range: bool,
        /// The worse of the two, for the tile's tone.
        critical: bool,
        /// An active operational fault.
        fault: bool,
    }

    /// The value axis as the tiles ask about it: (out of range, critical).
    fn value_class(state: &str) -> (bool, bool) {
        (matches!(state, "Warning" | "Critical"), state == "Critical")
    }

    /// The entities of one device this scope counts. Collected rather than
    /// chained lazily: the four sources are four iterator types and the scope
    /// picks between them.
    fn entities(
        device: &DeviceState,
        liveness: &crate::notify::offline::LivenessVerdicts,
        hub_faults: &crate::notify::HubFaults,
        scope: StatsScope,
    ) -> Vec<Counted> {
        let wired = || {
            device.sensors.iter().map(|s| {
                let (out_of_range, critical) =
                    value_class(&normalize_radio_alarm_state(&s.dominant_alarm_state));
                Counted {
                    present: s.connected,
                    live: s.connected && device.online,
                    out_of_range,
                    critical,
                    fault: false,
                }
            })
        };
        let nodes = || {
            device.nodes.values().map(|n| {
                let verdict = liveness.verdict(DeviceScope::Node, &n.node_key);
                let state = normalize_radio_alarm_state(&n.alarm_state);
                let (out_of_range, critical) = value_class(&state);
                Counted {
                    present: true,
                    live: node_connected(&state, verdict),
                    out_of_range,
                    critical,
                    fault: node_fault(n, verdict == Some(true)).is_some(),
                }
            })
        };
        let beacons = || {
            device
                .ble_sensors
                .values()
                .filter(|b| b.discovered != Some(true))
                .map(|b| {
                    let verdict = liveness.verdict(DeviceScope::Beacon, &b.mac);
                    let state = normalize_radio_alarm_state(&b.alarm_state);
                    let (out_of_range, critical) = value_class(&state);
                    Counted {
                        present: true,
                        live: node_connected(&state, verdict),
                        out_of_range,
                        critical,
                        fault: beacon_fault(b, verdict == Some(true)).is_some(),
                    }
                })
        };
        // devices.hub.md §2.1: a Hub is out of range when one of its wired
        // lines is, and faulty when its row draws the triangle.
        let hubs = || {
            let class = hub_value_class(device);
            std::iter::once(Counted {
                present: true,
                live: device.online,
                out_of_range: matches!(class, "warning" | "critical"),
                critical: class == "critical",
                fault: hub_fault(device, hub_faults),
            })
        };
        match scope {
            StatsScope::Nodes => nodes().collect(),
            StatsScope::Beacons => beacons().collect(),
            StatsScope::Hubs => hubs().collect(),
            StatsScope::Fleet => wired().chain(nodes()).chain(beacons()).collect(),
            StatsScope::None => Vec::new(),
        }
    }

    let connected = state.mqtt.connected;
    // A page the row says nothing about keeps the pill and gets no tiles.
    let Some(hrefs) = scope.filter_hrefs() else {
        return PageStatsCtx {
            connected,
            tiles: Vec::new(),
            refresh: None,
        };
    };

    let counted: Vec<Counted> = state
        .devices
        .values()
        .flat_map(|d| entities(d, liveness, hub_faults, scope))
        .collect();
    let count = |pred: fn(&Counted) -> bool| counted.iter().filter(|c| pred(c)).count() as i64;
    PageStatsCtx {
        refresh: scope.refresh_url(),
        ..stats_tiles(
            scope,
            connected,
            hrefs,
            TileCounts {
                total: count(|c| c.present),
                online: count(|c| c.present && c.live),
                out_of_range: count(|c| c.live && c.out_of_range),
                critical: count(|c| c.live && c.critical),
                faults: count(|c| c.present && c.fault),
            },
        )
    }
}

/// [`monitoring_stats_ctx`]'s refetch URL; `GET /stats/monitoring`
/// rebuilds the group sections to count them again.
pub const MONITORING_REFRESH_URL: &str = "/stats/monitoring";

/// The four numbers behind the strip.
struct TileCounts {
    total: i64,
    online: i64,
    out_of_range: i64,
    critical: i64,
    faults: i64,
}

/// The Monitoring page's strip, counted over the probes its groups show
/// (monitoring.md §2.1, amended 2026-09-25): the tiles' click-through filters
/// this page, and this page shows grouped probes only, so a tile that counted
/// the whole fleet could name a number its own filter never shows. A probe in
/// two groups is one probe. The fleet's totals are on `/nodes`, `/beacons`
/// and `/hubs`.
pub fn monitoring_stats_ctx(
    connected: bool,
    sections: &[crate::monitoring::GroupSectionCtx],
) -> PageStatsCtx {
    let mut seen = std::collections::HashSet::new();
    let rows: Vec<_> = sections
        .iter()
        .flat_map(|s| s.rows.iter())
        .filter(|r| seen.insert(r.token.clone()))
        .collect();
    let count = |pred: &dyn Fn(&crate::monitoring::GroupRowCtx) -> bool| {
        rows.iter().filter(|r| pred(r)).count() as i64
    };
    let hrefs = StatsScope::Fleet
        .filter_hrefs()
        .expect("the Monitoring scope has click-through filters");
    PageStatsCtx {
        refresh: Some(MONITORING_REFRESH_URL),
        ..stats_tiles(
            StatsScope::Fleet,
            connected,
            hrefs,
            TileCounts {
                total: rows.len() as i64,
                online: count(&|r| r.connected),
                out_of_range: count(&|r| {
                    r.connected && matches!(r.alarm_state.as_str(), "Warning" | "Critical")
                }),
                critical: count(&|r| r.connected && r.alarm_state == "Critical"),
                faults: count(&|r| !r.faults.is_empty()),
            },
        )
    }
}

fn stats_tiles(
    scope: StatsScope,
    connected: bool,
    hrefs: TileHrefs,
    TileCounts {
        total,
        online,
        out_of_range,
        critical,
        faults,
    }: TileCounts,
) -> PageStatsCtx {
    PageStatsCtx {
        connected,
        refresh: None,
        tiles: vec![
            StatTile {
                label: scope.total_label(),
                value: total,
                icon: scope.total_icon(),
                tone: "neutral",
                href: Some(hrefs.all),
                title: (scope == StatsScope::Fleet).then_some("common.metrics.total_sensors_title"),
            },
            StatTile {
                label: "common.metrics.online",
                value: online,
                icon: "circle-filled",
                tone: if online > 0 && online == total {
                    "success"
                } else {
                    "neutral"
                },
                href: Some(hrefs.online),
                title: None,
            },
            StatTile {
                label: "common.metrics.out_of_range",
                value: out_of_range,
                icon: "alert-circle",
                // #64 dropped the separate Critical tile: severity is this
                // tile's tone, and its click-through spans both classes.
                tone: if critical > 0 {
                    "critical"
                } else if out_of_range > 0 {
                    "warning"
                } else {
                    "neutral"
                },
                href: Some(hrefs.oor),
                title: None,
            },
            StatTile {
                label: "common.metrics.faults",
                value: faults,
                icon: "alert-triangle",
                tone: if faults > 0 { "alarm" } else { "neutral" },
                href: Some(hrefs.fault),
                title: None,
            },
        ],
    }
}

#[cfg(test)]
impl PageStatsCtx {
    /// The four tiles in row order — total, online, out of range, faults — so
    /// a test reads the numbers without indexing a vec by hand.
    fn counts(&self) -> (i64, i64, i64, i64) {
        let v = |i: usize| self.tiles[i].value;
        (v(0), v(1), v(2), v(3))
    }
}

/// The `/alarms` row: what the event log holds right now, not live probe
/// state. Built by `alarm_routes` because it takes a database read.
pub fn alarm_stats_ctx(connected: bool, counts: portal_storage::AlarmCounts) -> PageStatsCtx {
    PageStatsCtx {
        connected,
        refresh: None,
        tiles: vec![
            StatTile {
                label: "common.metrics.alarms_active",
                value: counts.active,
                icon: "bell",
                tone: if counts.active > 0 {
                    "warning"
                } else {
                    "neutral"
                },
                href: None,
                title: None,
            },
            StatTile {
                label: "common.metrics.alarms_critical",
                value: counts.critical,
                icon: "alert-circle",
                tone: if counts.critical > 0 {
                    "critical"
                } else {
                    "neutral"
                },
                href: Some("/alarms?severity=critical"),
                title: None,
            },
            StatTile {
                label: "common.metrics.alarms_warning",
                value: counts.warning,
                icon: "alert-triangle",
                tone: if counts.warning > 0 {
                    "warning"
                } else {
                    "neutral"
                },
                href: Some("/alarms?severity=warning"),
                title: None,
            },
            StatTile {
                label: "common.metrics.alarms_unacknowledged",
                value: counts.unacknowledged,
                icon: "circle-filled",
                tone: "neutral",
                href: None,
                title: None,
            },
        ],
    }
}

/// The metric row above `/audit`, which since 2026-10-15 is one event log
/// of alarms, warnings and the operator log (amendment 2026-10-15 in
/// plan/portal/user_admin.md). Built here for the same reason
/// `alarm_stats_ctx` is: the numbers are a database read and
/// `routes::render` is synchronous.
///
/// Every tile links to the log filtered to what it counts (`?type=`); the
/// 24-hour and failed views live in the page's Filters. An alarm is one that
/// reached CRITICAL, a warning one that peaked at WARNING (`max_severity`).
pub fn audit_stats_ctx(
    connected: bool,
    counts: portal_storage::AuditCounts,
    alarms: i64,
    warnings: i64,
) -> PageStatsCtx {
    PageStatsCtx {
        connected,
        refresh: None,
        tiles: vec![
            StatTile {
                label: "auth.audit.tile.all",
                value: counts.total + alarms + warnings,
                icon: "scroll-text",
                tone: "neutral",
                href: Some("/audit"),
                title: None,
            },
            StatTile {
                label: "auth.audit.tile.alarms",
                value: alarms,
                icon: "bell",
                tone: if alarms > 0 { "critical" } else { "neutral" },
                href: Some("/audit?type=alarm"),
                title: None,
            },
            StatTile {
                label: "auth.audit.tile.warnings",
                value: warnings,
                icon: "alert-triangle",
                tone: if warnings > 0 { "warning" } else { "neutral" },
                href: Some("/audit?type=warning"),
                title: None,
            },
            StatTile {
                label: "auth.audit.tile.logs",
                value: counts.total,
                icon: "clock",
                tone: "neutral",
                href: Some("/audit?type=log"),
                title: None,
            },
        ],
    }
}

/// The metric row above `/settings/users` (and its create and detail
/// pages), counted from the list the page is already holding — no second
/// read. Each tile links to the list filtered to what it counts, the same
/// way the device rows link `?conn=` / `?oor=` / `?fault=` (amendment
/// 2026-10-15 in plan/portal/user_admin.md): `?status=` and `?role=` are the
/// list's own filter fields, which portal-list.js seeds from the URL.
///
/// Superusers count as admins: that is what the roles mean everywhere else in
/// the portal (`layout/nav.html`'s `is_admin`), and `partials/user_row.html`
/// files a superuser under `data-role="admin"` so `?role=admin` lists exactly
/// the users this tile counts.
pub fn user_stats_ctx(connected: bool, users: &[portal_storage::User]) -> PageStatsCtx {
    let disabled = users.iter().filter(|u| !u.enabled).count() as i64;
    let admins = users
        .iter()
        .filter(|u| u.is_superuser || u.user_role == "admin")
        .count() as i64;
    let tile = |label, value, icon, tone, href| StatTile {
        label,
        value,
        icon,
        tone,
        href: Some(href),
        title: None,
    };
    PageStatsCtx {
        connected,
        refresh: None,
        tiles: vec![
            tile(
                "common.metrics.users_total",
                users.len() as i64,
                "users",
                "neutral",
                "/settings/users",
            ),
            tile(
                "common.metrics.users_active",
                users.len() as i64 - disabled,
                "check",
                "neutral",
                "/settings/users?status=enabled",
            ),
            tile(
                "common.metrics.users_admins",
                admins,
                "shield-check",
                "neutral",
                "/settings/users?role=admin",
            ),
            tile(
                "common.metrics.users_disabled",
                disabled,
                "lock",
                if disabled > 0 { "warning" } else { "neutral" },
                "/settings/users?status=disabled",
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::{beacon, node};

    /// The dashboard is the unified table, so the tiles above it must count
    /// what the table shows: Nodes and registered Beacons alongside wired
    /// lines, and never a discovered candidate (not a sensor yet).
    #[test]
    fn page_stats_counts_nodes_and_registered_beacons_but_not_candidates() {
        let mut state = PortalState::default();
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        // Two live wired lines, the other six unplugged.
        for line in 0..2 {
            device.sensors[line].connected = true;
            device.sensors[line].dominant_alarm_state = "NORMAL".into();
        }
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "CRITICAL".into();
        device.nodes.insert(n.node_key.clone(), n);
        let mut registered = beacon("AA:BB:CC:DD:EE:01");
        registered.alarm_state = "WARNING".into();
        registered.discovered = None; // wire sends null for a registered tag
        device
            .ble_sensors
            .insert(registered.mac.clone(), registered);
        let mut candidate = beacon("AA:BB:CC:DD:EE:02");
        candidate.alarm_state = "NORMAL".into();
        candidate.discovered = Some(true);
        device.ble_sensors.insert(candidate.mac.clone(), candidate);
        state.devices.insert("hub".into(), device);

        let (total, online, out_of_range, faults) = page_stats_ctx(
            &state,
            &Default::default(),
            &Default::default(),
            StatsScope::Fleet,
        )
        .counts();
        assert_eq!(total, 4, "2 lines + 1 Node + 1 registered Beacon");
        assert_eq!(online, 4);
        // Entities, not devices — v1 counted devices, which reads 0 or 1 here.
        assert_eq!(out_of_range, 2, "the Critical Node and the Warning Beacon");
        assert_eq!(faults, 0, "nothing is silent and no battery is flat");

        // …and a scoped row counts only its own population, which is the
        // whole point: "Total Nodes 4" above a list of one was the bug.
        let (nodes, ..) = page_stats_ctx(
            &state,
            &Default::default(),
            &Default::default(),
            StatsScope::Nodes,
        )
        .counts();
        assert_eq!(nodes, 1, "the Node, not the lines or the Beacon");
        let (beacons, ..) = page_stats_ctx(
            &state,
            &Default::default(),
            &Default::default(),
            StatsScope::Beacons,
        )
        .counts();
        assert_eq!(beacons, 1, "the registered Beacon, not the candidate");
        assert!(
            page_stats_ctx(
                &state,
                &Default::default(),
                &Default::default(),
                StatsScope::None
            )
            .tiles
            .is_empty(),
            "a page the row says nothing about gets no tiles"
        );
    }

    /// #64: the Faults tile counts the entities whose rows draw the fault
    /// triangle, and counts them whatever their value class says — an
    /// offline Node is exactly what the tile is for, and a total that
    /// dropped it would leave the tile unable to read anything but 0.
    #[test]
    fn page_stats_faults_count_the_rows_that_carry_one() {
        use crate::notify::offline::{DeviceScope, LivenessVerdicts};

        let mut state = PortalState::default();
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        // One line out of range: a wired line has no operational fault of
        // its own (monitoring.md §3.1), so it must not reach the tile.
        device.sensors[0].connected = true;
        device.sensors[0].dominant_alarm_state = "CRITICAL".into();

        let silent = node("aabbccdd00000001");
        device.nodes.insert(silent.node_key.clone(), silent);
        // Reporting, but contradicting itself: an alarm state for a field it
        // carries no value for is `NodeFault::MissingMeasurement`.
        let mut contradicts = node("aabbccdd00000002");
        contradicts
            .field_alarm_states
            .insert("temperature".into(), "NORMAL".into());
        device
            .nodes
            .insert(contradicts.node_key.clone(), contradicts);

        // Reporting, in range, and its only fault is a backfill job that
        // ran out of retries. This one used to be missing from the tile:
        // the fault lived in the jobs table and the strip is synchronous,
        // so it counted a triangle it could not see (#64, §19 F12).
        let mut dead_history = node("aabbccdd00000003");
        dead_history.history_unavailable = true;
        device
            .nodes
            .insert(dead_history.node_key.clone(), dead_history);

        let mut flat = beacon("AA:BB:CC:DD:EE:01");
        flat.discovered = None;
        flat.low_battery = Some(true);
        device.ble_sensors.insert(flat.mac.clone(), flat);
        state.devices.insert("hub".into(), device);

        let verdicts = LivenessVerdicts::from_verdicts([
            (DeviceScope::Node, "aabbccdd00000001", true),
            (DeviceScope::Node, "aabbccdd00000002", false),
            (DeviceScope::Node, "aabbccdd00000003", false),
            (DeviceScope::Beacon, "AA:BB:CC:DD:EE:01", false),
        ]);

        let (total, online, out_of_range, faults) =
            page_stats_ctx(&state, &verdicts, &Default::default(), StatsScope::Fleet).counts();
        assert_eq!(total, 5, "the silent Node is still a registered Node");
        assert_eq!(online, 4, "everything but the silent Node");
        assert_eq!(out_of_range, 1, "the wired line, and only the wired line");
        assert_eq!(
            faults, 4,
            "offline + self-contradicting + dead-history Node + flat Beacon, never the line"
        );

        let (_, _, _, node_faults) =
            page_stats_ctx(&state, &verdicts, &Default::default(), StatsScope::Nodes).counts();
        assert_eq!(node_faults, 3);
        let (_, _, _, beacon_faults) =
            page_stats_ctx(&state, &verdicts, &Default::default(), StatsScope::Beacons).counts();
        assert_eq!(beacon_faults, 1);
    }

    /// devices.hub.md §2.1: the Hubs row counts Hubs, its value class is the
    /// worst class of the Hub's wired lines and its fault is the one the row
    /// draws. Both read through the helpers `hub_row_ctx` uses, so the tiles
    /// cannot drift from the rows they link at.
    #[test]
    fn page_stats_hubs_scope_counts_hubs_not_their_probes() {
        let mut state = PortalState::default();

        let mut healthy = DeviceState::new("hub-a".into());
        healthy.online = true;
        // Three attached lines, one of them over its bound. The Hub is one
        // row out of range, not three.
        for line in 0..3 {
            healthy.sensors[line].connected = true;
            healthy.sensors[line].dominant_alarm_state = "NORMAL".into();
        }
        healthy.sensors[2].dominant_alarm_state = "WARNING".into();
        // …and a Node behind it, which the Hubs row must not count.
        let n = node("aabbccdd00000001");
        healthy.nodes.insert(n.node_key.clone(), n);
        state.devices.insert("hub-a".into(), healthy);

        let mut offline = DeviceState::new("hub-b".into());
        offline.online = false;
        state.devices.insert("hub-b".into(), offline);

        let stats = page_stats_ctx(
            &state,
            &Default::default(),
            &Default::default(),
            StatsScope::Hubs,
        );
        let (total, online, out_of_range, faults) = stats.counts();
        assert_eq!(total, 2, "two Hubs, not their lines and not the Node");
        assert_eq!(online, 1);
        assert_eq!(out_of_range, 1, "the Hub with a line over its bound");
        assert_eq!(faults, 1, "the offline Hub");
        assert_eq!(stats.tiles[0].label, "common.metrics.total_hubs");
        assert_eq!(stats.tiles[3].href, Some("/hubs?fault=any"));

        // An online Hub the notifier holds a system alert on is faulty too.
        let notifier = crate::notify::HubFaults::default();
        notifier.set(
            "hub-a",
            vec![crate::notify::SystemAlertKind::NetworkDisconnect],
        );
        let stats = page_stats_ctx(&state, &Default::default(), &notifier, StatsScope::Hubs);
        assert_eq!(
            stats.counts().3,
            2,
            "offline hub-b and hub-a's network alert"
        );
    }

    /// Review round 2: the four tiles were the last reader in the portal
    /// still taking Node/Beacon connectivity off the Hub's echoed
    /// `alarm_state`, so they disagreed with every row below them —
    /// `NeverConnected` (what a v2-envelope-only Node echoes for its whole
    /// life, §19.1 F3) read as absent, and a Node the detector had measured
    /// silent still counted as live and alarming (F2).
    #[test]
    fn page_stats_takes_connectivity_from_the_detector_not_the_hubs_echo() {
        use crate::notify::offline::{DeviceScope, LivenessVerdicts};

        let mut state = PortalState::default();
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        // The echo says "never connected", the detector has heard from it.
        let mut fed_by_v2 = node("aabbccdd00000001");
        fed_by_v2.alarm_state = "NeverConnected".into();
        device.nodes.insert(fed_by_v2.node_key.clone(), fed_by_v2);
        // The echo says "normal and critical", the detector says silent.
        let mut silent = node("aabbccdd00000002");
        silent.alarm_state = "CRITICAL".into();
        device.nodes.insert(silent.node_key.clone(), silent);
        // Same story on the Beacon axis.
        let mut silent_beacon = beacon("AA:BB:CC:DD:EE:01");
        silent_beacon.alarm_state = "WARNING".into();
        silent_beacon.discovered = None;
        device
            .ble_sensors
            .insert(silent_beacon.mac.clone(), silent_beacon);
        state.devices.insert("hub".into(), device);

        let verdicts = LivenessVerdicts::from_verdicts([
            (DeviceScope::Node, "aabbccdd00000001", false),
            (DeviceScope::Node, "aabbccdd00000002", true),
            (DeviceScope::Beacon, "AA:BB:CC:DD:EE:01", true),
        ]);
        let (total, online, out_of_range, faults) =
            page_stats_ctx(&state, &verdicts, &Default::default(), StatsScope::Fleet).counts();
        assert_eq!(total, 3, "all three are registered whatever the echo says");
        assert_eq!(
            online, 1,
            "only the one the detector has actually heard from"
        );
        assert_eq!(
            out_of_range, 0,
            "a value class read off a device nobody can hear is not a live alarm"
        );
        assert_eq!(faults, 2, "the two the detector calls silent");

        // And with no verdicts at all the echo still answers — the one case
        // it is entitled to (`node_connected`).
        let (total, online, out_of_range, faults) = page_stats_ctx(
            &state,
            &LivenessVerdicts::default(),
            &Default::default(),
            StatsScope::Fleet,
        )
        .counts();
        assert_eq!(total, 3);
        assert_eq!(
            online, 2,
            "the CRITICAL Node and the WARNING Beacon; NeverConnected is not live"
        );
        assert_eq!(out_of_range, 2);
        assert_eq!(faults, 0, "no verdict is not a fault");
    }

    /// The sidebar's section heading, and the rule that keeps it honest.
    ///
    /// It is drawn on the group CHANGE between two *rendered* entries rather
    /// than from the table, so a group whose every entry is gated away leaves
    /// no orphan heading. No role exercises that today — the audit log is
    /// ungated, so every signed-in user has at least one administrative entry
    /// — which is exactly why the condition is asserted from the source as
    /// well as the output: nothing else would notice it being simplified away.
    #[test]
    fn the_administration_heading_is_drawn_once_and_only_over_entries() {
        let templates = Templates::new("templates", "static");
        let render = |role: &str| {
            templates
                .env()
                .get_template("layout/nav.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    nav_active => "dashboard",
                    current_user => context! {
                        username => "u",
                        user_role => role,
                        is_superuser => false,
                        full_name => None::<String>,
                    },
                })
                .expect("nav renders")
        };

        for role in ["admin", "manager", "viewer"] {
            let nav = render(role);
            assert_eq!(
                nav.matches("Administration").count(),
                1,
                "{role}: one heading, however many entries are gated away"
            );
            // It separates the two blocks, so everything operational is above
            // it and everything administrative below.
            let heading = nav.find("Administration").expect("the heading");
            // minijinja escapes `/` inside an attribute, so an href in the
            // rendered output reads `&#x2f;nodes`. Searching for the plain
            // path finds nothing and the failure looks like a missing entry.
            let href = |path: &str| nav.find(&path.replace('/', "&#x2f;"));
            assert!(
                href("/nodes").expect("an operational entry") < heading,
                "{role}: the watch block comes first"
            );
            assert!(
                href("/docs").expect("an administrative entry") > heading,
                "{role}: the administer block comes after"
            );
            // Decision #18: the audit log is for manager and above.
            assert_eq!(
                href("/audit").is_some(),
                role != "viewer",
                "{role}: audit entry"
            );
        }

        // Users is admin-gated and is the first entry of the block; a manager
        // still gets the heading, from Audit and Settings.
        let users_href = "&#x2f;settings&#x2f;users";
        assert!(render("admin").contains(users_href));
        assert!(!render("manager").contains(users_href));

        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/layout/nav.html"),
        )
        .expect("nav template");
        assert!(
            source.contains("{% if ns.started and group != ns.group %}"),
            "the heading must key off a rendered entry, or a fully gated \
             group leaves its heading standing over nothing"
        );
    }

    /// Every nav entry gets the row that matches what its page lists, or no
    /// row at all. The mapping is asserted whole, so adding a nav entry is a
    /// decision here rather than a silent fall-through to the fleet row.
    #[test]
    fn every_page_gets_the_metric_row_it_is_about() {
        use StatsScope::*;
        for (nav, want) in [
            (Some("dashboard"), Fleet),
            (Some("nodes"), Nodes),
            (Some("beacons"), Beacons),
            // Its numbers come from the event log, so `/alarms` builds its
            // own row in `alarm_routes` and `routes::render` leaves it be.
            (Some("alarms"), None),
            (Some("hubs"), Hubs),
            (Some("history"), None),
            // `/limits` builds its own row over the limit targets.
            (Some("limits"), None),
            (Some("settings"), None),
            (Some("users"), None),
            (Some("audit"), None),
            // A page with no nav entry of its own is a fleet page; a new
            // watch surface should not have to enrol.
            (Option::None, Fleet),
        ] {
            assert_eq!(StatsScope::for_nav(nav), want, "{nav:?}");
        }
    }

    /// The tile labels reach the template through `tile.label|t`, a dynamic
    /// key the static-template sweep structurally cannot see. Without this a
    /// renamed catalogue entry would print the raw key above every page.
    #[test]
    fn page_stats_keys_exist() {
        let state = PortalState::default();
        let counts = portal_storage::AlarmCounts::default();
        let rows = [
            page_stats_ctx(
                &state,
                &Default::default(),
                &Default::default(),
                StatsScope::Fleet,
            ),
            page_stats_ctx(
                &state,
                &Default::default(),
                &Default::default(),
                StatsScope::Nodes,
            ),
            page_stats_ctx(
                &state,
                &Default::default(),
                &Default::default(),
                StatsScope::Beacons,
            ),
            page_stats_ctx(
                &state,
                &Default::default(),
                &Default::default(),
                StatsScope::Hubs,
            ),
            alarm_stats_ctx(false, counts),
            audit_stats_ctx(false, portal_storage::AuditCounts::default(), 0, 0),
            user_stats_ctx(false, &[]),
        ];
        for row in rows {
            for tile in row.tiles {
                for key in std::iter::once(tile.label).chain(tile.title) {
                    for lang in crate::i18n::LANGS {
                        assert_ne!(
                            crate::i18n::lookup(lang, key),
                            key,
                            "{key} is missing from the {lang} catalogue",
                        );
                    }
                }
            }
        }
    }

    /// Every page that declares a toolbar has to load the script that drives
    /// it, and load it in the BODY.
    ///
    /// `/audit` shipped with the tag inside `{% block title %}` — which
    /// layout/base.html renders inside `<title>`, so the browser treated it as
    /// text and the Filters and Columns buttons did nothing at all. Nothing
    /// failed: the page rendered, the markup was right, the controls were
    /// simply inert. A grep over the templates is enough to catch it and needs
    /// no context to render each page with.
    #[test]
    fn every_toolbar_page_loads_its_script_in_the_body() {
        let pages = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/pages");
        let mut checked = 0;
        for entry in std::fs::read_dir(&pages).expect("pages dir") {
            let path = entry.expect("dir entry").path();
            let html = std::fs::read_to_string(&path).expect("template");
            if !html.contains("data-list-key") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            checked += 1;

            let script = html
                .find(r#"<script src="/static/js/portal-list.js?v={{ asset_v }}""#)
                .unwrap_or_else(|| panic!("{name} declares a toolbar but never loads its script"));
            let content = html
                .find("{% block content %}")
                .unwrap_or_else(|| panic!("{name} has no content block"));
            assert!(
                script > content,
                "{name} loads the script before the content block — inside \
                 `title`, it is text and the toolbar is inert"
            );
        }
        // Not an exact census: a page gaining the toolbar should not have to
        // edit this number. Only that the sweep found something, so a rename
        // of the marker cannot turn the whole test into a no-op.
        assert!(checked >= 4, "the sweep found the lists, not zero of them");
    }

    /// The sidebar's alarm badge must not swap away the element carrying its
    /// own trigger.
    ///
    /// The first version did: one `<span>` with `hx-get`, `hx-trigger="load"`
    /// and `hx-swap="outerHTML"`, so the response replaced the trigger. The
    /// symptom was almost invisible — the badge appeared on `/alarms` and
    /// nowhere else — because that page's context happens to carry
    /// `active_count` and an include inherits it, so the one page that could
    /// not reveal the bug was the one being looked at.
    #[test]
    fn the_alarm_badge_fetches_into_itself_not_over_itself() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let nav = std::fs::read_to_string(root.join("templates/layout/nav.html"))
            .expect("the sidebar template");
        let pill_src =
            std::fs::read_to_string(root.join("templates/partials/nav_alarm_badge.html"))
                .expect("the badge fragment");
        // Its header comment explains the trap by naming the attributes, so
        // the markup has to be read without it.
        let pill = pill_src
            .split_once("#}")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or(pill_src);

        // The wrapper owns the request and survives it.
        assert!(
            nav.contains(r#"hx-get="/alarms/badge""#),
            "the wrapper fetches"
        );
        assert!(nav.contains(r#"hx-target="this""#), "into itself");
        assert!(nav.contains(r#"hx-swap="innerHTML""#), "contents only");
        assert!(
            nav.contains("sse:alarm-table"),
            "and refreshes on the alarm nudge, so the count is live"
        );

        // The fragment is the pill alone — no trigger, nothing to re-fire.
        assert!(
            !pill.contains("hx-get") && !pill.contains("hx-trigger"),
            "the swapped-in fragment must carry no trigger of its own"
        );
        assert!(
            !pill.contains("hx-swap"),
            "nor a swap directive that would outlive one response"
        );
    }

    /// F1 / decision 8: the toolbar's value-class select must offer every
    /// value class a row can carry, and nothing it cannot.
    ///
    /// Before MB2 it offered `Disconnected` — a state MB0 had already
    /// stopped emitting, since `alarm_state` became the value class alone —
    /// so picking it hid every row on the page, and a silent row could be
    /// found under no option at all. That axis is the *Spojení* select's
    /// now, reading `data-connected`.
    ///
    /// The control moved into partials/list_toolbar.html when Monitoring
    /// adopted the shared toolbar, so this reads the component rather than
    /// the page — and the same assertion now covers /nodes and /beacons too,
    /// which is the point of there being one component.
    #[test]
    fn the_status_filter_offers_exactly_the_states_a_row_can_emit() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let html = std::fs::read_to_string(root.join("templates/partials/list_toolbar.html"))
            .expect("toolbar component");

        let field = html
            .split(r#"data-list-filter="oor""#)
            .nth(1)
            .and_then(|s| s.split("</select>").next())
            .expect("the value-class select");
        let offered: Vec<&str> = field
            .match_indices(r#"<option value=""#)
            .map(|(at, _)| {
                let rest = &field[at + r#"<option value=""#.len()..];
                &rest[..rest.find('"').expect("a closed value attribute")]
            })
            .collect();

        // Everything `monitoring::value_class` can produce, plus `all` and
        // the `alarming` span the metric tiles deep-link to. The connectivity
        // states fold to Normal there, which is exactly why they have no
        // business in this control.
        assert_eq!(
            offered,
            ["all", "alarming", "normal", "warning", "critical"]
        );
        assert!(
            !html.to_lowercase().contains("disconnected"),
            "the dead option is gone from the toolbar entirely"
        );
        // …and the connection axis got its own control instead.
        assert!(html.contains(r#"data-list-filter="connection""#));

        let script = std::fs::read_to_string(root.join("static/js/portal-list.js"))
            .expect("the toolbar script");
        assert!(
            script.contains("dataset.connected"),
            "which filters on the attribute the row partials emit"
        );
    }
}
