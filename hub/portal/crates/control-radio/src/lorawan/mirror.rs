//! ChirpStack as a mirror of the Node registry — `plan/control/radio/node_registry.md` §3 (R3).
//!
//! The registry says which Nodes live on LoRaWAN; ChirpStack must hold exactly those devices.
//! [`reconcile`] runs after `create` converges, so every boot of a LoRaWAN or `dual` Hub
//! repairs the mirror:
//!
//! - a device whose Node the registry holds on **P2P** is deleted — a Node is on one carrier
//!   (decision #23), and a stale ChirpStack device would keep answering its joins — and so is
//!   one whose Node was removed while LoRaWAN was switched off (`Registry::removed`);
//! - a device the registry does **not** know is adopted into it, not deleted. It may predate
//!   the registry or have been added by hand, and deleting a device loses its keys;
//! - a `lorawan` Node with no device is created, with its keys. A Node without an `app_key`
//!   or with an unknown profile is reported and skipped.

use std::collections::BTreeSet;

use crate::error::{Result, Transport};
use crate::lorawan::client::ChirpStack;
use crate::lorawan::enroll::{EnrollmentRecord, NodesFile};
use crate::lorawan::keys::place_otaa;
use crate::lorawan::model::{DeviceSpec, OtaaKeys};
use crate::lorawan::state::{Activation, ConvergedState};
use crate::registry::{self, NodeRecord};
use crate::store::Store;

/// What one pass changed, for the `create` report.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MirrorReport {
    /// DevEUIs whose device was created from the registry.
    pub created: Vec<String>,
    /// DevEUIs whose device was deleted: the Node is registered on P2P, or was removed from
    /// the registry while LoRaWAN was off (`removed`).
    pub deleted: Vec<String>,
    /// DevEUIs of devices the registry did not know, now registered on LoRaWAN.
    pub adopted: Vec<String>,
    /// DevEUIs the pass could not bring in line, with the reason.
    pub skipped: Vec<(String, String)>,
}

impl MirrorReport {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.deleted.is_empty()
            && self.adopted.is_empty()
            && self.skipped.is_empty()
    }
}

