//! The outputs, one function each.
//!
//! Each one decides whether it is wanted, renders it if so, and **removes a previously
//! rendered file if not**. That second half is not tidiness: every consumer's
//! `ConditionPathExists=` reads the presence of one of these files as "this Hub asked for
//! this service", so a `radio.mode` flipped from `lorawan` to `p2p` that left the old
//! ChirpStack config behind would leave a live gate open for a service the device is no
//! longer configured to run.
//!
//! **No systemd drop-in is written anywhere here, and no unit is touched.** Ordering is
//! static and lives in the units (WP-F/WP-G); a renderer-written drop-in would need the
//! `daemon-reload` this process is forbidden to issue.

use std::collections::BTreeMap;

use minijinja::Value as J;

use crate::config::{Effective, Role};
use crate::{
    dbkey, fsutil, read_optional, render_template, run_paths, RenderPaths, Report, Result,
};

/// `/run/proximos/portal/config.yaml`.
///
/// Unconditional except for the client-role gate. Written first because it is the output
/// whose absence is most visible — a missing one is a 502 from nginx.
pub(crate) fn portal(paths: &RenderPaths, cfg: &Effective, report: &mut Report) -> Result<()> {
    let (rel, group) = run_paths::PORTAL;
    let dest = paths.run.join(rel);

    // Decision #5, ported from the shim. A client-role Hub serves no portal, so the file
    // is not merely left unwritten — an existing one from a previous role is removed, the
    // same way the ChirpStack pair is. In practice this branch never fires today: nothing
    // on any device writes the role marker yet (the join is cluster.md §8, milestone C4).
    if cfg.role.is_client() {
        if fsutil::remove_stale(&dest)? {
            report.removed(&dest);
        }
        report.skip(
            &dest,
            "/data/proximos/cluster/role says client; this Hub serves no portal",
        );
        return Ok(());
    }

    let db_key = dbkey::load_or_generate(paths)?;

    let mut ctx: BTreeMap<&str, J> = BTreeMap::new();
    ctx.insert("portal", J::from_serialize(&cfg.portal));
    ctx.insert("db_key", J::from(db_key));

    let text = render_template(paths, "portal/config.yaml.j2", ctx)?;
    fsutil::write_atomic(&dest, &text, group)?;
    report.wrote(&dest);
    Ok(())
}

/// `/run/proximos/control/config.yaml` — the `radio:` section.
///
/// Written in **every** mode, `off` included: control-radio has to be able to read the
/// mode in order to refuse a command with `OffMode` (exit 35), and a Hub with no config
/// at all reports `NotBootstrapped` (exit 10) instead, which is a different and wrong
/// diagnosis.
pub(crate) fn control(paths: &RenderPaths, cfg: &Effective, report: &mut Report) -> Result<()> {
    let (rel, group) = run_paths::CONTROL;
    // `PROXIMOS_CONFIG` names the file control-radio *reads*; honouring it here makes one
    // variable mean one file from both ends.
    let dest = match &paths.control_config {
        Some(p) => p.clone(),
        None => paths.run.join(rel),
    };

    let mut ctx: BTreeMap<&str, J> = BTreeMap::new();
    ctx.insert("radio_yaml", J::from(cfg.radio_yaml.clone()));
    // The MQTT root, from the **portal's** key, so both halves are rendered with one
    // value and cannot be edited apart (plan/portal/mqtt/design.md §9.2). There is no
    // second key to keep in step: this is `portal.topic_prefix`, not a copy of it.
    ctx.insert("topic_prefix", J::from(cfg.portal.topic_prefix.clone()));

    // A Client's agent is a device on the Server's broker (cluster.md §1, §4.4). The
    // override lives here, in a file regenerated every boot, so the seeded
    // `fiber.config.yaml` is never edited — v1 rewrote it in place and a role change then
    // had to remember to undo that. `none` on a Server, which renders the block away.
    let mut broker = J::from(());
    if let Role::Client(peer) = &cfg.role {
        match peer {
            Ok(peer) => broker = J::from_serialize(peer),
            // Written all the same — control-radio needs `radio:` on a Client too — but
            // without the override, so the agent stays on its loopback broker: never
            // a half override pointing at a Server with no account or no TLS.
            Err(why) => report.degraded(
                &dest,
                format!(
                    "/data/proximos/cluster/role says client but {why}; rendered without \
                     the mqtt.broker override, so the agent stays on its loopback broker"
                ),
            ),
        }
    }
    ctx.insert("broker", broker);

    let text = render_template(paths, "control/config.yaml.j2", ctx)?;
    fsutil::write_atomic(&dest, &text, group)?;
    report.wrote(&dest);
    Ok(())
}

