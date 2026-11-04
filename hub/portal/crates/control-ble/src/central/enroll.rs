//! The one enrollment seam — `plan/control/ble/plan.md` §9.3, §9.6, AC 4.
//!
//! **Every surface calls these two functions.** The CLI, the FB0E/FB10 GATT characteristics, and (in
//! Part B) the signed-MQTT handlers all land here; only [`Origin`] differs. That is decision #7's
//! "no fourth mutation dialect" made concrete — three auth models, one mutation path.
//!
//! Two v1 defects are closed here, and both are about *reporting* rather than storing:
//!
//! - **The enable flip is part of the same atomic write** as the tag upsert. Two writes would leave
//!   a window where a beacon is enrolled but monitoring is off, and a crash in it is a silently
//!   half-enrolled Hub.
//! - **The flip is only claimed when the loop actually started.** v1 could flip the flag in the file
//!   and leave the monitor dead until the next reboot — its own code says the thread is spawned once
//!   at boot. So `Monitoring::Unavailable` exists to say "the file says true, the loop is not
//!   running", out loud, rather than reporting success.

use crate::config::EyeTag;
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{AuditEntry, Host, Origin};
use crate::mac::MacAddr;

/// A label is for a human reading a list on a phone, so it is bounded.
pub const MAX_NAME_CHARS: usize = 64;

/// What happened to Beacon monitoring as a side effect of an enrollment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Monitoring {
    /// `eye.enabled` was false, this call flipped it, and the scan loop started.
    EnabledByThisCall,
    /// It was already on.
    AlreadyEnabled,
    /// The flag was flipped but no loop could be started — surfaced, never silent.
    Unavailable,
}

impl Monitoring {
    pub const fn as_str(self) -> &'static str {
        match self {
            Monitoring::EnabledByThisCall => "enabled_by_this_call",
            Monitoring::AlreadyEnabled => "already_enabled",
            Monitoring::Unavailable => "unavailable",
        }
    }
}

/// What an enrollment asks the dial worker to do next (GitLab #84). A decision, not a dial: the
/// surface that holds the dial queue queues it, and the decision is made here so the three surfaces
/// cannot disagree about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUp {
    /// Read the configuration back and write nothing — always, unless the next variant applies.
    ReadConfig,
    /// Write the fleet profile and verify it: a **new** enrollment with `eye.auto_provision`
    /// on. A re-add is a rename and never rewrites a beacon.
    Provision,
}