fn parse_key(hex: &str) -> Option<[u8; 16]> {
    let bytes = crate::decode::snapshot::unhex(hex)?;
    bytes.try_into().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The profile name and activation a ChirpStack profile id stands for, if this Hub made it.
fn profile_of(state: &ConvergedState, profile_id: &str) -> Option<(String, Activation)> {
    let ids = &state.device_profile_ids;
    ids.otaa
        .iter()
        .find(|(_, id)| id.as_str() == profile_id)
        .map(|(name, _)| (name.clone(), Activation::Otaa))
        .or_else(|| {
            ids.abp
                .iter()
                .find(|(_, id)| id.as_str() == profile_id)
                .map(|(name, _)| (name.clone(), Activation::Abp))
        })
}

/// Bring ChirpStack in line with the registry. `dry_run` reports without changing anything.
pub async fn reconcile(
    client: &dyn ChirpStack,
    store: &Store,
    state: &ConvergedState,
    dry_run: bool,
) -> Result<MirrorReport> {
    let application_id = state.application_id()?.to_string();
    let devices = client.list_devices(&application_id).await?;
    let reg = registry::current(store)?;
    let mut report = MirrorReport::default();
    let mut held = BTreeSet::new();

    let mut adopt: Vec<EnrollmentRecord> = Vec::new();
    for dev in &devices {
        let id = dev.dev_eui.as_str().to_ascii_lowercase();
        held.insert(id.clone());
        match reg.get(&id) {
            Some(n) if n.radio == Transport::P2p => {
                if !dry_run {
                    client.delete_device(&dev.dev_eui).await?;
                }
                report.deleted.push(id);
            }
            Some(_) => {}
            // Removed while LoRaWAN was off: deleted now, not adopted back (§4).
            None if reg.removed.contains_key(&id) => {
                if !dry_run {
                    client.delete_device(&dev.dev_eui).await?;
                    registry::clear_removed(store, &id)?;
                }
                report.deleted.push(id);
            }
            None => {
                let Some((profile, activation)) = profile_of(state, &dev.device_profile_id) else {
                    report.skipped.push((
                        id,
                        format!(
                            "device profile {} is not one this Hub made",
                            dev.device_profile_id
                        ),
                    ));
                    continue;
                };
                let app_key = client
                    .get_device_keys(&dev.dev_eui)
                    .await?
                    .map(|k| hex(&k.app_key));
                adopt.push(EnrollmentRecord {
                    serial_number: dev.name.clone(),
                    dev_eui: dev.dev_eui.clone(),
                    profile,
                    activation: activation.as_str().to_string(),
                    join_eui: None,
                    name: None,
                    app_key,
                });
                report.adopted.push(id);
            }
        }
    }
    if !adopt.is_empty() && !dry_run {
        let mut nodes = NodesFile::load(store)?;
        for rec in adopt {
            let key = crate::p2p::session::canonical_serial(&rec.serial_number)
                .unwrap_or_else(|| rec.serial_number.clone());
            nodes.by_serial.insert(key, rec);
        }
        nodes.save(store)?;
    }

    // A removed Node whose device is already gone needs nothing more.
    if !dry_run {
        for id in reg.removed.keys().filter(|id| !held.contains(*id)) {
            registry::clear_removed(store, id)?;
        }
    }

    for n in reg.on(Transport::Lorawan) {
        if held.contains(&n.dev_eui) {
            continue;
        }
        match device_for(n, state, &application_id) {
            Ok((spec, keys)) => {
                if !dry_run {
                    client.create_device(&spec).await?;
                    client.create_device_keys(&spec.dev_eui, &keys).await?;
                }
                report.created.push(n.dev_eui.clone());
            }
            Err(why) => report.skipped.push((n.dev_eui.clone(), why)),
        }
    }

    for (what, ids) in [
        ("created", &report.created),
        (
            "deleted (not a LoRaWAN Node in the registry)",
            &report.deleted,
        ),
        ("adopted", &report.adopted),
    ] {
        for id in ids {
            eprintln!("[registry] chirpstack device {id} {what}");
        }
    }
    for (id, why) in &report.skipped {
        eprintln!("[registry] chirpstack device {id} not mirrored: {why}");
    }
    Ok(report)
}

/// The ChirpStack device and keys a `lorawan` Node stands for.
pub(crate) fn device_for(
    n: &NodeRecord,
    state: &ConvergedState,
    application_id: &str,
) -> std::result::Result<(DeviceSpec, crate::lorawan::keys::PlacedOtaaKeys), String> {
    let lorawan = n.lorawan.clone().unwrap_or(registry::LorawanParams {
        activation: Activation::Otaa.as_str().to_string(),
        join_eui: None,
    });
    if lorawan.activation != Activation::Otaa.as_str() {
        return Err(format!("{} activation is not mirrored", lorawan.activation));
    }
    let app_key = n
        .app_key
        .as_deref()
        .and_then(parse_key)
        .ok_or_else(|| "no app_key in the registry".to_string())?;
    let profile_id = state
        .profile_id(Activation::Otaa, &n.profile)
        .ok_or_else(|| format!("profile {:?} is not provisioned", n.profile))?;
    let dev_eui =
        crate::lorawan::eui::Eui::parse(&n.dev_eui).map_err(|e| e.message().to_string())?;
    Ok((
        DeviceSpec {
            dev_eui,
            application_id: application_id.to_string(),
            device_profile_id: profile_id.to_string(),
            name: n.name.clone().unwrap_or_else(|| n.serial.clone()),
            description: String::new(),
            join_eui: lorawan.join_eui,
        },
        place_otaa(&OtaaKeys { app_key }),
    ))
}