/// `/run/proximos/chirpstack/{chirpstack.toml, region_<region>.toml}`.
///
/// Both files or neither: `enabled_regions` naming a region whose file is absent is a
/// server that refuses to start, and a region file with no `chirpstack.toml` beside it is
/// a directory `ConditionPathExists=` correctly reads as "nothing asked for".
pub(crate) fn chirpstack(paths: &RenderPaths, cfg: &Effective, report: &mut Report) -> Result<()> {
    let (rel, group) = run_paths::CHIRPSTACK_TOML;
    let dest = paths.run.join(rel);
    let region_name = format!("region_{}.toml", cfg.region.as_str());
    let region_dest = paths.run.join(run_paths::CHIRPSTACK_DIR).join(&region_name);

    // A Client forwards into the Server's ChirpStack and runs none of its own
    // (decision #8, cluster.md §2). redis is gated on this same chirpstack.toml, so the
    // one removal takes both off a Hub that became a Client.
    let skip_reason = if !cfg.mode.wants_lorawan_server() {
        Some(format!(
            "radio.mode is {}; this Hub runs no LoRaWAN network server",
            cfg.mode.as_str()
        ))
    } else if cfg.role.is_client() {
        Some(
            "/data/proximos/cluster/role says client; this Hub forwards into the Server's \
             ChirpStack and runs none of its own"
                .to_string(),
        )
    } else {
        None
    };
    if let Some(reason) = skip_reason {
        for p in [&dest, &region_dest] {
            if fsutil::remove_stale(p)? {
                report.removed(p);
            }
        }
        // A mode flip may also have changed the region on the way past.
        for p in fsutil::stale_region_files(&paths.run.join(run_paths::CHIRPSTACK_DIR), "") {
            if fsutil::remove_stale(&p)? {
                report.removed(&p);
            }
        }
        report.skip(&dest, reason);
        return Ok(());
    }

    // The JWT signing key behind every token in lorawan.md §4's table, generated 0600
    // root:root by migration 005-chirpstack-seed.sh — which proximos-data-migrate.service
    // runs before this unit. Its absence is therefore a real fault, but NOT a fatal one:
    // failing here would fail the unit, tear down its RuntimeDirectory= and delete the
    // portal config this same run just wrote. Skipping leaves a condition-skipped
    // chirpstack.service and a working device, which is the state an operator can
    // actually diagnose from.
    let secret_path = paths.data.join(crate::data_paths::API_SECRET);
    let Some(secret_raw) = read_optional(&secret_path)? else {
        report.skip(
            &dest,
            format!(
                "{} does not exist — migration 005-chirpstack-seed.sh generates it; \
                 has proximos-data-migrate.service run?",
                secret_path.display()
            ),
        );
        return Ok(());
    };
    let secret = secret_raw.trim();
    if secret.is_empty() {
        report.skip(
            &dest,
            format!(
                "{} is empty — refusing to sign every ChirpStack token with an empty secret",
                secret_path.display()
            ),
        );
        return Ok(());
    }

    let mut ctx: BTreeMap<&str, J> = BTreeMap::new();
    ctx.insert("chirpstack", J::from_serialize(&cfg.chirpstack));
    ctx.insert("region", J::from(cfg.region.as_str()));
    ctx.insert("api_secret", J::from(secret));
    let text = render_template(paths, "chirpstack/chirpstack.toml.j2", ctx)?;

    // Selected by name rather than by a match arm, so adding a region later is adding a
    // template file. Phase 1 has exactly one, because v2 ships only EU868.
    let region_template = format!("chirpstack/region_{}.toml.j2", cfg.region.as_str());
    let mut region_ctx: BTreeMap<&str, J> = BTreeMap::new();
    region_ctx.insert("region", J::from(cfg.region.as_str()));
    region_ctx.insert("region_common_name", J::from(cfg.region.common_name()));
    // One value, both sides of the gateway backend — see the template's own warning.
    region_ctx.insert("topic_prefix", J::from(cfg.region.as_str()));
    region_ctx.insert("mqtt_server", J::from(cfg.chirpstack.mqtt_server.clone()));
    let region_text = render_template(paths, &region_template, region_ctx)?;

    // Both rendered before either is written: a template error must not leave one half of
    // an inconsistent pair on the tmpfs.
    fsutil::write_atomic(&dest, &text, group)?;
    report.wrote(&dest);
    fsutil::write_atomic(&region_dest, &region_text, group)?;
    report.wrote(&region_dest);

    // ChirpStack loads *every* region_*.toml in its config directory, so one left over
    // from a render with a different radio.region is a second region definition with a
    // different topic_prefix — not an inert file.
    for p in fsutil::stale_region_files(&paths.run.join(run_paths::CHIRPSTACK_DIR), &region_name) {
        if fsutil::remove_stale(&p)? {
            report.removed(&p);
        }
    }
    Ok(())
}

