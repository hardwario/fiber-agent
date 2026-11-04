//! The paging contract, swept as properties — `plan/control/ble/peripheral.md` §4, §5.1.
//!
//! This contract is **declared reusable** by FB11/FB12 and FB13–FB15, so it is worth more than
//! example-based tests: the sweep below crosses every MTU the field produces with the name lengths
//! and list sizes that broke the earlier design's promises.
//!
//! `walk()` is written the way the app walks — read pages until one comes back empty — so a
//! duplicate or a dropped item shows up as a wrong *sequence*, not merely a wrong count.

use control_ble::config::EyeTag;
use control_ble::peripheral::paging::{
    build_page, page_budget, BeaconList, ListRead, NodeItem, NodeList, PageItem, PagedList,
    DEFAULT_MAX_PAGE_BYTES, ELLIPSIS, ERR_MTU_TOO_SMALL,
};

const MTUS: [u16; 4] = [23, 185, 247, 512];
const SIZES: [usize; 8] = [0, 1, 2, 3, 12, 30, 50, 120];

fn names() -> Vec<Option<String>> {
    vec![
        None,
        Some(String::new()),
        Some("F".to_string()),
        Some("Freezer aisle 3".to_string()),
        Some("Cold room 12 north".to_string()),
        // 32 quote characters: each one costs an escape on the wire, which is what makes a name
        // unexpectedly expensive.
        Some("\"".repeat(32)),
    ]
}

fn tag(i: usize, name: &Option<String>) -> EyeTag {
    EyeTag {
        // Spread across the whole address space so the sort is doing real work.
        mac: format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            (i * 7) % 256,
            (i * 13) % 256,
            (i * 29) % 256,
            (i * 31) % 256,
            (i / 256) % 256,
            i % 256
        ),
        name: name.clone(),
        enabled: true,
        logging_interval_min: None,
        recording: None,
        field_thresholds: Vec::new(),
        provisioned: None,
    }
}

fn list_of(size: usize, name: &Option<String>) -> BeaconList {
    let tags: Vec<EyeTag> = (0..size).map(|i| tag(i, name)).collect();
    BeaconList::from_tags(&tags)
}

/// Walk as the app does: pages until `count == 0`. Returns the sequence of items served, or the
/// error key if the very first read refused.
///
/// Generic over the item type since §4's contract is: the same walk has to hold for FB11's Node
/// list as for FB0F's Beacon list, and a sweep written against only one of them would not prove it.
fn walk<T: PageItem>(list: &PagedList<T>, mtu: u16) -> Result<Vec<T>, String> {
    let mut seen = Vec::new();
    for page in 0..10_000u16 {
        match build_page(list, page, mtu, DEFAULT_MAX_PAGE_BYTES) {
            ListRead::Error(e) => {
                if page == 0 {
                    return Err(e.error);
                }
                panic!("page {page} refused mid-walk: {e:?}");
            }
            ListRead::Page(p) => {
                assert_eq!(p.page, page, "a page must report its own index");
                assert_eq!(p.count as usize, p.items.len(), "count must match items");
                if p.count == 0 {
                    return Ok(seen);
                }
                seen.extend(p.items);
            }
        }
    }
    panic!("pagination did not terminate");
}

#[test]
fn every_beacon_is_served_exactly_once_in_mac_order() {
    for mtu in MTUS {
        for name in names() {
            for size in SIZES {
                let list = list_of(size, &name);
                let Ok(seen) = walk(&list, mtu) else {
                    // MTU 23 refuses; that is its own test below.
                    continue;
                };
                let expected_macs: Vec<&str> = list.items.iter().map(|i| i.mac.as_str()).collect();
                let seen_macs: Vec<&str> = seen.iter().map(|i| i.mac.as_str()).collect();
                assert_eq!(
                    seen_macs, expected_macs,
                    "mtu {mtu}, size {size}, name {name:?}: the walked SEQUENCE must be the sorted \
                     list — comparing counts would miss a duplicate that replaced a missing item"
                );
            }
        }
    }
}

