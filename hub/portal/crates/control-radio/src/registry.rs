//! The one Node registry — `plan/control/radio/node_registry.md` (decision #23, amended
//! 2026-09-27).
//!
//! Every Node the Hub knows is one record here, keyed by its DevEUI: identity, parameters,
//! key material and the single carrier it lives on (`radio: lorawan | p2p`). The carrier arms
//! derive their own state from it — ChirpStack is reconciled from it, and the P2P
//! `sessions.db` keeps sessions only. Portal metadata (description, location, notes,
//! overrides) is not here; the Portal keys it by the same DevEUI.
//!
//! The file holds key material, so it is a secret slot (0600, [`Slot::NodeRegistry`]) and
//! written atomically under its sidecar lock like every other slot.
//!
//! R1 of the design: the type, its codec and the one-time [`migrate`] from the two stores it
//! replaces. R2–R4 moved the arms and the verbs onto it. The Portal reads it, and so do the
//! Hub's BLE Node verbs FB0D/FB11/FB12 on a radio-mode Hub (GitLab #118), with
//! [`Registry::load`]: a read never migrates it.

use std::collections::BTreeMap;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::enroll::{EnrollmentRecord, NodesFile};
use crate::lorawan::eui::Eui;
use crate::p2p::session::{canonical_serial, RegisteredNode};
use crate::store::{Slot, Store};

/// The registry document's schema version. A reader refuses a newer one rather than drop the
/// fields it does not know on its next write.
pub const REGISTRY_VERSION: u32 = 1;

/// The LoRaWAN-only parameters. Kept across a move to P2P, so a move back needs nothing
/// re-typed (§2).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LorawanParams {
    /// `otaa` or `abp`, as `EnrollmentRecord::activation` spells it.
    pub activation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_eui: Option<String>,
}

/// One Node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeRecord {
    /// 16 hex, lower case — the wire identity on both carriers (#73), and the map key.
    pub dev_eui: String,
    /// 8 hex, lower case ([`canonical_serial`]); see [`NodeRecord::serial_invalid`].
    pub serial: String,
    /// The serial could not be canonicalised when it was migrated and is kept as it was
    /// typed. New records never carry it: `node-add` refuses such a serial.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub serial_invalid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub profile: String,
    /// The Node's single carrier.
    pub radio: Transport,
    /// Hex, lower case. One root key per Node: STICKER uses its LoRaWAN AppKey as the P2P root
    /// key too. `None` only for a migrated LoRaWAN record that never carried one; such a Node
    /// cannot move to P2P until a key is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lorawan: Option<LorawanParams>,
    pub created_at: String,
    pub updated_at: String,
}

/// `node_registry.json`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Registry {
    pub version: u32,
    /// Keyed by the canonical DevEUI; each record repeats it.
    pub nodes: BTreeMap<String, NodeRecord>,
    /// LoRaWAN Nodes removed while this Hub had LoRaWAN switched off, by DevEUI, with the time
    /// (§4, amended 2026-09-27). Their ChirpStack device is still there; the mirror deletes it
    /// at the next `create` instead of adopting it, then drops the entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub removed: BTreeMap<String, String>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            version: REGISTRY_VERSION,
            nodes: BTreeMap::new(),
            removed: BTreeMap::new(),
        }
    }
}

fn registry_err(kind: Kind, message: impl Into<String>) -> Error {
    Error::new(kind, None, Component::NodeRegistry, message)
}

/// The map key for `dev_eui`, or an `InvalidArgument` naming it.
pub fn canonical_dev_eui(dev_eui: &str) -> Result<String> {
    Eui::parse(dev_eui)
        .map(|e| e.as_str().to_ascii_lowercase())
        .map_err(|_| {
            registry_err(
                Kind::InvalidArgument,
                format!("{dev_eui:?} is not a DevEUI (16 hex digits)"),
            )
        })
}

impl Registry {
    /// Load, or `None` when the registry has never been written — which is what tells
    /// [`migrate`] to run.
    pub fn load(store: &Store) -> Result<Option<Self>> {
        let Some(raw) = store.read(Slot::NodeRegistry)? else {
            return Ok(None);
        };
        let reg: Registry = serde_json::from_str(&raw).map_err(|e| {
            registry_err(
                Kind::StorageError,
                format!("node_registry.json is not readable: {e}"),
            )
        })?;
        if reg.version > REGISTRY_VERSION {
            return Err(registry_err(
                Kind::StorageError,
                format!(
                    "node_registry.json is version {}, this build reads up to {REGISTRY_VERSION}",
                    reg.version
                ),
            ));
        }
        if let Some((key, node)) = reg.nodes.iter().find(|(k, n)| **k != n.dev_eui) {
            return Err(registry_err(
                Kind::StorageError,
                format!(
                    "node_registry.json keys {key:?} to a record of dev_eui {:?}",
                    node.dev_eui
                ),
            ));
        }
        Ok(Some(reg))
    }