impl FollowUp {
    pub const fn as_str(self) -> &'static str {
        match self {
            FollowUp::ReadConfig => "read_config",
            FollowUp::Provision => "provision",
        }
    }

    pub fn dial_kind(self) -> crate::tasks::DialKind {
        match self {
            FollowUp::ReadConfig => crate::tasks::DialKind::ReadConfig,
            FollowUp::Provision => crate::tasks::DialKind::Provision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AddOutcome {
    pub mac: MacAddr,
    pub name: Option<String>,
    pub monitoring: Monitoring,
    /// The MAC was already enrolled. **Not an error**: FB0E and the signed `beacon-add` are upserts,
    /// so re-adding is how a phone renames a Beacon.
    pub already_enrolled: bool,
    pub follow_up: FollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RemoveOutcome {
    pub mac: MacAddr,
    pub removed: bool,
}

fn bad_name(message: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Role::Central,
        Component::BeaconRegistry,
        message,
    )
}

/// Validate and normalize a label: trimmed, blank becomes absent, bounded, no control characters.
fn clean_name(name: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = name else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // A blank label is the operator clearing it, not a label of zero length.
        return Ok(None);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(bad_name(
            "--name contains a control character: a Beacon label is displayed on a phone and in a \
             list, so it must be plain text",
        ));
    }
    let count = trimmed.chars().count();
    if count > MAX_NAME_CHARS {
        return Err(bad_name(format!(
            "--name is {count} characters: at most {MAX_NAME_CHARS} are allowed"
        )));
    }
    Ok(Some(trimmed.to_string()))
}

/// A cluster Client registers no Beacons (decision #42, Q6): the registry is the Server's, and
/// every surface — FB0E, the CLI, the signed command — reaches this seam, so one check covers
/// them all. Checked before anything else so a Client leaves no audit row or forgotten ledger.
fn refuse_on_client(host: &Host) -> Result<()> {
    match host.cluster.read() {
        Ok(status) if status.role == Some(crate::host::HubRole::Client) => {
            Err(crate::central::cluster::client_refusal(
                status.server.as_deref().unwrap_or("of this cluster"),
            ))
        }
        _ => Ok(()),
    }
}

/// Enroll a Beacon, turning monitoring on if it was off — in **one** atomic write.
pub fn add(host: &Host, origin: Origin, mac: &str, name: Option<&str>) -> Result<AddOutcome> {
    refuse_on_client(host)?;
    let mac = MacAddr::parse(mac)?;
    let canonical = mac.to_string();
    let name = clean_name(name)?;

    let mut flipped = false;
    let mut already_enrolled = false;
    let mut provision_on_enroll = false;
    host.config.mutate(&mut |section| {
        // Reset per invocation: a store that retries the closure must not accumulate.
        flipped = false;
        already_enrolled = false;
        provision_on_enroll = section.auto_provision;

        if !section.enabled {
            section.enabled = true;
            flipped = true;
        }
        match section
            .tags
            .iter_mut()
            .find(|t| t.mac.eq_ignore_ascii_case(&canonical))
        {
            Some(tag) => {
                already_enrolled = true;
                tag.enabled = true;
                // Normalize the stored spelling, so the file converges on the canonical form.
                tag.mac = canonical.clone();
                // Only overwrite the label when one was actually given: dropping a Beacon's name
                // because the operator omitted `--name` would be a surprising erasure.
                if name.is_some() {
                    tag.name = name.clone();
                }
            }
            None => section.tags.push(EyeTag {
                mac: canonical.clone(),
                name: name.clone(),
                enabled: true,
                logging_interval_min: None,
                recording: None,
                field_thresholds: Vec::new(),
                provisioned: None,
            }),
        }
    })?;

    // The loop start is deliberately *after* the durable write: claiming a running loop before the
    // enrollment is on disk would be the opposite lie.
    let monitoring = if flipped {
        match host.subsystem.ensure_scanning() {
            Ok(true) => Monitoring::EnabledByThisCall,
            // Ok(false) means "nothing was started" — in a one-shot CLI there is no loop to start.
            Ok(false) | Err(_) => Monitoring::Unavailable,
        }
    } else {
        Monitoring::AlreadyEnabled
    };

    let follow_up = if provision_on_enroll && !already_enrolled {
        FollowUp::Provision
    } else {
        FollowUp::ReadConfig
    };

    // Exactly one audit row per call (AC 4), naming the v2 verb — never a `*_eye_*` string.
    host.config.audit(&AuditEntry {
        action: "beacon-add",
        mac: Some(canonical),
        origin,
        detail: format!(
            "{}; monitoring {}{}",
            if already_enrolled {
                "already enrolled, record updated"
            } else {
                "enrolled"
            },
            monitoring.as_str(),
            if flipped {
                " (eye.enabled flipped to true in the same write)"
            } else {
                ""
            }
        ),
    });

    Ok(AddOutcome {
        mac,
        name,
        monitoring,
        already_enrolled,
        follow_up,
    })
}

/// Remove a Beacon. An unknown MAC is `removed: false`, **not** an error — the FB10 contract is
/// `success:false` with a message, and the CLI is the surface that turns it into an exit code.
///
/// A removal is complete (central.md §5, amended 2026-09-23): besides the enrollment, the central
/// role forgets the beacon's provisioning record and reported configuration, so a re-add starts
/// from nothing. Asked for an unknown MAC too, so a record left behind by an older build goes.
pub fn remove(host: &Host, origin: Origin, mac: &str) -> Result<RemoveOutcome> {
    refuse_on_client(host)?;
    let mac = MacAddr::parse(mac)?;
    let canonical = mac.to_string();

    let mut removed = false;
    host.config.mutate(&mut |section| {
        removed = false;
        let before = section.tags.len();
        section
            .tags
            .retain(|t| !t.mac.eq_ignore_ascii_case(&canonical));
        removed = section.tags.len() != before;
    })?;
    host.subsystem.forget_beacon(&mac);

    host.config.audit(&AuditEntry {
        action: "beacon-remove",
        mac: Some(canonical),
        origin,
        detail: if removed {
            "removed".to_string()
        } else {
            "not enrolled: nothing to remove".to_string()
        },
    });

    Ok(RemoveOutcome { mac, removed })
}

/// Turn Beacon monitoring on or off — the explicit twin of the auto-enable `add` performs.
///
/// Shares `add`'s seam on purpose: the flag flip and the runtime loop start/stop are **one**
/// operation, so the signed `beacon-monitoring` command and this one cannot diverge.
pub fn set_monitoring(host: &Host, origin: Origin, enabled: bool) -> Result<MonitoringOutcome> {
    let mut changed = false;
    host.config.mutate(&mut |section| {
        changed = section.enabled != enabled;
        section.enabled = enabled;
    })?;

    let monitoring = if enabled {
        match host.subsystem.ensure_scanning() {
            Ok(true) => Monitoring::EnabledByThisCall,
            Ok(false) | Err(_) if changed => Monitoring::Unavailable,
            _ => Monitoring::AlreadyEnabled,
        }
    } else {
        host.subsystem.stop_scanning();
        Monitoring::AlreadyEnabled
    };

    host.config.audit(&AuditEntry {
        action: "beacon-monitoring",
        mac: None,
        origin,
        detail: format!(
            "eye.enabled set to {enabled}{}; monitoring {}",
            if changed { "" } else { " (already)" },
            monitoring.as_str()
        ),
    });

    Ok(MonitoringOutcome {
        enabled,
        changed,
        monitoring,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MonitoringOutcome {
    pub enabled: bool,
    pub changed: bool,
    pub monitoring: Monitoring,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EyeSection;
    use crate::host::{
        ConfigStore, MemoryConfigStore, NeverStandby, NullPublisher, NullReadingSink,
        StaticDeviceInfo, SubsystemControl, SwitchableSubsystem, UnavailableNetwork,
        UnwiredExportDestination, UnwiredNodeEnroll, UnwiredPortalPairing, UnwiredRadioMode,
        UnwiredSetupState, WallClock,
    };
    use std::sync::Arc;

    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    struct Fixture {
        host: Host,
        config: Arc<MemoryConfigStore>,
        subsystem: Arc<SwitchableSubsystem>,
    }

    /// A host whose config store and subsystem we can inspect afterwards — the whole point of the
    /// AC 4 assertions is the *number* of writes and audit rows, so they must be observable.
    fn fixture(section: EyeSection) -> Fixture {
        let config = Arc::new(MemoryConfigStore::with_section(section));
        let subsystem = Arc::new(SwitchableSubsystem::default());
        let host = Host {
            config: config.clone(),
            publisher: Arc::new(NullPublisher),
            readings: Arc::new(NullReadingSink),
            subsystem: subsystem.clone(),
            standby: Arc::new(NeverStandby),
            network: Arc::new(UnavailableNetwork),
            clock: Arc::new(WallClock),
            device: Arc::new(StaticDeviceInfo::for_tests()),
            nodes: Arc::new(UnwiredNodeEnroll),
            setup: Arc::new(UnwiredSetupState),
            export: Arc::new(UnwiredExportDestination),
            pairing: Arc::new(UnwiredPortalPairing),
            radio: Arc::new(UnwiredRadioMode),
            cluster: Arc::new(crate::host::UnwiredClusterJoin),
        };
        Fixture {
            host,
            config,
            subsystem,
        }
    }

    fn disabled() -> EyeSection {
        EyeSection {
            enabled: false,
            ..EyeSection::default()
        }
    }

    #[test]
    fn an_enrollment_reads_the_beacon_and_configures_it_only_when_that_is_switched_on() {
        // The default, and what every Hub does until the portal exposes the switch.
        let f = fixture(EyeSection::default());
        let new = add(&f.host, Origin::Cli, MAC, None).expect("enrolls");
        assert_eq!(new.follow_up, FollowUp::ReadConfig);
        let again = add(&f.host, Origin::Cli, MAC, Some("Freezer 1")).expect("re-adds");
        assert_eq!(again.follow_up, FollowUp::ReadConfig);

        let f = fixture(EyeSection {
            auto_provision: true,
            ..EyeSection::default()
        });
        let new = add(&f.host, Origin::SignedMqtt, MAC, None).expect("enrolls");
        assert_eq!(new.follow_up, FollowUp::Provision);
        assert_eq!(new.follow_up.dial_kind(), crate::tasks::DialKind::Provision);
        let rename = add(&f.host, Origin::Ble, MAC, Some("Freezer 1")).expect("re-adds");
        assert_eq!(
            rename.follow_up,
            FollowUp::ReadConfig,
            "a re-add is a rename and never rewrites a beacon"
        );
    }

    #[test]
    fn enrolling_while_disabled_flips_enabled_in_the_same_write_and_reports_it() {
        let f = fixture(disabled());
        let out = add(&f.host, Origin::Cli, MAC, Some("Freezer 1")).expect("enrolls");

        assert_eq!(out.monitoring, Monitoring::EnabledByThisCall);
        assert!(!out.already_enrolled);
        assert_eq!(out.name.as_deref(), Some("Freezer 1"));

        // AC 4: ONE write, ONE audit row.
        assert_eq!(
            f.config.write_count(),
            1,
            "the flip and the tag are one write"
        );
        assert_eq!(f.config.audit_entries().len(), 1);

        let section = f.config.load().expect("load");
        assert!(section.enabled);
        assert_eq!(section.tags.len(), 1);
        assert_eq!(section.tags[0].mac, MAC);
        assert!(section.tags[0].enabled);

        // The loop really is running when the call returns.
        assert!(f.subsystem.is_scanning());
        let audit = &f.config.audit_entries()[0];
        assert_eq!(audit.action, "beacon-add");
        assert!(audit.detail.contains("flipped"), "{}", audit.detail);
    }

    #[test]
    fn enrolling_while_enabled_reports_already_enabled() {
        let f = fixture(EyeSection::default());
        let out = add(&f.host, Origin::Ble, MAC, None).expect("enrolls");
        assert_eq!(out.monitoring, Monitoring::AlreadyEnabled);
        assert_eq!(f.config.write_count(), 1);
        assert_eq!(f.config.audit_entries().len(), 1);
        assert_eq!(f.config.audit_entries()[0].origin, Origin::Ble);
    }

    #[test]
    fn enable_flip_without_a_running_loop_reports_unavailable() {
        // The one-shot CLI: nothing hosts the loops, so `ensure_scanning` starts nothing. The flag
        // is still flipped — but the result says so instead of claiming monitoring is live.
        let config = Arc::new(MemoryConfigStore::with_section(disabled()));
        let host = Host {
            config: config.clone(),
            publisher: Arc::new(NullPublisher),
            readings: Arc::new(NullReadingSink),
            subsystem: Arc::new(crate::host::NoopSubsystem),
            standby: Arc::new(NeverStandby),
            network: Arc::new(UnavailableNetwork),
            clock: Arc::new(WallClock),
            device: Arc::new(StaticDeviceInfo::for_tests()),
            nodes: Arc::new(UnwiredNodeEnroll),
            setup: Arc::new(UnwiredSetupState),
            export: Arc::new(UnwiredExportDestination),
            pairing: Arc::new(UnwiredPortalPairing),
            radio: Arc::new(UnwiredRadioMode),
            cluster: Arc::new(crate::host::UnwiredClusterJoin),
        };
        let out = add(&host, Origin::Cli, MAC, None).expect("enrolls");
        assert_eq!(out.monitoring, Monitoring::Unavailable);
        assert!(
            config.load().expect("load").enabled,
            "the flag is still flipped — the report is about the loop, not the file"
        );
    }

    #[test]
    fn re_adding_updates_the_record_instead_of_failing() {
        let f = fixture(EyeSection::default());
        add(&f.host, Origin::Cli, MAC, Some("First")).expect("first");
        let out = add(&f.host, Origin::Cli, MAC, Some("Second")).expect("second");
        assert!(out.already_enrolled, "an upsert, not AlreadyExists");
        let section = f.config.load().expect("load");
        assert_eq!(section.tags.len(), 1, "no duplicate row");
        assert_eq!(section.tags[0].name.as_deref(), Some("Second"));
    }

    #[test]
    fn re_adding_without_a_name_keeps_the_existing_one() {
        let f = fixture(EyeSection::default());
        add(&f.host, Origin::Cli, MAC, Some("Freezer 1")).expect("first");
        add(&f.host, Origin::Cli, MAC, None).expect("second");
        assert_eq!(
            f.config.load().expect("load").tags[0].name.as_deref(),
            Some("Freezer 1"),
            "omitting --name must not erase a label"
        );
    }

    #[test]
    fn a_re_add_that_changes_nothing_writes_nothing() {
        let f = fixture(EyeSection::default());
        add(&f.host, Origin::Cli, MAC, Some("Freezer 1")).expect("first");
        assert_eq!(f.config.write_count(), 1);
        add(&f.host, Origin::Cli, MAC, Some("Freezer 1")).expect("identical");
        assert_eq!(
            f.config.write_count(),
            1,
            "an idempotent re-add must not churn the fleet file"
        );
        // It still audits, because the operator did ask.
        assert_eq!(f.config.audit_entries().len(), 2);
    }

    #[test]
    fn a_lower_case_mac_is_stored_canonically() {
        let f = fixture(EyeSection::default());
        add(&f.host, Origin::Cli, "aa:bb:cc:dd:ee:ff", None).expect("enrolls");
        assert_eq!(f.config.load().expect("load").tags[0].mac, MAC);
        // And the same beacon in another spelling is the same row.
        add(&f.host, Origin::Cli, "AABBCCDDEEFF", None).expect("enrolls");
        assert_eq!(f.config.load().expect("load").tags.len(), 1);
    }

    #[test]
    fn names_are_bounded_and_control_characters_refused() {
        let f = fixture(EyeSection::default());
        let long = "x".repeat(MAX_NAME_CHARS + 1);
        let err = add(&f.host, Origin::Cli, MAC, Some(&long)).expect_err("too long");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert_eq!(err.component(), Component::BeaconRegistry);

        let err = add(&f.host, Origin::Cli, MAC, Some("Freezer\n1")).expect_err("control char");
        assert_eq!(err.kind(), Kind::InvalidArgument);

        // Exactly at the bound is fine, and a blank name is simply absent.
        let ok = "y".repeat(MAX_NAME_CHARS);
        add(&f.host, Origin::Cli, MAC, Some(&ok)).expect("at the bound");
        let out = add(&f.host, Origin::Cli, MAC, Some("   ")).expect("blank");
        assert_eq!(out.name, None);

        // A refused name must not have written anything.
        assert_eq!(
            f.config
                .load()
                .expect("load")
                .tags
                .iter()
                .filter(|t| t.name.as_deref() == Some(long.as_str()))
                .count(),
            0
        );
    }

    #[test]
    fn a_malformed_mac_is_refused_before_any_write() {
        let f = fixture(EyeSection::default());
        let err = add(&f.host, Origin::Cli, "AA:BB:CC:DD:EE", None).expect_err("five octets");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert_eq!(f.config.write_count(), 0);
        assert!(
            f.config.audit_entries().is_empty(),
            "and nothing is audited"
        );
    }

    #[test]
    fn remove_unknown_is_not_an_error() {
        let f = fixture(EyeSection::default());
        let out = remove(&f.host, Origin::Ble, MAC).expect("not an error");
        assert!(!out.removed);
        assert_eq!(
            f.config.write_count(),
            0,
            "nothing changed, nothing written"
        );
        assert_eq!(
            f.config.audit_entries().len(),
            1,
            "the attempt is still recorded"
        );
    }

    #[test]
    fn remove_is_case_insensitive() {
        let f = fixture(EyeSection::default());
        add(&f.host, Origin::Cli, MAC, None).expect("enrolls");
        let out = remove(&f.host, Origin::Cli, "aa:bb:cc:dd:ee:ff").expect("removes");
        assert!(out.removed);
        assert!(f.config.load().expect("load").tags.is_empty());
    }

    #[test]
    fn add_then_remove_leaves_the_section_as_before_except_enabled() {
        let f = fixture(disabled());
        let before = f.config.load().expect("load");
        add(&f.host, Origin::Cli, MAC, Some("Freezer 1")).expect("enrolls");
        remove(&f.host, Origin::Cli, MAC).expect("removes");
        let after = f.config.load().expect("load");

        assert_eq!(after.tags, before.tags, "the tag list is back to empty");
        assert_eq!(after.en12830, before.en12830);
        assert_eq!(after.auto_provision, before.auto_provision);
        assert_eq!(after.legacy, before.legacy);
        // Monitoring stays on: removal is not a reason to stop hearing the rest of the fleet.
        assert!(!before.enabled);
        assert!(after.enabled);
    }

    #[test]
    fn every_origin_produces_the_same_store_effect_and_differs_only_in_the_audit_row() {
        // AC 4's equivalence, at the store layer: the CLI, FB10 and the signed command are
        // indistinguishable except for who asked.
        let mut sections = Vec::new();
        for origin in [Origin::Cli, Origin::Ble, Origin::SignedMqtt] {
            let f = fixture(disabled());
            add(&f.host, origin, MAC, Some("Freezer 1")).expect("enrolls");
            let section = f.config.load().expect("load");
            let audit = f.config.audit_entries();
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].action, "beacon-add");
            assert_eq!(audit[0].origin, origin);
            sections.push(section);
        }
        assert_eq!(sections[0], sections[1]);
        assert_eq!(sections[1], sections[2]);
    }

    #[test]
    fn set_monitoring_is_the_same_seam_and_reports_whether_it_changed_anything() {
        let f = fixture(disabled());
        let out = set_monitoring(&f.host, Origin::SignedMqtt, true).expect("enables");
        assert!(out.enabled);
        assert!(out.changed);
        assert_eq!(out.monitoring, Monitoring::EnabledByThisCall);
        assert!(f.subsystem.is_scanning());

        // Idempotent: enabling an enabled Hub changes nothing and writes nothing.
        let out = set_monitoring(&f.host, Origin::SignedMqtt, true).expect("again");
        assert!(!out.changed);
        assert_eq!(f.config.write_count(), 1);

        // Disabling idles the loop rather than leaving the flag alone.
        let out = set_monitoring(&f.host, Origin::SignedMqtt, false).expect("disables");
        assert!(!out.enabled);
        assert!(out.changed);
        assert!(!f.subsystem.is_scanning());
        assert_eq!(f.subsystem.stops(), 1);
        assert_eq!(f.config.audit_entries().len(), 3);
    }

    #[test]
    fn the_monitoring_vocabulary_is_snake_case_on_the_wire() {
        for (m, s) in [
            (Monitoring::EnabledByThisCall, "enabled_by_this_call"),
            (Monitoring::AlreadyEnabled, "already_enabled"),
            (Monitoring::Unavailable, "unavailable"),
        ] {
            assert_eq!(m.as_str(), s);
            assert_eq!(serde_json::to_value(m).expect("serialize"), s);
        }
    }

    #[test]
    fn a_cluster_client_refuses_add_and_remove_and_touches_nothing() {
        let mut f = fixture(EyeSection::default());
        f.host.cluster = Arc::new(crate::host::MemoryClusterJoin::with_status(
            crate::host::ClusterJoinStatus {
                role: Some(crate::host::HubRole::Client),
                joinable: false,
                server: Some("proximos-4e970f4a".into()),
            },
        ));
        let err = add(&f.host, Origin::Ble, MAC, None).expect_err("a Client refuses");
        assert_eq!(err.kind(), crate::error::Kind::CapabilityDisabled);
        assert_eq!(
            err.message(),
            "this Hub is a cluster Client; Beacons are registered on the Server \
             proximos-4e970f4a"
        );
        for origin in [Origin::Cli, Origin::SignedMqtt] {
            assert!(add(&f.host, origin, MAC, None).is_err());
            assert!(remove(&f.host, origin, MAC).is_err());
        }
        assert_eq!(f.config.write_count(), 0);
        assert!(f.config.audit_entries().is_empty());
    }
}