#[test]
fn no_page_ever_exceeds_the_budget_its_mtu_allows() {
    for mtu in MTUS {
        let budget = page_budget(mtu, DEFAULT_MAX_PAGE_BYTES);
        for name in names() {
            for size in SIZES {
                let list = list_of(size, &name);
                for page in 0..40u16 {
                    let read = build_page(&list, page, mtu, DEFAULT_MAX_PAGE_BYTES);
                    let bytes = read.to_bytes();
                    if read.is_error() {
                        continue;
                    }
                    assert!(
                        bytes.len() <= budget,
                        "mtu {mtu} page {page} size {size} name {name:?}: {} bytes exceeds the \
                         {budget}-byte budget",
                        bytes.len()
                    );
                }
            }
        }
    }
}

#[test]
fn a_page_is_identical_however_many_times_it_is_asked_for() {
    for mtu in MTUS {
        for name in names() {
            for size in SIZES {
                let list = list_of(size, &name);
                for page in 0..8u16 {
                    let a = build_page(&list, page, mtu, DEFAULT_MAX_PAGE_BYTES);
                    let b = build_page(&list, page, mtu, DEFAULT_MAX_PAGE_BYTES);
                    assert_eq!(
                        a.to_bytes(),
                        b.to_bytes(),
                        "mtu {mtu} page {page}: a read retry must be byte-identical"
                    );
                }
            }
        }
    }
}

#[test]
fn pagination_terminates_and_the_total_is_honest() {
    for mtu in [185u16, 247, 512] {
        for name in names() {
            for size in SIZES {
                let list = list_of(size, &name);
                let seen = walk(&list, mtu).expect("a negotiated link serves pages");
                assert_eq!(seen.len(), size.min(list.len()));
                match build_page(&list, 0, mtu, DEFAULT_MAX_PAGE_BYTES) {
                    ListRead::Page(p) => assert_eq!(
                        p.total as usize,
                        list.len(),
                        "total is the full enrolled count, so the app can show progress"
                    ),
                    ListRead::Error(e) => panic!("{e:?}"),
                }
            }
        }
    }
}

#[test]
fn a_link_too_small_for_one_item_refuses_instead_of_truncating() {
    // §5.1: at MTU 23 no JSON message is short enough to help, so the refusal must be an ATT-level
    // error rather than a payload. `build_page` reports that intent as `mtu_too_small`, and the GATT
    // layer turns it into an ATT error. The one thing it must never do is emit a fragment.
    for name in names() {
        for size in SIZES {
            let list = list_of(size, &name);
            let read = build_page(&list, 0, 23, DEFAULT_MAX_PAGE_BYTES);
            match read {
                ListRead::Error(e) => assert_eq!(e.error, ERR_MTU_TOO_SMALL, "size {size}"),
                ListRead::Page(p) => panic!(
                    "MTU 23 must refuse even the empty list, got a page of {} items",
                    p.items.len()
                ),
            }
        }
    }
    // And whatever it returns is always a complete JSON document, never a fragment.
    let list = list_of(3, &Some("Freezer 1".to_string()));
    let bytes = build_page(&list, 0, 23, DEFAULT_MAX_PAGE_BYTES).to_bytes();
    serde_json::from_slice::<serde_json::Value>(&bytes).expect("a complete document");
}

#[test]
fn the_address_is_never_shortened_even_when_the_name_must_be() {
    // 20 items each named with 64 quote characters: every name has to be cut, and every MAC must
    // survive at its full 17 characters.
    let long = "\"".repeat(64);
    let tags: Vec<EyeTag> = (0..20).map(|i| tag(i, &Some(long.clone()))).collect();
    let list = BeaconList::from_tags(&tags);
    let seen = walk(&list, 185).expect("serves");
    assert_eq!(seen.len(), 20);
    for item in &seen {
        assert_eq!(
            item.mac.chars().count(),
            17,
            "a half MAC addresses the wrong device: {:?}",
            item.mac
        );
        assert!(item.mac.matches(':').count() == 5);
    }
}

