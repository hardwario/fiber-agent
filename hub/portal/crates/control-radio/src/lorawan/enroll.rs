//! Node enrollment — `plan/control/radio/lorawan.md` §7, §7.1.
//!
//! Two things here are corrections to v1 rather than ports of it.
//!
//! **The ask-first upsert.** v1's `set_device_keys` calls `CreateKeys` unconditionally and its
//! caller logs-and-continues on any ChirpStack error, so **re-adding a Node in v1 with a new
//! AppKey silently keeps the old key**: the add reports success and the Node never joins with
//! the new credential. v2 gets the device and its keys first, then creates or updates. It
//! costs one extra round trip on a human-triggered operation and removes a dependence on an
//! error message that varies by storage backend.
//!
//! **ABP enrollment is refused in phase 1** (§7.1) — locally, before any RPC, as the first
//! statement of [`Enroller::enroll`]. The ChirpStack side stays prepared: `create` still makes
//! the ABP profile, so ChirpStack never needs a later schema change. Not deprecated, just not
//! wired into the enrollment path yet.

use std::collections::BTreeMap;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::client::ChirpStack;
use crate::lorawan::eui::Eui;
use crate::lorawan::keyfile::{self, KeyMaterial};
use crate::lorawan::keys::{place_abp, place_otaa};
use crate::lorawan::model::DeviceSpec;
use crate::lorawan::state::{Activation, ConvergedState};
use crate::store::{Slot, Store};

/// Phase 1 refuses ABP enrollment. Flipping this is a phase-2 decision, not a config knob —
/// decision #8 scopes it, so it is a compile-time constant that a reviewer can find.
pub const ABP_ENROLLMENT_ENABLED: bool = false;

/// One row of `nodes.json`: the local serial↔dev_eui mapping plus what the device was
/// enrolled with. Holds key material, which is why the file is 0600.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnrollmentRecord {
    pub serial_number: String,
    pub dev_eui: Eui,
    pub profile: String,
    pub activation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_eui: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hex, lowercase. Returned in plaintext by `node-info` — decided, not defaulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_key: Option<String>,
}

/// `nodes.json` — the enrollment records, keyed by serial.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct NodesFile {
    pub by_serial: BTreeMap<String, EnrollmentRecord>,
    /// The records [`NodesFile::load`] took from the registry, so [`NodesFile::save`] writes
    /// back only what this copy changed.
    #[serde(skip)]
    registry_base: crate::registry::ViewBase<BTreeMap<String, EnrollmentRecord>>,
}

impl NodesFile {
    /// The registry's LoRaWAN view (node_registry.md, R3): the Nodes whose `radio` is
    /// `lorawan`, keyed by their canonical serial. `nodes.json` itself is only a mirror a
    /// rolled-back build reads.
    pub fn load(store: &Store) -> Result<Self> {
        let mut by_serial = BTreeMap::new();
        for n in crate::registry::current(store)?.on(Transport::Lorawan) {
            by_serial.insert(n.serial.clone(), n.as_enrollment()?);
        }
        Ok(NodesFile {
            registry_base: crate::registry::ViewBase::new(by_serial.clone()),
            by_serial,
        })
    }

    /// The file alone, as a pre-registry build wrote it — what the migration merges.
    pub fn load_file(store: &Store) -> Result<Self> {
        match store.read(Slot::LorawanNodes)? {
            None => Ok(Self::default()),
            Some(raw) if raw.trim().is_empty() => Ok(Self::default()),
            Some(raw) => serde_json::from_str(&raw).map_err(|e| {
                Error::new(
                    Kind::StorageError,
                    Transport::Lorawan,
                    Component::EnrollmentRecord,
                    format!("nodes.json is not readable: {e}"),
                )
            }),
        }
    }