/// `/run/proximos/chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml`.
///
/// Replaces the static `/etc` file the 4.6.1 recipe ships today. Same mode gate as
/// ChirpStack: the concentratord side is udev-started and may well run on a Hub with no
/// network server, but a config that says "forward LoRaWAN" on a Hub configured for P2P
/// is a claim the renderer should not be making. **Not** the same role gate: a LoRaWAN
/// Client keeps its radio and forwards it to the Server (cluster.md §2).
pub(crate) fn forwarder(paths: &RenderPaths, cfg: &Effective, report: &mut Report) -> Result<()> {
    let (rel, group) = run_paths::FORWARDER;
    let dest = paths.run.join(rel);

    if !cfg.mode.wants_lorawan_server() {
        if fsutil::remove_stale(&dest)? {
            report.removed(&dest);
        }
        report.skip(
            &dest,
            format!(
                "radio.mode is {}; nothing forwards a LoRaWAN uplink on this Hub",
                cfg.mode.as_str()
            ),
        );
        return Ok(());
    }

    // Where the forwarder publishes. A Server's is its own loopback broker, anonymous; a
    // Client's is the Server's TLS listener under the Client's peer account, trusting only
    // the Server's CA (cluster.md §1, §4.3) — the one key v1's follower retargeted too.
    let mqtt = match &cfg.role {
        Role::Server => ForwarderMqtt {
            server: cfg.forwarder.mqtt_server.clone(),
            username: String::new(),
            password: String::new(),
            ca_cert: String::new(),
            remote: false,
        },
        Role::Client(Ok(peer)) => ForwarderMqtt {
            server: peer.ssl_url(),
            username: peer.username.clone(),
            password: peer.password.clone(),
            ca_cert: peer.ca_cert_path.clone(),
            remote: true,
        },
        // No loopback fallback: a Client has no ChirpStack to forward into, so a forwarder
        // config pointed at its own broker would be a claim that uplinks go somewhere.
        // Absent, the forwarder and the concentrator condition-skip, which says what
        // happened.
        Role::Client(Err(why)) => {
            if fsutil::remove_stale(&dest)? {
                report.removed(&dest);
            }
            report.skip(
                &dest,
                format!("/data/proximos/cluster/role says client but {why}"),
            );
            return Ok(());
        }
    };

    let mut ctx: BTreeMap<&str, J> = BTreeMap::new();
    ctx.insert("forwarder", J::from_serialize(&cfg.forwarder));
    ctx.insert("mqtt", J::from_serialize(&mqtt));
    // The cross-file contract with region_<region>.toml, rendered from the same value in
    // the same pass. This is the whole reason topic_prefix is not a ForwarderSection
    // field.
    ctx.insert("topic_prefix", J::from(cfg.region.as_str()));

    let text = render_template(
        paths,
        "chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml.j2",
        ctx,
    )?;
    fsutil::write_atomic(&dest, &text, group)?;
    report.wrote(&dest);
    Ok(())
}

/// The forwarder's `[mqtt]` connection keys, one shape for both roles so the template has
/// one set of lines rather than two branches that can drift.
#[derive(serde::Serialize)]
struct ForwarderMqtt {
    server: String,
    username: String,
    password: String,
    ca_cert: String,
    /// Selects the comment above the keys, nothing else.
    remote: bool,
}

/// `/run/proximos/mosquitto/8883.conf` — the Server's TLS listener for Client Hubs.
///
/// **Server only** (decided 2026-10-02, plan/control/cluster.md §2): written when there is
/// no client role marker, whatever the radio mode — a P2P or radio-off Server takes
/// Clients too — and removed on a Client, so a Hub that became one keeps no network
/// listener from its previous role. A Client marker with a damaged payload is still a
/// Client here: the listener is the one thing it must never open.
///
/// Its presence is two gates at once: mosquitto opens the listener because
/// `mosquitto.conf` includes this directory, and `proximos-firewall-server.service` opens
/// TCP 8883 on `ConditionPathExists=` of this path. One file, so the port and the
/// listener cannot disagree.
pub(crate) fn mosquitto_listener(
    paths: &RenderPaths,
    cfg: &Effective,
    report: &mut Report,
) -> Result<()> {
    let (rel, group) = run_paths::MOSQUITTO_LISTENER;
    let dest = paths.run.join(rel);

    if cfg.role.is_client() {
        if fsutil::remove_stale(&dest)? {
            report.removed(&dest);
        }
        report.skip(
            &dest,
            "/data/proximos/cluster/role says client; a Client's broker is loopback-only",
        );
        return Ok(());
    }

    let text = render_template(paths, "mosquitto/8883.conf.j2", BTreeMap::new())?;
    fsutil::write_atomic(&dest, &text, group)?;
    report.wrote(&dest);
    Ok(())
}