#[test]
fn rev_is_eight_hex_chars_and_changes_only_on_content() {
    let one = BeaconList::from_tags(&[tag(0, &Some("Freezer 1".to_string()))]);
    assert_eq!(one.rev.len(), 8);
    assert!(one.rev.chars().all(|c| c.is_ascii_hexdigit()));

    // A rename that changes nothing → same rev.
    let same = BeaconList::from_tags(&[tag(0, &Some("Freezer 1".to_string()))]);
    assert_eq!(one.rev, same.rev);

    // Adding a beacon → different rev, which is what restarts a stale walk.
    let two = BeaconList::from_tags(&[
        tag(0, &Some("Freezer 1".to_string())),
        tag(1, &Some("Freezer 2".to_string())),
    ]);
    assert_ne!(one.rev, two.rev);

    // The same list "after a restart" → same rev. A counter or a randomly-seeded hasher would fail
    // here, and every phone's walk would be invalidated on every reboot.
    let after_restart = BeaconList::from_tags(&[
        tag(0, &Some("Freezer 1".to_string())),
        tag(1, &Some("Freezer 2".to_string())),
    ]);
    assert_eq!(two.rev, after_restart.rev);

    // And every page of a walk carries the same rev, so the app can detect a change mid-walk.
    let list = list_of(30, &Some("Freezer aisle 3".to_string()));
    for page in 0..6u16 {
        if let ListRead::Page(p) = build_page(&list, page, 185, DEFAULT_MAX_PAGE_BYTES) {
            assert_eq!(p.rev, list.rev);
        }
    }
}

#[test]
fn a_page_past_the_end_is_empty_rather_than_an_error() {
    let list = list_of(3, &Some("F".to_string()));
    match build_page(&list, 500, 185, DEFAULT_MAX_PAGE_BYTES) {
        ListRead::Page(p) => {
            assert_eq!(p.count, 0);
            assert!(p.items.is_empty());
            assert_eq!(p.total, 3, "total is still honest past the end");
        }
        ListRead::Error(e) => panic!("past the end is how a walk terminates, not an error: {e:?}"),
    }
}

#[test]
fn at_least_one_item_per_page_always() {
    // The guarantee peripheral.md §4 actually makes, across the whole sweep.
    for mtu in [185u16, 247, 512] {
        for name in names() {
            for size in SIZES.iter().copied().filter(|s| *s > 0) {
                let list = list_of(size, &name);
                match build_page(&list, 0, mtu, DEFAULT_MAX_PAGE_BYTES) {
                    ListRead::Page(p) => assert!(
                        p.count >= 1,
                        "mtu {mtu} size {size} name {name:?} produced an empty first page"
                    ),
                    ListRead::Error(e) => panic!("mtu {mtu}: {e:?}"),
                }
            }
        }
    }
}

#[test]
fn a_bigger_mtu_never_serves_fewer_items_on_the_first_page() {
    // Monotonicity is not required by the contract, but a violation would mean the fixed-width
    // measurement had gone wrong somewhere.
    for name in names() {
        let list = list_of(50, &name);
        let mut last = 0u32;
        for mtu in [185u16, 247, 512] {
            if let ListRead::Page(p) = build_page(&list, 0, mtu, DEFAULT_MAX_PAGE_BYTES) {
                assert!(
                    p.count >= last,
                    "mtu {mtu} name {name:?}: {} < {last}",
                    p.count
                );
                last = p.count;
            }
        }
    }
}

/// The rev of a fixed list, written out in full.
///
/// Every other assertion here is relational — "the same content hashes the same", "adding an item
/// changes it" — and a refactor that changed the hash *consistently* would satisfy all of them
/// while invalidating the walk of every phone already in the field. This one is absolute: it is the
/// literal value the deployed contract produces, and it exists so the generalization of this module
/// over its item type cannot move it.
#[test]
fn beacon_rev_is_pinned() {
    let list = BeaconList::from_tags(&[
        EyeTag {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: Some("Freezer 1".to_string()),
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: Vec::new(),
            provisioned: None,
        },
        EyeTag {
            mac: "00:11:22:33:44:55".to_string(),
            name: None,
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: Vec::new(),
            provisioned: None,
        },
    ]);
    assert_eq!(
        list.rev, "cea35fc5",
        "the deployed rev of this list must not move"
    );
}