    pub fn save(&self, store: &Store) -> Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            registry_err(
                Kind::StorageError,
                format!("node_registry.json could not be serialized: {e}"),
            )
        })?;
        store.write(Slot::NodeRegistry, &json)
    }

    pub fn get(&self, dev_eui: &str) -> Option<&NodeRecord> {
        self.nodes.get(&dev_eui.to_ascii_lowercase())
    }

    pub fn by_serial(&self, serial: &str) -> Option<&NodeRecord> {
        let canonical = canonical_serial(serial);
        self.nodes.values().find(|n| match &canonical {
            Some(c) => n.serial == *c,
            None => n.serial_invalid && n.serial == serial.trim(),
        })
    }

    /// The Nodes on `radio`, in DevEUI order.
    pub fn on(&self, radio: Transport) -> impl Iterator<Item = &NodeRecord> + '_ {
        self.nodes.values().filter(move |n| n.radio == radio)
    }
}

/// Something the migration decided that an operator should be able to find later — a Node
/// registered on both carriers, a serial it could not canonicalise, two keys that disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationNote {
    pub dev_eui: String,
    pub message: String,
}

/// When `dev_eui` was last heard on `transport`, in the crate's UTC `…Z` spelling.
pub type LastHeard<'a> = &'a dyn Fn(&str, Transport) -> Option<String>;