    /// Save: this copy's changes go to the registry first, then `nodes.json`, the mirror.
    pub fn save(&self, store: &Store) -> Result<()> {
        let (upserts, removals) =
            crate::registry::diff(self.registry_base.get().as_ref(), &self.by_serial, |r| {
                crate::registry::CarrierRecord::from(r)
            });
        crate::registry::apply(store, Transport::Lorawan, &upserts, &removals)?;
        self.registry_base.set(self.by_serial.clone());
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            Error::new(
                Kind::StorageError,
                Transport::Lorawan,
                Component::EnrollmentRecord,
                format!("nodes.json could not be serialized: {e}"),
            )
        })?;
        store.write(Slot::LorawanNodes, &json)
    }

    pub fn by_dev_eui(&self, eui: &Eui) -> Option<&EnrollmentRecord> {
        self.by_serial.values().find(|r| r.dev_eui == *eui)
    }

    /// The record a command addressed, by whichever identifier it used.
    ///
    /// `None` rather than an error: only the caller knows whether a missing record is a
    /// failure (`node-info` has nothing to show) or not (`node-remove --dev-eui` addresses
    /// ChirpStack directly, §5's one asymmetric corner).
    pub fn find(&self, node: &NodeRef) -> Option<&EnrollmentRecord> {
        match node {
            NodeRef::Serial(s) => self.key_for(s).and_then(|k| self.by_serial.get(&k)),
            NodeRef::DevEui(e) => self.by_dev_eui(e),
        }
    }

    /// The key a serial is held under, however the operator spelled it: the registry keys by
    /// the canonical 8-hex serial, an operator may type the decimal one.
    pub fn key_for(&self, serial: &str) -> Option<String> {
        if self.by_serial.contains_key(serial) {
            return Some(serial.to_string());
        }
        crate::p2p::session::canonical_serial(serial).filter(|c| self.by_serial.contains_key(c))
    }
}

/// Whether a rendered enrollment record carries its plaintext key.
///
/// Two consumers, one shape: the CLI readback includes `app_key` because `plan.md` §5 decides
/// it does ("the operational need to verify a key actually matches what was provisioned …
/// outweighs the exposure concern"), and the retained MQTT document never does — a broker
/// topic is a different exposure surface from a control-socket read, and §5's decision is
/// explicitly about the latter. Spelled as an enum rather than a `bool` so neither call site
/// can be read the wrong way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDisclosure {
    Included,
    Omitted,
}

impl EnrollmentRecord {
    /// §5's `node-info` shape.
    ///
    /// Lives on the record rather than in `dispatch`, because three call sites now render it —
    /// `node-info`, `node-add`'s retained `info` document, and the uplink task's refresh of
    /// that same document — and a retained topic that drifted from what `node-info` prints
    /// would be a difference nobody notices until they disagree in a field report.
    pub fn to_json(&self, key: KeyDisclosure) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("serial_number".into(), self.serial_number.clone().into());
        m.insert("transport".into(), Transport::Lorawan.as_str().into());
        m.insert("profile".into(), self.profile.clone().into());
        // §5's common core carries `pairing_status`, and **nothing local can answer it**:
        // whether a device has joined is a fact about the radio, and this record is only
        // bookkeeping. Null rather than a guess — the uplink task overwrites it with `joined`
        // on the retained document the moment a frame actually arrives.
        m.insert("pairing_status".into(), serde_json::Value::Null);
        m.insert("dev_eui".into(), self.dev_eui.as_str().into());
        if key == KeyDisclosure::Included {
            if let Some(k) = &self.app_key {
                m.insert("app_key".into(), k.clone().into());
            }
        }
        m.insert("activation".into(), self.activation.clone().into());
        if let Some(j) = &self.join_eui {
            m.insert("join_eui".into(), j.clone().into());
        }
        if let Some(n) = &self.name {
            m.insert("name".into(), n.clone().into());
        }
        serde_json::Value::Object(m)
    }
}

/// How a command addressed a node. Both work on both transports; resolution is local, with no
/// ChirpStack call and no radio traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRef {
    Serial(String),
    DevEui(Eui),
}

impl NodeRef {
    /// Resolve to a wire identity.
    ///
    /// The one asymmetric corner §5 names: a LoRaWAN device with **no enrollment record** is
    /// still reachable by `--dev-eui`, addressing ChirpStack directly. A `--serial` miss is
    /// `NotFound` with `component: enrollment-record`, deliberately distinct from a
    /// ChirpStack-side miss.
    pub fn resolve(&self, nodes: &NodesFile) -> Result<Eui> {
        match self {
            NodeRef::DevEui(e) => Ok(e.clone()),
            NodeRef::Serial(s) => nodes
                .key_for(s)
                .and_then(|k| nodes.by_serial.get(&k))
                .map(|r| r.dev_eui.clone())
                .ok_or_else(|| {
                    Error::new(
                        Kind::NotFound,
                        Transport::Lorawan,
                        Component::EnrollmentRecord,
                        format!("no enrollment record maps serial {s:?} to a dev_eui"),
                    )
                }),
        }
    }
}