// -------------------------------------------------------------------------------------------
// The same contract, swept over FB11's item type
// -------------------------------------------------------------------------------------------
//
// §4 declares the paging scheme reusable. These are the sweeps that make "reusable" a property
// rather than a claim: the same walk, the same budget rule, the same never-truncate-the-key
// guarantee, over `NodeItem` instead of `ListItem`.

fn node_item(i: usize, serial: &Option<String>, name: &Option<String>) -> NodeItem {
    NodeItem {
        // Spread across the whole 64-bit space so the sort is doing real work, and never
        // ambiguous — a DevEUI is 16 hex characters, always.
        deveui: format!("{:016x}", (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        serial: serial.clone(),
        name: name.clone(),
    }
}

fn serials() -> Vec<Option<String>> {
    vec![
        None,
        Some("SN-1".to_string()),
        Some("HW-STICKER-000000000000042".to_string()),
    ]
}

fn node_list_of(size: usize, serial: &Option<String>, name: &Option<String>) -> NodeList {
    NodeList::from_items((0..size).map(|i| node_item(i, serial, name)).collect())
}

#[test]
fn every_node_is_served_exactly_once_in_deveui_order() {
    for mtu in MTUS {
        for serial in serials() {
            for name in names() {
                for size in SIZES {
                    let list = node_list_of(size, &serial, &name);
                    let Ok(seen) = walk(&list, mtu) else {
                        // MTU 23 refuses; that is the beacon sweep's own test and holds here too.
                        continue;
                    };
                    let expected: Vec<&str> =
                        list.items.iter().map(|i| i.deveui.as_str()).collect();
                    let got: Vec<&str> = seen.iter().map(|i| i.deveui.as_str()).collect();
                    assert_eq!(
                        got, expected,
                        "mtu {mtu}, size {size}, serial {serial:?}, name {name:?}: the walked \
                         SEQUENCE must be the sorted list"
                    );
                }
            }
        }
    }
}

#[test]
fn no_node_page_ever_exceeds_the_budget_its_mtu_allows() {
    for mtu in MTUS {
        let budget = page_budget(mtu, DEFAULT_MAX_PAGE_BYTES);
        for serial in serials() {
            for name in names() {
                for size in SIZES {
                    let list = node_list_of(size, &serial, &name);
                    for page in 0..40u16 {
                        let read = build_page(&list, page, mtu, DEFAULT_MAX_PAGE_BYTES);
                        if read.is_error() {
                            continue;
                        }
                        let bytes = read.to_bytes();
                        assert!(
                            bytes.len() <= budget,
                            "mtu {mtu} page {page} size {size}: {} bytes exceeds the \
                             {budget}-byte budget",
                            bytes.len()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_deveui_is_never_shortened_and_the_serial_outlives_the_name() {
    // 20 Nodes each named with 64 quote characters — every name has to be cut. The DevEUI must
    // survive at its full 16 characters on every one of them, and a shortened name is marked.
    let long = "\"".repeat(64);
    let list = node_list_of(20, &Some("SN-42".to_string()), &Some(long.clone()));
    let seen = walk(&list, 185).expect("serves");
    assert_eq!(seen.len(), 20);
    for item in &seen {
        assert_eq!(
            item.deveui.chars().count(),
            16,
            "a half DevEUI addresses the wrong device: {:?}",
            item.deveui
        );
        assert!(item.deveui.chars().all(|c| c.is_ascii_hexdigit()));
        // At a tighter budget the name may be gone entirely; the DevEUI still must not be.
        if let Some(n) = item.name.as_deref() {
            assert!(n.ends_with(ELLIPSIS), "a shortened name is marked: {n:?}");
            assert!(n.chars().count() < long.chars().count());
            assert_eq!(
                item.serial.as_deref(),
                Some("SN-42"),
                "the label goes before the number on the sticker does"
            );
        }
    }
}

#[test]
fn a_node_page_is_identical_however_many_times_it_is_asked_for() {
    for mtu in [185u16, 247, 512] {
        for serial in serials() {
            for name in names() {
                for size in SIZES {
                    let list = node_list_of(size, &serial, &name);
                    for page in 0..8u16 {
                        assert_eq!(
                            build_page(&list, page, mtu, DEFAULT_MAX_PAGE_BYTES).to_bytes(),
                            build_page(&list, page, mtu, DEFAULT_MAX_PAGE_BYTES).to_bytes(),
                            "mtu {mtu} page {page}: a read retry must be byte-identical"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn at_least_one_node_per_page_always() {
    for mtu in [185u16, 247, 512] {
        for serial in serials() {
            for name in names() {
                for size in SIZES.iter().copied().filter(|s| *s > 0) {
                    let list = node_list_of(size, &serial, &name);
                    match build_page(&list, 0, mtu, DEFAULT_MAX_PAGE_BYTES) {
                        ListRead::Page(p) => assert!(
                            p.count >= 1,
                            "mtu {mtu} size {size} produced an empty first page"
                        ),
                        ListRead::Error(e) => panic!("mtu {mtu}: {e:?}"),
                    }
                }
            }
        }
    }
}

#[test]
fn the_node_rev_is_a_content_hash_of_every_field_the_phone_is_shown() {
    let one = NodeList::from_items(vec![NodeItem {
        deveui: "0011223344556677".to_string(),
        serial: Some("SN-1".to_string()),
        name: Some("Fridge 1".to_string()),
    }]);
    assert_eq!(one.rev.len(), 8);
    assert!(one.rev.chars().all(|c| c.is_ascii_hexdigit()));

    // The same content again — as after a restart — hashes the same. A counter or a randomly
    // seeded hasher would invalidate every phone's walk on every reboot.
    let same = NodeList::from_items(one.items.clone());
    assert_eq!(one.rev, same.rev);

    // Each of the three fields is part of the content, so a change to any of them restarts a
    // stale walk rather than being served under a rev the phone already trusts.
    for mutate in [
        |i: &mut NodeItem| i.deveui = "8899aabbccddeeff".to_string(),
        |i: &mut NodeItem| i.serial = Some("SN-2".to_string()),
        |i: &mut NodeItem| i.name = Some("Fridge 2".to_string()),
        |i: &mut NodeItem| i.name = None,
    ] {
        let mut item = one.items[0].clone();
        mutate(&mut item);
        assert_ne!(
            one.rev,
            NodeList::from_items(vec![item]).rev,
            "a change the phone can see must move the rev"
        );
    }

    // And every page of a walk carries the same rev, so the app can detect a change mid-walk.
    let list = node_list_of(30, &Some("SN-x".to_string()), &Some("Freezer".to_string()));
    for page in 0..6u16 {
        if let ListRead::Page(p) = build_page(&list, page, 185, DEFAULT_MAX_PAGE_BYTES) {
            assert_eq!(p.rev, list.rev);
        }
    }
}

#[test]
fn a_beacon_rev_and_a_node_rev_of_the_same_size_are_not_confusable_by_construction() {
    // Not a correctness requirement — the two lists are served on different characteristics — but
    // a shared `rev` for lists that are not the same list would be a confusing coincidence to
    // debug, and the per-item line format is what keeps them apart.
    let beacons = BeaconList::from_tags(&[tag(0, &Some("x".to_string()))]);
    let nodes = NodeList::from_items(vec![NodeItem {
        deveui: beacons.items[0].mac.clone(),
        serial: None,
        name: Some("x".to_string()),
    }]);
    assert_ne!(beacons.rev, nodes.rev);
}