/// Merge the two registries this one replaces into one record per DevEUI (§7).
///
/// - A Node on one carrier keeps it.
/// - A Node on both keeps the carrier it was **last heard on**; never heard on either, the
///   `fallback` (the carrier `radio.mode` prefers). The other side is noted, and R2/R3
///   deprovision it.
/// - Serial: P2P's, which a JoinRequest carries on air; else LoRaWAN's, canonicalised; else
///   LoRaWAN's as typed, flagged [`NodeRecord::serial_invalid`].
/// - Name and profile: the chosen carrier's, else the other's. A name equal to the serial is a
///   default and loses to a real one.
/// - Key: the chosen carrier's, as that is the one the Node works with; two keys that differ
///   are noted.
///
/// Pure, so the bench shapes are unit tests.
pub fn merge(
    lorawan: &NodesFile,
    p2p: &BTreeMap<String, RegisteredNode>,
    last_heard: LastHeard<'_>,
    fallback: Transport,
    now: &str,
) -> (Registry, Vec<MigrationNote>) {
    let mut lrw: BTreeMap<String, &EnrollmentRecord> = BTreeMap::new();
    for rec in lorawan.by_serial.values() {
        lrw.insert(rec.dev_eui.as_str().to_ascii_lowercase(), rec);
    }
    let p2p: BTreeMap<String, &RegisteredNode> = p2p
        .values()
        .map(|n| (n.dev_eui.to_ascii_lowercase(), n))
        .collect();

    let mut reg = Registry::default();
    let mut notes = Vec::new();
    let mut note = |dev_eui: &str, message: String| {
        notes.push(MigrationNote {
            dev_eui: dev_eui.to_string(),
            message,
        })
    };

    let ids: std::collections::BTreeSet<&String> = lrw.keys().chain(p2p.keys()).collect();
    for id in ids {
        let l = lrw.get(id).copied();
        let p = p2p.get(id).copied();
        let radio = match (l, p) {
            (Some(_), None) => Transport::Lorawan,
            (None, Some(_)) => Transport::P2p,
            _ => {
                let heard_l = last_heard(id, Transport::Lorawan);
                let heard_p = last_heard(id, Transport::P2p);
                let key = |h: &Option<String>| h.as_deref().map(crate::cache::at_order_key);
                let chosen = match (key(&heard_l), key(&heard_p)) {
                    (Some(a), Some(b)) if b >= a => Transport::P2p,
                    (Some(_), Some(_)) | (Some(_), None) => Transport::Lorawan,
                    (None, Some(_)) => Transport::P2p,
                    (None, None) => fallback,
                };
                note(
                    id,
                    format!(
                        "registered on both carriers; kept on {} (last heard lorawan {}, p2p {})",
                        chosen.as_str(),
                        heard_l.as_deref().unwrap_or("never"),
                        heard_p.as_deref().unwrap_or("never"),
                    ),
                );
                chosen
            }
        };

        let (serial, serial_invalid) = match (p, l) {
            (Some(p), _) => (p.serial.clone(), false),
            (None, Some(l)) => match canonical_serial(&l.serial_number) {
                Some(c) => (c, false),
                None => {
                    note(
                        id,
                        format!(
                            "serial {:?} is not a STICKER serial; kept as typed",
                            l.serial_number
                        ),
                    );
                    (l.serial_number.trim().to_string(), true)
                }
            },
            (None, None) => unreachable!("every id comes from one of the two maps"),
        };
        if let (Some(p), Some(l)) = (p, l) {
            if canonical_serial(&l.serial_number).as_deref() != Some(p.serial.as_str()) {
                note(
                    id,
                    format!(
                        "the LoRaWAN serial {:?} differs from the P2P serial {:?}; kept P2P's",
                        l.serial_number, p.serial
                    ),
                );
            }
        }

        let is_default = |name: &Option<String>, typed_serial: &str| match name.as_deref() {
            None => true,
            Some(n) => {
                let n = n.trim();
                n.is_empty()
                    || n == typed_serial.trim()
                    || canonical_serial(n).is_some_and(|c| c == serial)
            }
        };
        let l_name = l.and_then(|l| {
            (!is_default(&l.name, &l.serial_number)).then(|| l.name.clone().unwrap_or_default())
        });
        let p_name = p.and_then(|p| {
            (!is_default(&p.name, &p.serial)).then(|| p.name.clone().unwrap_or_default())
        });
        let (first_name, second_name) = match radio {
            Transport::P2p => (p_name, l_name),
            Transport::Lorawan => (l_name, p_name),
        };
        let name = first_name.or(second_name);

        let (profile, app_key) = match (radio, l, p) {
            (Transport::P2p, _, Some(p)) => {
                (p.profile.clone(), Some(p.app_key.to_ascii_lowercase()))
            }
            (Transport::Lorawan, Some(l), _) => (
                l.profile.clone(),
                l.app_key.as_ref().map(|k| k.to_ascii_lowercase()),
            ),
            _ => unreachable!("radio is a carrier the Node is on"),
        };
        let other_key = match radio {
            Transport::P2p => l
                .and_then(|l| l.app_key.as_ref())
                .map(|k| k.to_ascii_lowercase()),
            Transport::Lorawan => p.map(|p| p.app_key.to_ascii_lowercase()),
        };
        let app_key = match (app_key, other_key) {
            (Some(k), Some(o)) => {
                if k != o {
                    note(
                        id,
                        format!(
                            "the two carriers hold different keys; kept the {} key",
                            radio.as_str()
                        ),
                    );
                }
                Some(k)
            }
            (k, o) => k.or(o),
        };

        reg.nodes.insert(
            id.clone(),
            NodeRecord {
                dev_eui: id.clone(),
                serial,
                serial_invalid,
                name,
                profile,
                radio,
                app_key,
                lorawan: l.map(|l| LorawanParams {
                    activation: l.activation.clone(),
                    join_eui: l.join_eui.clone(),
                }),
                created_at: now.to_string(),
                updated_at: now.to_string(),
            },
        );
    }
    (reg, notes)
}

/// The one-time migration (§7): the registry if one exists, otherwise one merged from
/// `nodes.json` and the `sessions.db` registry and written. **Idempotent**: an existing registry
/// is never overwritten, so a second start reads the same file.
///
/// The old files stay, as mirrors for a rolled-back build (§7, amended 2026-09-27): the
/// carrier views keep rewriting them from the registry.
pub fn migrate(
    store: &Store,
    fallback: Transport,
    now: &str,
) -> Result<(Registry, Vec<MigrationNote>)> {
    let _lock = store.lock(Slot::NodeRegistry)?;
    migrate_locked(store, fallback, now)
}

