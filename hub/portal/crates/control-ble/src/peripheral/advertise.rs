//! FB00 advertising — `plan/control/ble/peripheral.md` §1.
//!
//! **UUID-only, no manufacturer data.** The Hub advertises the fact that it serves FB00 and nothing
//! else: an advertisement is broadcast to anyone in range, so putting state in it would publish that
//! state to the street. What a phone needs to *recognise* a Hub it reads from FB07 after connecting.

use bluer::adv::Advertisement;
use bluer::Adapter;

use crate::error::{Component, Error, Kind, Result, Role};

use super::gatt::{fb, SVC_FB00};

/// Build the advertisement.
///
/// Pure, so the one thing worth asserting — that it carries exactly one service UUID and no
/// manufacturer data — is testable without a controller.
pub fn advertisement(alias: &str) -> Advertisement {
    Advertisement {
        service_uuids: vec![fb(SVC_FB00)].into_iter().collect(),
        // Deliberately empty: this is the difference between advertising "a ProXimos Hub is here"
        // and advertising its state to the street.
        manufacturer_data: Default::default(),
        discoverable: Some(true),
        local_name: Some(alias.to_string()),
        ..Default::default()
    }
}

/// The advertised name: the configured one uppercased, else the hostname in lowercase — the
/// spelling of the pairing QR's `n` field the app matches against (decision #21).
pub fn alias(configured: Option<&str>, hostname: &str) -> String {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_uppercase(),
        None => hostname.trim().to_lowercase(),
    }
}

/// Start advertising. Stopping is dropping the returned handle.
pub async fn start(adapter: &Adapter, alias: &str) -> Result<bluer::adv::AdvertisementHandle> {
    adapter.set_alias(alias.to_string()).await.map_err(|e| {
        Error::new(
            Kind::Transport,
            Role::Peripheral,
            Component::Advertising,
            format!("cannot set the adapter alias: {e}"),
        )
    })?;
    adapter.advertise(advertisement(alias)).await.map_err(|e| {
        Error::new(
            Kind::Transport,
            Role::Peripheral,
            Component::Advertising,
            format!(
                "cannot start FB00 advertising: {e}. Some controllers refuse an advertisement \
                     the kernel considers malformed even when the parameters are legal — check \
                     `journalctl -u bluetooth` for the MGMT error."
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertisement_is_uuid_only() {
        let adv = advertisement("PROXIMOS-HUB-1");
        assert_eq!(adv.service_uuids.len(), 1);
        assert!(adv.service_uuids.contains(&fb(SVC_FB00)));
        assert!(
            adv.manufacturer_data.is_empty(),
            "an advertisement is broadcast to the street; state belongs behind the gate"
        );
        assert_eq!(adv.discoverable, Some(true));
        assert_eq!(adv.local_name.as_deref(), Some("PROXIMOS-HUB-1"));
    }

    #[test]
    fn the_advertised_service_is_fb00_and_not_a_characteristic_alias() {
        let adv = advertisement("x");
        let only = adv.service_uuids.iter().next().copied().expect("one");
        assert_eq!(only.to_string(), "0000fb00-0000-1000-8000-00805f9b34fb");
    }

    #[test]
    fn the_alias_falls_back_to_the_hostname_in_lowercase() {
        assert_eq!(alias(None, "proximos-hub-1"), "proximos-hub-1");
        assert_eq!(alias(Some(""), "PROXIMOS-HUB-1"), "proximos-hub-1");
        assert_eq!(alias(Some("   "), "proximos-hub-1"), "proximos-hub-1");
        assert_eq!(alias(Some("Freezer Room"), "hub"), "FREEZER ROOM");
        assert_eq!(alias(Some("  Cold  "), "hub"), "COLD");
    }
}
