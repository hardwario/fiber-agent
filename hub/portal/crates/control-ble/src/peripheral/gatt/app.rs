//! Assembling the FB00 application for `bluer`, and connection tracking.
//! `plan/control/ble/peripheral.md` §2, `plan/control/ble/plan.md` §0.1.
//!
//! This module is the only place that touches BlueZ. The closures below extract
//! `device_address`, `mtu` and `offset` from each request and call the pure handlers in the sibling
//! modules — so everything testable is tested there, and what lives here is exactly the part that
//! needs a controller.
//!
//! **Connection tracking is where §0.1's shared adapter matters.** BlueZ reports an inbound phone
//! connection and our own outbound beacon dial as the same transition, so a `Connected` event only
//! counts as a phone when [`AdapterSlot::is_our_dial`] says it is not ours. Without that, a
//! provisioning burst looks like a phone connecting over and over.

use std::sync::Arc;

use bluer::gatt::local::{
    Application, Characteristic, CharacteristicRead, CharacteristicWrite,
    CharacteristicWriteMethod, Service,
};

use crate::mac::MacAddr;

use super::{
    auth, beacon_add, beacon_list, beacon_remove, cluster_join, device_info, export_destination,
    fb, label, lan, node_add, node_list, node_remove, pairing_code, radio_mode, setup, time, wifi,
    GattCtx, CH_AUTH, CH_BEACON_ADD, CH_BEACON_LIST, CH_BEACON_REMOVE, CH_CLUSTER_JOIN,
    CH_DEVICE_INFO, CH_DEVICE_LABEL, CH_EXPORT_DESTINATION, CH_LAN_CONFIG, CH_LAN_STATUS,
    CH_NODE_ADD, CH_NODE_LIST, CH_NODE_REMOVE, CH_PAIRING_CODE, CH_RADIO_MODE, CH_SETUP_COMPLETE,
    CH_SETUP_STATE, CH_TIME_SET, CH_WIFI_CONNECT, CH_WIFI_DISCONNECT, CH_WIFI_SCAN, CH_WIFI_STATUS,
    SVC_FB00,
};

/// The ATT default, used when BlueZ reports no MTU on a request.
const FALLBACK_MTU: u16 = 23;

/// Should a connection transition for `addr` be treated as a phone?
///
/// Pure, so §0.1's rule is testable without a controller: it is a phone unless it is a dial we
/// started — including a raw-ATT dial, which BlueZ cannot see at all.
pub fn is_client_connection(slot: &crate::central::adapter::AdapterSlot, addr: &MacAddr) -> bool {
    !slot.is_our_dial(addr)
}