/// [`migrate`] for a caller that already holds the registry lock. `flock` is per open file,
/// so taking it twice in one thread would wait on itself.
fn migrate_locked(
    store: &Store,
    fallback: Transport,
    now: &str,
) -> Result<(Registry, Vec<MigrationNote>)> {
    if let Some(reg) = Registry::load(store)? {
        return Ok((reg, Vec::new()));
    }
    // The files as they are on disk: the views' `load` reads the registry, which is being made.
    let lorawan = NodesFile::load_file(store)?;
    let p2p = crate::p2p::db::SessionsDb::load_file(store)?.nodes;
    // Each carrier's own last-heard, from the per-carrier caches a pre-registry build kept: the
    // merged cache (R5) holds one document for both and would tie every Node on both carriers.
    let heard = |dev_eui: &str, t: Transport| {
        crate::cache::Cache::legacy(store, t)
            .read(dev_eui)
            .ok()
            .flatten()
            .and_then(|d| d.last_heard())
    };
    let (reg, notes) = merge(&lorawan, &p2p, &heard, fallback, now);
    reg.save(store)?;
    for n in &notes {
        eprintln!("[registry] migrated {}: {}", n.dev_eui, n.message);
    }
    Ok((reg, notes))
}

/// The time a record is stamped with.
fn now() -> String {
    crate::lorawan::gateway::format_rfc3339(crate::lorawan::gateway::now_unix())
}

/// The registry as it stands, migrated on first use.
///
/// A Node on both carriers that neither has heard goes to P2P, the carrier decision #16
/// prefers. That keeps the answer the same whichever arm happens to run first.
pub fn current(store: &Store) -> Result<Registry> {
    if let Some(reg) = Registry::load(store)? {
        return Ok(reg);
    }
    let _lock = store.lock(Slot::NodeRegistry)?;
    Ok(migrate_locked(store, Transport::P2p, &now())?.0)
}

/// What a carrier view says about one Node: the fields its old record carried. `None` in
/// `app_key` or `lorawan` means the view does not hold that field, and the registry keeps its
/// own value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarrierRecord {
    pub dev_eui: String,
    pub serial: String,
    pub profile: String,
    pub name: Option<String>,
    pub app_key: Option<String>,
    pub lorawan: Option<LorawanParams>,
}

/// Write one carrier view's changes into the registry: `upserts` are records the view added or
/// changed, `removals` the DevEUIs it dropped. Only a change the view made is applied, so a
/// view loaded before another writer's change cannot undo it.
///
/// A removal touches only a Node on `radio`: a Node that has moved to the other carrier is not
/// the view's to drop. An upsert of a Node the registry holds on the other carrier moves it.
/// `node-add` refuses that case before it gets here (one carrier per Node, decision #23), so
/// reaching it is logged.
///
/// Lock order: a caller holding a carrier file's lock (`sessions.db`) takes the registry lock
/// here, never the other way round.
pub fn apply(
    store: &Store,
    radio: Transport,
    upserts: &[CarrierRecord],
    removals: &[String],
) -> Result<()> {
    if upserts.is_empty() && removals.is_empty() {
        return Ok(());
    }
    let _lock = store.lock(Slot::NodeRegistry)?;
    let stamp = now();
    let (mut reg, _) = migrate_locked(store, Transport::P2p, &stamp)?;
    let before = reg.clone();

    for id in removals {
        let id = id.to_ascii_lowercase();
        let upserted = upserts.iter().any(|u| u.dev_eui.eq_ignore_ascii_case(&id));
        if !upserted && reg.nodes.get(&id).is_some_and(|n| n.radio == radio) {
            reg.nodes.remove(&id);
        }
    }

    for u in upserts {
        let id = canonical_dev_eui(&u.dev_eui)?;
        // Registered again: its ChirpStack device is wanted after all.
        reg.removed.remove(&id);
        let (serial, serial_invalid) = match canonical_serial(&u.serial) {
            Some(c) => (c, false),
            None => (u.serial.trim().to_string(), true),
        };
        match reg.nodes.get_mut(&id) {
            Some(rec) => {
                let mut next = rec.clone();
                if next.radio != radio {
                    eprintln!(
                        "[registry] {id} moves from {} to {}: registered on the other carrier",
                        next.radio.as_str(),
                        radio.as_str()
                    );
                    next.radio = radio;
                }
                next.serial = serial;
                next.serial_invalid = serial_invalid;
                next.profile = u.profile.clone();
                next.name = u.name.clone();
                if let Some(k) = &u.app_key {
                    next.app_key = Some(k.to_ascii_lowercase());
                }
                if let Some(l) = &u.lorawan {
                    next.lorawan = Some(l.clone());
                }
                if next != *rec {
                    next.updated_at = stamp.clone();
                    *rec = next;
                }
            }
            None => {
                reg.nodes.insert(
                    id.clone(),
                    NodeRecord {
                        dev_eui: id,
                        serial,
                        serial_invalid,
                        name: u.name.clone(),
                        profile: u.profile.clone(),
                        radio,
                        app_key: u.app_key.as_ref().map(|k| k.to_ascii_lowercase()),
                        lorawan: u.lorawan.clone(),
                        created_at: stamp.clone(),
                        updated_at: stamp.clone(),
                    },
                );
            }
        }
    }

    if reg != before {
        reg.save(store)?;
    }
    Ok(())
}