/// The outcome of an enrollment, for the §5 result envelope.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EnrollOutcome {
    pub serial_number: String,
    pub dev_eui: Eui,
    pub profile: String,
    pub activation: String,
    /// True when the device already existed and only its key material was rotated — the case
    /// v1 got silently wrong.
    pub rotated_keys: bool,
}

pub struct Enroller<'a> {
    pub client: &'a dyn ChirpStack,
    pub store: &'a Store,
    pub state: &'a ConvergedState,
}

pub struct EnrollRequest<'a> {
    pub serial: &'a str,
    pub dev_eui: Eui,
    pub profile: &'a str,
    pub otaa: bool,
    pub join_eui: Option<String>,
    pub name: Option<String>,
    pub material: KeyMaterial,
}

impl Enroller<'_> {
    pub async fn enroll(&self, req: EnrollRequest<'_>) -> Result<EnrollOutcome> {
        // §7.1's gate, first statement — before EUI handling, before any profile lookup, and
        // before any RPC. The error names the scope decision so the operator knows this is a
        // phase boundary, not a broken device.
        if !req.otaa && !ABP_ENROLLMENT_ENABLED {
            return Err(Error::new(
                Kind::InvalidArgument,
                Transport::Lorawan,
                Component::EnrollmentRecord,
                "ABP enrollment is out of scope in phase 1 (decisions.md #8): the ABP device \
                 profile is provisioned and ABP is not deprecated, but enrolling a node with \
                 ABP key material is refused until phase 2",
            ));
        }

        keyfile::check_matches_activation(&req.material, req.otaa)?;

        // A `dev_eui` belongs to exactly one serial. The file is keyed by serial, so nothing
        // structurally prevents two of them naming one EUI — and if that happens, `--dev-eui`
        // addressing becomes ambiguous and removal cannot tell the records apart. Refused
        // locally, before any RPC, like every other shape error here. Re-enrolling the *same*
        // serial is untouched: that is the key rotation §7 requires.
        let mut nodes = NodesFile::load(self.store)?;
        if let Some(held) = nodes
            .by_serial
            .values()
            .find(|r| r.dev_eui == req.dev_eui && !same_serial(&r.serial_number, req.serial))
        {
            return Err(Error::new(
                Kind::AlreadyExists,
                Transport::Lorawan,
                Component::EnrollmentRecord,
                format!(
                    "dev_eui {} is already enrolled as serial {:?}; remove that node first, \
                     or re-run node-add with the same serial to rotate its keys",
                    req.dev_eui, held.serial_number
                ),
            ));
        }
        // The other direction (Nodes test, 2026-09-23): the *same* serial with a *different*
        // dev_eui used to overwrite the record, leaving the first device enrolled in ChirpStack
        // with no record and its retained `info` on the broker. That produced a ghost Node that
        // `node-remove --serial` can no longer reach. Key rotation keeps the dev_eui; moving a
        // serial to another device is a removal and a new enrollment.
        if let Some(held) = nodes
            .by_serial
            .get(req.serial)
            .filter(|r| r.dev_eui != req.dev_eui)
        {
            return Err(Error::new(
                Kind::AlreadyExists,
                Transport::Lorawan,
                Component::EnrollmentRecord,
                format!(
                    "serial {:?} is already enrolled as dev_eui {}; remove that node first \
                     (node-remove --serial {}) to move the serial to {}",
                    req.serial, held.dev_eui, req.serial, req.dev_eui
                ),
            ));
        }

        let activation = if req.otaa {
            Activation::Otaa
        } else {
            Activation::Abp
        };
        let profile_id = self
            .state
            .profile_id(activation, req.profile)
            .ok_or_else(|| {
                Error::new(
                    Kind::NotFound,
                    Transport::Lorawan,
                    Component::ConfigJson,
                    format!(
                        "device profile {:?} has no converged id for the {} arm; run `create` first",
                        req.profile,
                        activation.as_str()
                    ),
                )
            })?
            .to_string();
        let application_id = self.state.application_id()?.to_string();

        // Ask first. A duplicate Create returns an Internal carrying a backend-specific
        // storage message, so "create and interpret the failure" is not implementable.
        let existed = self.client.get_device(&req.dev_eui).await?.is_some();
        if !existed {
            self.client
                .create_device(&DeviceSpec {
                    dev_eui: req.dev_eui.clone(),
                    application_id,
                    device_profile_id: profile_id,
                    name: req.name.clone().unwrap_or_else(|| req.serial.to_string()),
                    description: String::new(),
                    join_eui: req.join_eui.clone(),
                })
                .await?;
        }

        let rotated = match &req.material {
            KeyMaterial::Otaa(keys) => {
                let placed = place_otaa(keys);
                // Same ask-first shape for the keys: this is the exact call v1 made
                // unconditionally, then swallowed the error from.
                if self.client.get_device_keys(&req.dev_eui).await?.is_some() {
                    self.client
                        .update_device_keys(&req.dev_eui, &placed)
                        .await?;
                    true
                } else {
                    self.client
                        .create_device_keys(&req.dev_eui, &placed)
                        .await?;
                    false
                }
            }
            KeyMaterial::Abp(keys) => {
                // Unreachable in phase 1; kept defined and exercised by unit tests so the
                // wire encoding is ready when the gate opens.
                self.client
                    .activate_device(&req.dev_eui, &place_abp(keys))
                    .await?;
                existed
            }
        };

        // Keyed as the registry keys it, so a re-enroll typed in decimal replaces the record
        // rather than adding a second one.
        let key = nodes.key_for(req.serial).unwrap_or_else(|| {
            crate::p2p::session::canonical_serial(req.serial)
                .unwrap_or_else(|| req.serial.to_string())
        });
        nodes.by_serial.insert(
            key,
            EnrollmentRecord {
                serial_number: req.serial.to_string(),
                dev_eui: req.dev_eui.clone(),
                profile: req.profile.to_string(),
                activation: activation.as_str().to_string(),
                join_eui: req.join_eui,
                name: req.name,
                app_key: match &req.material {
                    KeyMaterial::Otaa(k) => Some(hex(&k.app_key)),
                    KeyMaterial::Abp(_) => None,
                },
            },
        );
        nodes.save(self.store)?;

        Ok(EnrollOutcome {
            serial_number: req.serial.to_string(),
            dev_eui: req.dev_eui,
            profile: req.profile.to_string(),
            activation: activation.as_str().to_string(),
            rotated_keys: rotated,
        })
    }

    /// Remove a node: delete it in ChirpStack, then drop the enrollment record.
    ///
    /// Deleting a device that is already gone is success — removal must be re-runnable. The
    /// retained-topic tombstone that must accompany this is the northbound layer's half, which
    /// is why the two steps are also available separately: `dispatch` publishes the tombstone
    /// **between** them, so a broker failure leaves the record in place and the whole command
    /// re-runnable by `--serial` (see [`Enroller::forget`]).
    pub async fn remove(&self, node: &NodeRef) -> Result<Eui> {
        let nodes = NodesFile::load(self.store)?;
        let dev_eui = node.resolve(&nodes)?;
        self.delete_device(&dev_eui).await?;
        self.forget(node, &dev_eui)?;
        Ok(dev_eui)
    }

    /// Delete the device in ChirpStack, leaving the enrollment record alone.
    ///
    /// Tolerating `NotFound` is what makes removal re-runnable at all: the second run of a
    /// half-finished `node-remove` finds nothing to delete and must still reach the steps
    /// after it.
    pub async fn delete_device(&self, dev_eui: &Eui) -> Result<()> {
        match self.client.delete_device(dev_eui).await {
            Ok(()) => {}
            Err(e) if e.kind() == Kind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Drop the local enrollment record — the last step of a removal.
    ///
    /// Last deliberately: it is the only piece of state that lets `--serial` resolve, so
    /// dropping it before the tombstone is published would make a broker failure unrecoverable
    /// by the identifier the operator actually typed.
    pub fn forget(&self, node: &NodeRef, dev_eui: &Eui) -> Result<()> {
        let mut nodes = NodesFile::load(self.store)?;
        // Drop only what was addressed. `--serial` names one record; `--dev-eui` names the
        // device, so every record pointing at it is stale once ChirpStack has deleted it —
        // which after the uniqueness guard in `enroll` is at most one anyway.
        match node {
            NodeRef::Serial(s) => {
                if let Some(k) = nodes.key_for(s) {
                    nodes.by_serial.remove(&k);
                }
            }
            NodeRef::DevEui(_) => nodes.by_serial.retain(|_, r| r.dev_eui != *dev_eui),
        }
        nodes.save(self.store)
    }
}

/// Two spellings of one serial: the same text, or the same STICKER serial in hex and decimal.
fn same_serial(a: &str, b: &str) -> bool {
    use crate::p2p::session::canonical_serial;
    a == b || canonical_serial(a).is_some_and(|c| Some(c) == canonical_serial(b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