macro_rules! read_only {
    ($ctx:expr, $alias:expr, $handler:path) => {{
        let ctx = $ctx.clone();
        Characteristic {
            uuid: fb($alias),
            read: Some(CharacteristicRead {
                read: true,
                fun: Box::new(move |req| {
                    let ctx = ctx.clone();
                    Box::pin(async move {
                        $handler(
                            &ctx,
                            req.device_address.0,
                            req.mtu.max(FALLBACK_MTU),
                            req.offset,
                        )
                    })
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }};
}

macro_rules! write_only {
    ($ctx:expr, $alias:expr, $handler:path) => {{
        let ctx = $ctx.clone();
        Characteristic {
            uuid: fb($alias),
            write: Some(CharacteristicWrite {
                write: true,
                write_without_response: false,
                method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                    let ctx = ctx.clone();
                    Box::pin(async move { $handler(&ctx, req.device_address.0, &value) })
                })),
                ..Default::default()
            }),
            ..Default::default()
        }
    }};
}

macro_rules! write_then_read {
    ($ctx:expr, $alias:expr, $w:path, $r:path) => {{
        let wctx = $ctx.clone();
        let rctx = $ctx.clone();
        Characteristic {
            uuid: fb($alias),
            write: Some(CharacteristicWrite {
                write: true,
                write_without_response: false,
                method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                    let ctx = wctx.clone();
                    Box::pin(async move { $w(&ctx, req.device_address.0, &value) })
                })),
                ..Default::default()
            }),
            read: Some(CharacteristicRead {
                read: true,
                fun: Box::new(move |req| {
                    let ctx = rctx.clone();
                    Box::pin(async move {
                        $r(
                            &ctx,
                            req.device_address.0,
                            req.mtu.max(FALLBACK_MTU),
                            req.offset,
                        )
                    })
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }};
}

/// Build the FB00 service.
///
/// FB05/FB06 are absent because CP-1 deleted the BLE shell; FB13–FB15 are absent because they are
/// phase 2 reservations. Both absences are asserted in `mod.rs`'s tests.
pub fn fb00_service(ctx: Arc<GattCtx>) -> Service {
    let mut characteristics = vec![
        // FB01 is the gate, so it is the one write that is not itself gated.
        write_then_read!(ctx, CH_AUTH, auth::write, auth::read),
        read_only!(ctx, CH_WIFI_SCAN, wifi::scan_read),
        write_only!(ctx, CH_WIFI_CONNECT, wifi::connect_write),
        read_only!(ctx, CH_WIFI_STATUS, wifi::status_read),
        write_only!(ctx, CH_WIFI_DISCONNECT, wifi::disconnect_write),
        write_only!(ctx, CH_LAN_CONFIG, lan::config_write),
        read_only!(ctx, CH_LAN_STATUS, lan::status_read),
        write_then_read!(ctx, CH_DEVICE_LABEL, label::write, label::read),
        write_then_read!(ctx, CH_TIME_SET, time::write, time::read),
        write_then_read!(ctx, CH_NODE_ADD, node_add::write, node_add::read),
        write_then_read!(ctx, CH_BEACON_ADD, beacon_add::write, beacon_add::read),
        write_then_read!(
            ctx,
            CH_BEACON_REMOVE,
            beacon_remove::write,
            beacon_remove::read
        ),
        write_then_read!(ctx, CH_NODE_REMOVE, node_remove::write, node_remove::read),
        // FB16 is a *gated* read, unlike FB07 beside it: it carries the Hub's state, and state is
        // what FB07 must not have (§3, §6.3).
        read_only!(ctx, CH_SETUP_STATE, setup::read),
        write_then_read!(ctx, CH_SETUP_COMPLETE, setup::write, setup::read_result),
        write_then_read!(
            ctx,
            CH_EXPORT_DESTINATION,
            export_destination::write,
            export_destination::read
        ),
        write_then_read!(
            ctx,
            CH_PAIRING_CODE,
            pairing_code::write,
            pairing_code::read
        ),
        // FB1B reads the state and, after a write, the write's result in front of it.
        write_then_read!(ctx, CH_RADIO_MODE, radio_mode::write, radio_mode::read),
        // FB1C reads the state and, after a write, the join's progress or result in front of it.
        write_then_read!(
            ctx,
            CH_CLUSTER_JOIN,
            cluster_join::write,
            cluster_join::read
        ),
    ];

    // FB07 is deliberately **un-gated**: the phone reads it to recognise a Hub before it has any
    // credential. It therefore takes no peer, which is why it is spelled out rather than macro'd —
    // the difference is the point, not an inconvenience.
    let dctx = ctx.clone();
    characteristics.push(Characteristic {
        uuid: fb(CH_DEVICE_INFO),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new(move |req| {
                let ctx = dctx.clone();
                Box::pin(
                    async move { device_info::read(&ctx, req.mtu.max(FALLBACK_MTU), req.offset) },
                )
            }),
            ..Default::default()
        }),
        ..Default::default()
    });

    // The two paged characteristics build their page **at write time**, so their writes need the
    // MTU as well as the address — which is one argument more than `write_then_read!` passes.
    macro_rules! paged {
        ($alias:expr, $w:path, $r:path) => {{
            let wctx = ctx.clone();
            let rctx = ctx.clone();
            characteristics.push(Characteristic {
                uuid: fb($alias),
                write: Some(CharacteristicWrite {
                    write: true,
                    write_without_response: false,
                    method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                        let ctx = wctx.clone();
                        Box::pin(async move {
                            $w(
                                &ctx,
                                req.device_address.0,
                                req.mtu.max(FALLBACK_MTU),
                                &value,
                            )
                        })
                    })),
                    ..Default::default()
                }),
                read: Some(CharacteristicRead {
                    read: true,
                    fun: Box::new(move |req| {
                        let ctx = rctx.clone();
                        Box::pin(async move {
                            $r(
                                &ctx,
                                req.device_address.0,
                                req.mtu.max(FALLBACK_MTU),
                                req.offset,
                            )
                        })
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }};
    }
    paged!(CH_BEACON_LIST, beacon_list::write, beacon_list::read);
    paged!(CH_NODE_LIST, node_list::write, node_list::read);

    Service {
        uuid: fb(SVC_FB00),
        primary: true,
        characteristics,
        ..Default::default()
    }
}

/// The whole GATT application: one service.
pub fn application(ctx: Arc<GattCtx>) -> Application {
    Application {
        services: vec![fb00_service(ctx)],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::adapter::AdapterSlot;

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).expect("mac")
    }

    #[test]
    fn our_own_dial_is_not_counted_as_a_phone_connecting() {
        let slot = AdapterSlot::new();
        let beacon = mac("AA:BB:CC:DD:EE:FF");
        let phone = mac("11:22:33:44:55:66");

        assert!(
            is_client_connection(&slot, &beacon),
            "with nothing claimed, any connection is inbound"
        );

        let _claim = slot.claim(beacon).expect("dial");
        assert!(
            !is_client_connection(&slot, &beacon),
            "§0.1: a provisioning burst must not look like a phone connecting over and over"
        );
        assert!(
            is_client_connection(&slot, &phone),
            "and a real phone is still recognised during a dial"
        );
    }

    #[test]
    fn a_raw_att_dial_is_also_not_a_phone() {
        // Raw dials are invisible to BlueZ, so only the slot can tell.
        let slot = AdapterSlot::new();
        let beacon = mac("AA:BB:CC:DD:EE:FF");
        let _claim = slot.claim_raw(beacon).expect("dial");
        assert!(!is_client_connection(&slot, &beacon));
    }

    /// A `GattCtx` is all `fb00_service` needs; nothing here reaches a controller.
    fn ctx() -> Arc<GattCtx> {
        Arc::new(GattCtx::new(
            crate::host::Host::in_memory(),
            Arc::new(crate::peripheral::session::SessionSlot::new()),
            Arc::new(AdapterSlot::new()),
            Arc::new(std::sync::RwLock::new(
                crate::central::state::CentralState::default(),
            )),
            crate::config::PeripheralConfig::default(),
            mac("B8:27:EB:11:22:33"),
            crate::peripheral::events::EventBus::new(),
        ))
    }

    /// What is actually *served*, as opposed to what [`super::REGISTERED`] claims.
    ///
    /// These two can disagree, and the disagreement is invisible to every other test in this crate:
    /// the handlers are pure functions tested directly, `REGISTERED` is a constant asserted against
    /// itself, and this function — the one place the two are joined — needs a controller to observe
    /// in production. It does not need one to observe here, because a `Service` is a plain struct.
    fn served(svc: &Service) -> Vec<u16> {
        svc.characteristics
            .iter()
            .map(|c| {
                // Recover the 16-bit alias from the Bluetooth base UUID.
                (c.uuid.as_u128() >> 96) as u16
            })
            .collect()
    }

    #[test]
    fn the_service_serves_exactly_the_registered_set() {
        let svc = fb00_service(ctx());
        assert_eq!(svc.uuid, fb(SVC_FB00));
        assert!(svc.primary);

        let mut got = served(&svc);
        got.sort_unstable();
        let mut want = super::super::REGISTERED.to_vec();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "the served set and REGISTERED must be the same list — a constant that agrees with              nothing is not an allocation table"
        );

        // Nothing is registered twice: a duplicate UUID is a BlueZ registration failure at runtime,
        // which is the worst place to find it.
        let mut deduped = got.clone();
        deduped.dedup();
        assert_eq!(deduped.len(), got.len());

        // And the two absent sets are absent from what is served, not merely from the constant.
        for absent in super::super::DELETED
            .iter()
            .chain(super::super::RESERVED.iter())
            .chain(super::super::RETIRED.iter())
        {
            assert!(
                !got.contains(absent),
                "{absent:#06X} must not be served by this build"
            );
        }
    }

    #[test]
    fn every_characteristic_is_readable_or_writable_the_way_its_contract_says() {
        // The defect this catches: a registration that compiles and reports the right UUID while
        // serving a write-only characteristic the app is supposed to read back from. FB12's result
        // and FB11's pages are both read *after* a write, so a missing `read` would strand them.
        let svc = fb00_service(ctx());
        let find = |alias: u16| {
            svc.characteristics
                .iter()
                .find(|c| c.uuid == fb(alias))
                .unwrap_or_else(|| panic!("{alias:#06X} is not served"))
        };

        for alias in [
            CH_AUTH,
            CH_DEVICE_LABEL,
            CH_TIME_SET,
            CH_NODE_ADD,
            CH_BEACON_ADD,
            CH_BEACON_REMOVE,
            CH_BEACON_LIST,
            CH_NODE_LIST,
            CH_NODE_REMOVE,
            CH_SETUP_COMPLETE,
            CH_EXPORT_DESTINATION,
            CH_PAIRING_CODE,
            CH_RADIO_MODE,
            CH_CLUSTER_JOIN,
        ] {
            let c = find(alias);
            assert!(c.write.is_some(), "{alias:#06X} must accept a write");
            assert!(
                c.read.is_some(),
                "{alias:#06X} is write-then-read: without a read the answer is unreachable"
            );
        }

        // Read-only, and deliberately so: a writable FB04 or FB0C would be a state change dressed
        // as a status query.
        for alias in [
            CH_WIFI_SCAN,
            CH_WIFI_STATUS,
            CH_LAN_STATUS,
            CH_DEVICE_INFO,
            CH_SETUP_STATE,
        ] {
            let c = find(alias);
            assert!(c.read.is_some(), "{alias:#06X} must be readable");
            assert!(c.write.is_none(), "{alias:#06X} must not be writable");
        }

        // Write-only: these act, they do not report. A read would be an empty promise.
        for alias in [CH_WIFI_CONNECT, CH_WIFI_DISCONNECT, CH_LAN_CONFIG] {
            let c = find(alias);
            assert!(c.write.is_some(), "{alias:#06X} must accept a write");
            assert!(c.read.is_none(), "{alias:#06X} must not be readable");
        }
    }

    #[test]
    fn no_characteristic_accepts_a_write_without_a_response() {
        // Write-without-response has no way to report a refusal, and every write on this surface can
        // be refused — by the session gate if nothing else. v1's FB0E accepted both.
        for c in fb00_service(ctx()).characteristics {
            if let Some(w) = c.write.as_ref() {
                assert!(
                    !w.write_without_response,
                    "{:?} must not accept an unacknowledged write",
                    c.uuid
                );
                assert!(w.write, "a write characteristic must advertise write");
            }
        }
    }
}