/// Put the Node on `radio` — the registry half of `node-move`. Returns the record as it now
/// stands.
pub fn set_radio(store: &Store, dev_eui: &str, radio: Transport) -> Result<NodeRecord> {
    let _lock = store.lock(Slot::NodeRegistry)?;
    let stamp = now();
    let (mut reg, _) = migrate_locked(store, Transport::P2p, &stamp)?;
    let id = dev_eui.to_ascii_lowercase();
    let rec = reg.nodes.get_mut(&id).ok_or_else(|| {
        registry_err(
            Kind::NotFound,
            format!("no Node registered with dev_eui {id}"),
        )
    })?;
    if rec.radio != radio {
        rec.radio = radio;
        rec.updated_at = stamp;
    }
    let out = rec.clone();
    reg.save(store)?;
    Ok(out)
}

/// Remove a Node whose carrier this Hub has switched off (§4, amended 2026-09-27). A LoRaWAN
/// Node's DevEUI goes on [`Registry::removed`], so the mirror deletes its ChirpStack device later
/// instead of adopting it back. Returns the record that was removed, if the registry held it.
pub fn remove_off_carrier(store: &Store, dev_eui: &str) -> Result<Option<NodeRecord>> {
    let _lock = store.lock(Slot::NodeRegistry)?;
    let stamp = now();
    let (mut reg, _) = migrate_locked(store, Transport::P2p, &stamp)?;
    let id = dev_eui.to_ascii_lowercase();
    let removed = reg.nodes.remove(&id);
    if removed
        .as_ref()
        .is_some_and(|n| n.radio == Transport::Lorawan)
    {
        reg.removed.insert(id, stamp);
    }
    reg.save(store)?;
    Ok(removed)
}

/// Drop `dev_eui` from [`Registry::removed`] once its ChirpStack device is gone.
pub fn clear_removed(store: &Store, dev_eui: &str) -> Result<()> {
    let _lock = store.lock(Slot::NodeRegistry)?;
    let Some(mut reg) = Registry::load(store)? else {
        return Ok(());
    };
    if reg.removed.remove(&dev_eui.to_ascii_lowercase()).is_some() {
        reg.save(store)?;
    }
    Ok(())
}

/// Drop every Node on `radio` — the registry half of `clear`/`wipe` for that carrier.
pub fn drop_carrier(store: &Store, radio: Transport) -> Result<usize> {
    let _lock = store.lock(Slot::NodeRegistry)?;
    let Some(mut reg) = Registry::load(store)? else {
        return Ok(0);
    };
    let before = reg.nodes.len();
    reg.nodes.retain(|_, n| n.radio != radio);
    let dropped = before - reg.nodes.len();
    if dropped > 0 {
        reg.save(store)?;
    }
    Ok(dropped)
}

impl NodeRecord {
    /// The P2P view of this Node, or `None` when it cannot join: a P2P join is authenticated
    /// with the `app_key`, so a record without one is not admitted.
    pub fn as_registered(&self) -> Option<RegisteredNode> {
        Some(RegisteredNode {
            serial: self.serial.clone(),
            dev_eui: self.dev_eui.clone(),
            profile: self.profile.clone(),
            app_key: self.app_key.clone()?,
            name: self.name.clone(),
        })
    }

    /// The LoRaWAN view of this Node.
    pub fn as_enrollment(&self) -> Result<EnrollmentRecord> {
        let lorawan = self.lorawan.clone().unwrap_or(LorawanParams {
            activation: "otaa".to_string(),
            join_eui: None,
        });
        Ok(EnrollmentRecord {
            serial_number: self.serial.clone(),
            dev_eui: Eui::parse(&self.dev_eui)?,
            profile: self.profile.clone(),
            activation: lorawan.activation,
            join_eui: lorawan.join_eui,
            name: self.name.clone(),
            app_key: self.app_key.clone(),
        })
    }
}

impl From<&RegisteredNode> for CarrierRecord {
    fn from(n: &RegisteredNode) -> Self {
        CarrierRecord {
            dev_eui: n.dev_eui.clone(),
            serial: n.serial.clone(),
            profile: n.profile.clone(),
            name: n.name.clone(),
            app_key: Some(n.app_key.clone()),
            lorawan: None,
        }
    }
}

impl From<&EnrollmentRecord> for CarrierRecord {
    fn from(r: &EnrollmentRecord) -> Self {
        CarrierRecord {
            dev_eui: r.dev_eui.as_str().to_string(),
            serial: r.serial_number.clone(),
            profile: r.profile.clone(),
            name: r.name.clone(),
            app_key: r.app_key.clone(),
            lorawan: Some(LorawanParams {
                activation: r.activation.clone(),
                join_eui: r.join_eui.clone(),
            }),
        }
    }
}

/// What a carrier view held when it was loaded, so its `save` writes back only its own changes.
///
/// Invisible to equality and cloned by value: two views with the same records are the same
/// view, whoever loaded them.
#[derive(Debug, Default)]
pub struct ViewBase<T>(std::sync::Mutex<Option<T>>);

impl<T: Clone> ViewBase<T> {
    pub fn new(base: T) -> Self {
        ViewBase(std::sync::Mutex::new(Some(base)))
    }

    pub fn get(&self) -> Option<T> {
        self.0.lock().ok().and_then(|g| g.clone())
    }

    pub fn set(&self, base: T) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(base);
        }
    }
}

impl<T: Clone> Clone for ViewBase<T> {
    fn clone(&self) -> Self {
        ViewBase(std::sync::Mutex::new(self.get()))
    }
}

impl<T> PartialEq for ViewBase<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl<T> Eq for ViewBase<T> {}

/// The changes between a view's `base` and its records now, as [`apply`] takes them. A view
/// with no base (never loaded) counts every record as new and drops nothing.
pub fn diff<K: Ord + Clone, V: PartialEq>(
    base: Option<&BTreeMap<K, V>>,
    now: &BTreeMap<K, V>,
    record: impl Fn(&V) -> CarrierRecord,
) -> (Vec<CarrierRecord>, Vec<String>) {
    let empty = BTreeMap::new();
    let base = base.unwrap_or(&empty);
    let upserts = now
        .iter()
        .filter(|(k, v)| base.get(*k) != Some(*v))
        .map(|(_, v)| record(v))
        .collect();
    let removals = base
        .iter()
        .filter(|(k, _)| !now.contains_key(*k))
        .map(|(_, v)| record(v).dev_eui)
        .collect();
    (upserts, removals)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EUI_A: &str = "5876070000000413";
    const EUI_B: &str = "5876070000005722";
    const KEY_A: &str = "000102030405060708090a0b0c0d0e0f";
    const KEY_B: &str = "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff";

    fn lrw(serial: &str, dev_eui: &str, name: Option<&str>, key: Option<&str>) -> EnrollmentRecord {
        EnrollmentRecord {
            serial_number: serial.to_string(),
            dev_eui: Eui::parse(dev_eui).expect("eui"),
            profile: "default".to_string(),
            activation: "otaa".to_string(),
            join_eui: Some("0000000000000000".to_string()),
            name: name.map(str::to_string),
            app_key: key.map(str::to_string),
        }
    }

    fn p2p(serial: &str, dev_eui: &str, name: Option<&str>, key: &str) -> RegisteredNode {
        RegisteredNode {
            serial: serial.to_string(),
            dev_eui: dev_eui.to_string(),
            profile: "p2p-profile".to_string(),
            app_key: key.to_string(),
            name: name.map(str::to_string),
        }
    }

    fn files(
        l: Vec<EnrollmentRecord>,
        p: Vec<RegisteredNode>,
    ) -> (NodesFile, BTreeMap<String, RegisteredNode>) {
        let mut nf = NodesFile::default();
        for r in l {
            nf.by_serial.insert(r.serial_number.clone(), r);
        }
        let pm = p.into_iter().map(|n| (n.dev_eui.clone(), n)).collect();
        (nf, pm)
    }

    const NOW: &str = "2026-09-27T18:00:00Z";

    #[test]
    fn a_node_on_one_carrier_keeps_it() {
        let (nf, pm) = files(
            vec![lrw("2162165722", EUI_B, Some("Sklad"), Some(KEY_B))],
            vec![p2p("80e0684d", EUI_A, None, KEY_A)],
        );
        let (reg, notes) = merge(&nf, &pm, &|_, _| None, Transport::Lorawan, NOW);
        assert!(notes.is_empty(), "{notes:?}");
        let a = reg.get(EUI_A).expect("a");
        assert_eq!(a.radio, Transport::P2p);
        assert_eq!(a.app_key.as_deref(), Some(KEY_A));
        assert_eq!(a.lorawan, None);
        let b = reg.get(EUI_B).expect("b");
        assert_eq!(b.radio, Transport::Lorawan);
        assert_eq!(b.serial, "80e007da", "decimal serial canonicalised");
        assert_eq!(b.name.as_deref(), Some("Sklad"));
        assert_eq!(
            b.lorawan.as_ref().map(|l| l.activation.as_str()),
            Some("otaa")
        );
    }

    /// Bench 2026-09-27: 0413 in ChirpStack under a typed "33333" and in P2P under its
    /// on-air serial, heard last on P2P.
    #[test]
    fn a_node_on_both_carriers_stays_where_it_was_last_heard() {
        let (nf, pm) = files(
            vec![lrw("33333", EUI_A, Some("33333"), Some(KEY_A))],
            vec![p2p("80e0684d", EUI_A, Some("STICKER 0413"), KEY_A)],
        );
        let heard = |_: &str, t: Transport| match t {
            Transport::Lorawan => Some("2026-09-27T16:00:00.123456Z".to_string()),
            Transport::P2p => Some("2026-09-27T17:59:00Z".to_string()),
        };
        let (reg, notes) = merge(&nf, &pm, &heard, Transport::Lorawan, NOW);
        let a = reg.get(EUI_A).expect("a");
        assert_eq!(a.radio, Transport::P2p);
        assert_eq!(a.serial, "80e0684d", "P2P's on-air serial wins");
        assert_eq!(a.name.as_deref(), Some("STICKER 0413"));
        assert_eq!(a.profile, "p2p-profile");
        assert!(
            a.lorawan.is_some(),
            "LoRaWAN parameters kept for a move back"
        );
        assert!(
            notes.iter().any(|n| n.message.contains("kept on p2p")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.message.contains("serial")),
            "{notes:?}"
        );
    }

    #[test]
    fn heard_on_neither_carrier_falls_back_and_a_real_name_beats_a_default() {
        let (nf, pm) = files(
            vec![lrw("80e0684d", EUI_A, Some("Chladák"), Some(KEY_A))],
            vec![p2p("80e0684d", EUI_A, Some("80e0684d"), KEY_B)],
        );
        let (reg, notes) = merge(&nf, &pm, &|_, _| None, Transport::P2p, NOW);
        let a = reg.get(EUI_A).expect("a");
        assert_eq!(a.radio, Transport::P2p);
        assert_eq!(a.name.as_deref(), Some("Chladák"));
        assert_eq!(
            a.app_key.as_deref(),
            Some(KEY_B),
            "the chosen carrier's key"
        );
        assert!(
            notes.iter().any(|n| n.message.contains("different keys")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_serial_that_is_no_sticker_serial_is_kept_and_flagged() {
        let (nf, pm) = files(vec![lrw("node-7", EUI_B, None, None)], vec![]);
        let (reg, notes) = merge(&nf, &pm, &|_, _| None, Transport::Lorawan, NOW);
        let b = reg.get(EUI_B).expect("b");
        assert_eq!(b.serial, "node-7");
        assert!(b.serial_invalid);
        assert_eq!(b.app_key, None);
        assert_eq!(notes.len(), 1);
        assert_eq!(
            reg.by_serial("node-7").map(|n| n.dev_eui.as_str()),
            Some(EUI_B)
        );
    }

    fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-registry-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    #[test]
    fn migrate_writes_once_and_never_overwrites() {
        let store = temp_store("migrate");
        // The files as a pre-registry build left them: `NodesFile::save` would migrate.
        let raw = |nf: &NodesFile| serde_json::to_string(nf).expect("serializes");
        let (nf, _) = files(vec![lrw("2162165722", EUI_B, None, Some(KEY_B))], vec![]);
        store
            .write(Slot::LorawanNodes, &raw(&nf))
            .expect("nodes.json");
        assert_eq!(Registry::load(&store).expect("load"), None);

        let (first, _) = migrate(&store, Transport::Lorawan, NOW).expect("migrates");
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(Registry::load(&store).expect("load").as_ref(), Some(&first));

        // A second start with a changed nodes.json reads the registry as it is.
        let (nf2, _) = files(
            vec![
                lrw("2162165722", EUI_B, None, Some(KEY_B)),
                lrw("80e0684d", EUI_A, None, Some(KEY_A)),
            ],
            vec![],
        );
        store
            .write(Slot::LorawanNodes, &raw(&nf2))
            .expect("nodes.json");
        let (second, notes) =
            migrate(&store, Transport::Lorawan, "2026-09-28T00:00:00Z").expect("reads");
        assert_eq!(second, first);
        assert!(notes.is_empty());
        let _ = std::fs::remove_dir_all(store.root());
    }

    /// Bench c55, 2026-09-27: the migration read last-heard through the merged cache, so both
    /// carriers read the same time and every double registration tied. It reads each carrier's
    /// own legacy cache, and a merged cache already in place does not change that.
    #[test]
    fn migrate_reads_each_carriers_own_last_heard() {
        let store = temp_store("heard");
        let (nf, pm) = files(
            vec![lrw("33333", EUI_A, None, Some(KEY_A))],
            vec![p2p("80e0684d", EUI_A, None, KEY_A)],
        );
        store
            .write(
                Slot::LorawanNodes,
                &serde_json::to_string(&nf).expect("json"),
            )
            .expect("nodes.json");
        let mut db = crate::p2p::db::SessionsDb::default();
        db.nodes = pm;
        store
            .write(
                Slot::P2pSessions,
                &serde_json::to_string(&db).expect("json"),
            )
            .expect("sessions.db");
        for (t, at) in [
            (Transport::Lorawan, "2026-09-27T20:31:28Z"),
            (Transport::P2p, "2026-09-27T11:00:00Z"),
        ] {
            let dir = store.root().join(t.as_str()).join(crate::cache::CACHE_DIR);
            std::fs::create_dir_all(&dir).expect("dir");
            let doc = serde_json::json!({ "id": EUI_A, "message": { "at": at, "value": {} } });
            std::fs::write(dir.join(format!("{EUI_A}.json")), doc.to_string()).expect("write");
        }
        // The merge has run, as it does at the first Cache::new.
        let _ = crate::cache::Cache::new(&store, Transport::P2p);
        let (reg, _) = migrate(&store, Transport::P2p, NOW).expect("migrates");
        assert_eq!(reg.get(EUI_A).map(|n| n.radio), Some(Transport::Lorawan));
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[cfg(unix)]
    #[test]
    fn the_registry_is_a_secret_file() {
        use std::os::unix::fs::PermissionsExt;
        let store = temp_store("mode");
        Registry::default().save(&store).expect("saves");
        let mode = std::fs::metadata(store.path(Slot::NodeRegistry))
            .expect("exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn a_newer_or_mis_keyed_registry_is_refused() {
        let store = temp_store("refuse");
        store
            .write(Slot::NodeRegistry, r#"{"version": 99, "nodes": {}}"#)
            .expect("writes");
        assert_eq!(
            Registry::load(&store).expect_err("newer").kind(),
            Kind::StorageError
        );
        let (nf, _) = files(vec![lrw("2162165722", EUI_B, None, Some(KEY_B))], vec![]);
        let (mut reg, _) = merge(&nf, &BTreeMap::new(), &|_, _| None, Transport::Lorawan, NOW);
        let rec = reg.nodes.remove(EUI_B).expect("b");
        reg.nodes.insert(EUI_A.to_string(), rec);
        reg.save(&store).expect("saves");
        assert!(Registry::load(&store).is_err());
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn the_record_round_trips_and_omits_defaults() {
        let (nf, pm) = files(vec![], vec![p2p("80e0684d", EUI_A, None, KEY_A)]);
        let (reg, _) = merge(&nf, &pm, &|_, _| None, Transport::P2p, NOW);
        let json = serde_json::to_value(&reg).expect("serializes");
        let rec = &json["nodes"][EUI_A];
        assert_eq!(rec["radio"], "p2p");
        assert!(rec.get("serial_invalid").is_none());
        assert!(rec.get("lorawan").is_none());
        let back: Registry = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, reg);
        assert_eq!(
            canonical_dev_eui("5876070000000413").ok().as_deref(),
            Some(EUI_A)
        );
        assert!(canonical_dev_eui("nope").is_err());
    }
}
