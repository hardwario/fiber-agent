//! The paging contract — `plan/control/ble/peripheral.md` §4, §5.1.
//!
//! One scheme, **declared reusable** by FB11/FB12 and FB13–FB15 rather than redesigned per
//! characteristic. Seven rules, all of them here:
//!
//! 1. the response buffer is per requesting central (see [`super::buffers`]);
//! 2. a read with no prior write returns a defined error, never a default first page;
//! 3. reads are idempotent — the cursor advances only on write;
//! 4. buffers are cleared on disconnect;
//! 5. **`rev` is a content hash of the sorted list, not a counter** — so it survives a restart and
//!    does *not* change on a no-op rename, which is what closes the
//!    beacon-added-mid-pagination race without invalidating every walk;
//! 6. items are sorted by their key, so page N is stable across reads;
//! 7. the page budget is `min(mtu - 1, max_page_bytes)` and **never relies on long reads**, whose
//!    behaviour is uneven across platforms.
//!
//! **The size guarantee, stated correctly.** The earlier Beacon design promised "≥ 2 items per page,
//! 3–4 typical" and "a 50-beacon site pages in ≤ 25 round trips". Both were wrong: the envelope is
//! far larger than that estimate assumed. The guarantee that is actually true is
//!
//! > **at least one item per page, always** — the name is truncated with an ellipsis if it must be,
//! > and the **key** — the MAC on FB0F, the DevEUI on FB11 — is **never** truncated.
//!
//! Correctness was never at risk; the promise to the app team was. So the page index is measured at
//! a **fixed width** rather than made variable: a variable index reintroduces a duplicate-items
//! defect, because page N's boundary would then depend on how many digits N has.
//!
//! **Declared reusable, then actually reused.** FB11 adopts this scheme rather than getting its own,
//! which is what [`PageItem`] is for: the rules above are written once against a key, an optional
//! shortenable name, and a reduction ladder, and each characteristic supplies those three. The
//! byte-level behaviour of the Beacon list is unchanged by that generalization — `beacon_rev_is_pinned`
//! in `tests/paging_properties.rs` holds the literal `rev` of a fixed list so a phone already walking
//! the field's Hubs cannot be invalidated by it.

use crate::config::EyeTag;

pub const DEFAULT_MAX_PAGE_BYTES: usize = 180;
/// A floor for "could one item ever fit here". A key-only item is about 27 bytes on the wire.
pub const MIN_ITEM_BYTES: usize = 30;

/// The closed set of error keys.
pub const ERR_NO_REQUEST: &str = "no_request";
pub const ERR_BAD_REQUEST: &str = "bad_request";
pub const ERR_MTU_TOO_SMALL: &str = "mtu_too_small";
pub const ERR_INTERNAL: &str = "internal";

/// What a name is shortened with when it cannot fit.
pub const ELLIPSIS: char = '…';

/// What the paging contract needs to know about an item, and nothing more.
///
/// Three things, in the order the rules use them: the **key** it is sorted, deduped and addressed by
/// and which is never shortened; the **name**, which is the one field a tight budget is allowed to
/// cut; and a **reduction ladder** for the pathological link where even the named item will not fit.
///
/// Keeping it this small is deliberate. A trait that also knew about MTUs or JSON would have to be
/// re-decided per characteristic, which is precisely the "redesigned per characteristic" outcome §4
/// exists to prevent.
pub trait PageItem:
    Clone + PartialEq + Eq + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned
{
    /// The sort and dedup key — a MAC on FB0F, a DevEUI on FB11. **Never truncated**: a half key
    /// addresses the wrong device.
    fn key(&self) -> &str;

    /// The shortenable label, when the item carries one.
    fn name(&self) -> Option<&str>;

    /// The same item with its name replaced — used to serve a shortened one, and with `None` to drop
    /// the name entirely while keeping every other field.
    fn with_name(&self, name: Option<String>) -> Self;

    /// The smallest form that still addresses the device: the key, and nothing else.
    fn minimal(&self) -> Self;

    /// This item's contribution to the `rev` hash, appended verbatim.
    ///
    /// It is a **line format rather than the serialized JSON** on purpose: the hash must not move
    /// when a serde attribute changes how the same content is spelled on the wire.
    fn write_rev_line(&self, buf: &mut Vec<u8>);
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListItem {
    pub mac: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl PageItem for ListItem {
    fn key(&self) -> &str {
        &self.mac
    }
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    fn with_name(&self, name: Option<String>) -> ListItem {
        ListItem {
            mac: self.mac.clone(),
            name,
        }
    }
    fn minimal(&self) -> ListItem {
        ListItem {
            mac: self.mac.clone(),
            name: None,
        }
    }
    fn write_rev_line(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.mac.as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(self.name.as_deref().unwrap_or("").as_bytes());
        buf.push(b'\n');
    }
}

/// FB11's item — a LoRaWAN Node as the phone's list shows it.
///
/// The **DevEUI is the key** because it is the LoRaWAN wire identity and the one field that is
/// guaranteed present and unique; the serial is the operator-facing identity
/// (`plan/control/radio/plan.md` §5 "One identifier") and is carried alongside so the phone can show
/// what is printed on the device label. Both survive every budget: only `name` is ever shortened.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeItem {
    pub deveui: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl PageItem for NodeItem {
    fn key(&self) -> &str {
        &self.deveui
    }
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    fn with_name(&self, name: Option<String>) -> NodeItem {
        NodeItem {
            deveui: self.deveui.clone(),
            serial: self.serial.clone(),
            name,
        }
    }
    fn minimal(&self) -> NodeItem {
        // The serial goes only after the name has already gone — the ladder in `fit_one` tries
        // `with_name(None)` first, so a link tight enough to lose the label still keeps the number
        // on the device's sticker if it can.
        NodeItem {
            deveui: self.deveui.clone(),
            serial: None,
            name: None,
        }
    }
    fn write_rev_line(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.deveui.as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(self.serial.as_deref().unwrap_or("").as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(self.name.as_deref().unwrap_or("").as_bytes());
        buf.push(b'\n');
    }
}

/// `{"page": N}`. `deny_unknown_fields` so a typo is a `bad_request` rather than a silent page 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    #[serde(default)]
    pub page: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PageResponse<T> {
    pub total: u32,
    pub page: u16,
    pub count: u32,
    pub rev: String,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PageError {
    pub error: String,
    pub message: String,
}

impl PageError {
    pub fn new(error: &str, message: impl Into<String>) -> PageError {
        PageError {
            error: error.to_string(),
            message: message.into(),
        }
    }

    /// The refusal on the wire. Identical to the `Error` arm of [`ListRead::to_bytes`] — which
    /// delegates here, so the two cannot drift — and available without naming an item type, because
    /// a refusal never carries one.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| {
            br#"{"error":"internal","message":"the page could not be serialized"}"#.to_vec()
        })
    }
}

/// Success and error share one **untagged** shape, so the app branches on the presence of `error`
/// without a wrapper level.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum ListRead<T> {
    Page(PageResponse<T>),
    Error(PageError),
}

impl<T: serde::Serialize> ListRead<T> {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            // A serialization failure here would be a bug in this module, not a runtime condition;
            // the fallback is still a complete JSON document rather than a fragment.
            ListRead::Page(p) => serde_json::to_vec(p).unwrap_or_else(|_| {
                br#"{"error":"internal","message":"the page could not be serialized"}"#.to_vec()
            }),
            ListRead::Error(e) => e.to_bytes(),
        }
    }
}

impl<T> ListRead<T> {
    pub fn is_error(&self) -> bool {
        matches!(self, ListRead::Error(_))
    }
}

/// FNV-1a, 64-bit. Chosen because `rev` needs to be **stable across restarts** — a `DefaultHasher`
/// is randomly seeded per process, so it would invalidate every phone's walk on every reboot.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// An enrolled list, sorted and hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagedList<T> {
    pub rev: String,
    pub items: Vec<T>,
}

/// The Beacon list — FB0F's, and the one whose byte-level behaviour is pinned.
pub type BeaconList = PagedList<ListItem>;
/// The Node list — FB11's.
pub type NodeList = PagedList<NodeItem>;

impl<T: PageItem> PagedList<T> {
    /// Sort by key so page N is stable, dedup by key, and hash exactly that sorted content.
    ///
    /// Two Hubs with the same fleet produce the same `rev`, and a rename that changes nothing leaves
    /// it untouched.
    pub fn from_items(mut items: Vec<T>) -> PagedList<T> {
        items.sort_by(|a, b| a.key().cmp(b.key()));
        items.dedup_by(|a, b| a.key() == b.key());
        PagedList {
            rev: Self::rev_of(&items),
            items,
        }
    }

    pub fn rev_of(items: &[T]) -> String {
        let mut buf = Vec::new();
        for i in items {
            i.write_rev_line(&mut buf);
        }
        // The low 32 bits, 8 lowercase hex characters — the narrowing peripheral.md §4 calls for.
        format!("{:08x}", fnv1a64(&buf) & 0xFFFF_FFFF)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl BeaconList {
    /// Build from the fleet section's tags.
    pub fn from_tags(tags: &[EyeTag]) -> BeaconList {
        PagedList::from_items(
            tags.iter()
                .map(|t| ListItem {
                    mac: t.mac.to_uppercase(),
                    name: t.name.clone().filter(|n| !n.is_empty()),
                })
                .collect(),
        )
    }
}

/// Rule 7: never rely on long reads.
pub fn page_budget(mtu: u16, max_page_bytes: usize) -> usize {
    (mtu.saturating_sub(1) as usize).min(max_page_bytes)
}

/// The serialized length of a page, measured with the **varying integer fields at their maximum
/// width**.
///
/// This is the fixed-width rule. Measuring with the real `page`/`count` would make a page boundary
/// depend on how many digits those numbers have, so page 9 and page 10 could hold different items —
/// and a client walking the list would see an item twice or not at all.
fn encoded_len<T: PageItem>(items: &[T], rev: &str, total: u32) -> usize {
    let probe = PageResponse {
        total,
        page: u16::MAX,
        count: u32::from(u16::MAX),
        rev: rev.to_string(),
        items: items.to_vec(),
    };
    serde_json::to_vec(&probe)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

/// Shorten an item until a single-item page fits. Returns `None` when even the key alone cannot fit.
///
/// The ladder, in order: the item as it is; the name truncated with an ellipsis, longest first, so
/// the operator keeps as much label as possible; the name dropped entirely; and finally
/// [`PageItem::minimal`], the key on its own. For a Beacon the last two rungs are the same item —
/// there is nothing between the name and the MAC — which is why generalizing the ladder leaves FB0F
/// byte-identical.
fn fit_one<T: PageItem>(item: &T, rev: &str, total: u32, budget: usize) -> Option<T> {
    if encoded_len(std::slice::from_ref(item), rev, total) <= budget {
        return Some(item.clone());
    }
    if let Some(name) = item.name() {
        let chars: Vec<char> = name.chars().collect();
        // Walk down from the longest truncation, so the operator keeps as much label as possible.
        for keep in (0..chars.len()).rev() {
            let mut shortened: String = chars[..keep].iter().collect();
            shortened.push(ELLIPSIS);
            let candidate = item.with_name(Some(shortened));
            if encoded_len(std::slice::from_ref(&candidate), rev, total) <= budget {
                return Some(candidate);
            }
        }
        // The label is gone, but everything else the item carries is still worth serving.
        let unnamed = item.with_name(None);
        if encoded_len(std::slice::from_ref(&unnamed), rev, total) <= budget {
            return Some(unnamed);
        }
    }
    // The key alone. Never truncated — a half key addresses the wrong device.
    let bare = item.minimal();
    if encoded_len(std::slice::from_ref(&bare), rev, total) <= budget {
        return Some(bare);
    }
    None
}

/// Build page `page` of `list` for a link of `mtu`.
pub fn build_page<T: PageItem>(
    list: &PagedList<T>,
    page: u16,
    mtu: u16,
    max_page_bytes: usize,
) -> ListRead<T> {
    let budget = page_budget(mtu, max_page_bytes);
    let total = list.len() as u32;
    let envelope = encoded_len::<T>(&[], &list.rev, total);

    // A link that cannot carry the envelope plus one item cannot carry a page at all. This fires
    // for the empty list too: at MTU 23 the *envelope* does not fit, and answering with a truncated
    // fragment of a JSON error is precisely what §5.1 says the guard must not do.
    let need = envelope + if list.is_empty() { 0 } else { MIN_ITEM_BYTES };
    if budget < need {
        return ListRead::Error(PageError::new(
            ERR_MTU_TOO_SMALL,
            format!(
                "a {budget}-byte page budget cannot carry this list; at least {need} bytes are \
                 needed. Negotiate a larger ATT MTU."
            ),
        ));
    }

    // Walk pages from the start. The boundaries are deterministic because `encoded_len` measures the
    // integer fields at fixed width, so this walk yields the same partition on every call.
    let mut index = 0usize;
    let mut current: u16 = 0;
    loop {
        let mut items: Vec<T> = Vec::new();
        while index < list.items.len() {
            let mut candidate = items.clone();
            candidate.push(list.items[index].clone());
            if encoded_len(&candidate, &list.rev, total) <= budget {
                items.push(list.items[index].clone());
                index += 1;
                continue;
            }
            if items.is_empty() {
                // Rule: a page is never empty while items remain. Shorten the name — never the key.
                match fit_one(&list.items[index], &list.rev, total, budget) {
                    Some(fitted) => {
                        items.push(fitted);
                        index += 1;
                    }
                    None => {
                        return ListRead::Error(PageError::new(
                            ERR_MTU_TOO_SMALL,
                            "not even one address fits in this page budget",
                        ))
                    }
                }
            }
            break;
        }

        if current == page {
            let count = items.len() as u32;
            return ListRead::Page(PageResponse {
                total,
                page,
                count,
                rev: list.rev.clone(),
                items,
            });
        }
        if index >= list.items.len() {
            // Past the end is an empty page, not an error: it is how a client's walk terminates.
            return ListRead::Page(PageResponse {
                total,
                page,
                count: 0,
                rev: list.rev.clone(),
                items: Vec::new(),
            });
        }
        current = match current.checked_add(1) {
            Some(n) => n,
            None => {
                return ListRead::Page(PageResponse {
                    total,
                    page,
                    count: 0,
                    rev: list.rev.clone(),
                    items: Vec::new(),
                })
            }
        };
    }
}

/// Parse a page request. A malformed body is `bad_request` — including a page index past `u16`,
/// which is what pins the fixed-width index.
pub fn parse_request(body: &[u8]) -> std::result::Result<PageRequest, PageError> {
    serde_json::from_slice::<PageRequest>(body).map_err(|e| {
        PageError::new(
            ERR_BAD_REQUEST,
            format!("expected {{\"page\": N}} with N in 0..=65535: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(mac: &str, name: Option<&str>) -> EyeTag {
        EyeTag {
            mac: mac.to_string(),
            name: name.map(str::to_string),
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: Vec::new(),
            provisioned: None,
        }
    }

    fn node(deveui: &str, serial: Option<&str>, name: Option<&str>) -> NodeItem {
        NodeItem {
            deveui: deveui.to_string(),
            serial: serial.map(str::to_string),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn the_budget_is_the_documented_minimum() {
        assert_eq!(page_budget(185, 180), 180, "the ceiling wins here");
        assert_eq!(page_budget(100, 180), 99, "mtu - 1 wins here");
        assert_eq!(page_budget(23, 180), 22);
        assert_eq!(page_budget(0, 180), 0, "no underflow");
    }

    #[test]
    fn rev_is_eight_lowercase_hex_characters() {
        let list = BeaconList::from_tags(&[tag("AA:BB:CC:DD:EE:FF", Some("Freezer 1"))]);
        assert_eq!(list.rev.len(), 8);
        assert!(
            list.rev
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "{}",
            list.rev
        );
    }

    #[test]
    fn rev_is_a_content_hash_not_a_counter() {
        let a = BeaconList::from_tags(&[tag("AA:BB:CC:DD:EE:FF", Some("Freezer 1"))]);
        // The same content again — as after a restart — hashes the same.
        let b = BeaconList::from_tags(&[tag("aa:bb:cc:dd:ee:ff", Some("Freezer 1"))]);
        assert_eq!(a.rev, b.rev, "it survives a restart and a case difference");

        // A rename that changes nothing leaves it alone.
        let c = BeaconList::from_tags(&[tag("AA:BB:CC:DD:EE:FF", Some("Freezer 1"))]);
        assert_eq!(a.rev, c.rev);

        // A real rename changes it.
        let d = BeaconList::from_tags(&[tag("AA:BB:CC:DD:EE:FF", Some("Freezer 2"))]);
        assert_ne!(a.rev, d.rev);

        // Adding a beacon changes it — which is what restarts a stale walk.
        let e = BeaconList::from_tags(&[
            tag("AA:BB:CC:DD:EE:FF", Some("Freezer 1")),
            tag("00:11:22:33:44:55", None),
        ]);
        assert_ne!(a.rev, e.rev);
    }

    #[test]
    fn items_are_sorted_by_mac_and_deduped() {
        let list = BeaconList::from_tags(&[
            tag("FF:FF:FF:FF:FF:FF", None),
            tag("00:00:00:00:00:01", None),
            tag("aa:aa:aa:aa:aa:aa", None),
            tag("AA:AA:AA:AA:AA:AA", Some("a duplicate spelling")),
        ]);
        let macs: Vec<&str> = list.items.iter().map(|i| i.mac.as_str()).collect();
        assert_eq!(
            macs,
            vec![
                "00:00:00:00:00:01",
                "AA:AA:AA:AA:AA:AA",
                "FF:FF:FF:FF:FF:FF"
            ]
        );
    }

    #[test]
    fn a_read_before_any_write_has_a_defined_error_key() {
        let e =
            ListRead::<ListItem>::Error(PageError::new(ERR_NO_REQUEST, "no page was requested"));
        let json = String::from_utf8(e.to_bytes()).expect("utf8");
        assert!(json.contains(r#""error":"no_request""#), "{json}");
        // The untagged shape means no wrapper level: the app branches on `error` being present.
        assert!(!json.contains("Error"), "{json}");
        assert!(!json.contains("Page"), "{json}");
        // The item type is not part of a refusal, so both spellings of one produce the same bytes.
        assert_eq!(
            e.to_bytes(),
            ListRead::<NodeItem>::Error(PageError::new(ERR_NO_REQUEST, "no page was requested"))
                .to_bytes()
        );
    }

    #[test]
    fn the_error_keys_are_the_closed_set() {
        assert_eq!(
            [
                ERR_NO_REQUEST,
                ERR_BAD_REQUEST,
                ERR_MTU_TOO_SMALL,
                ERR_INTERNAL
            ],
            ["no_request", "bad_request", "mtu_too_small", "internal"]
        );
    }

    #[test]
    fn a_page_index_past_65535_is_a_bad_request() {
        let e = parse_request(br#"{"page": 70000}"#).expect_err("must refuse");
        assert_eq!(e.error, ERR_BAD_REQUEST);
        // …and so is an unknown key, rather than being read as a default page 0.
        let e = parse_request(br#"{"pge": 1}"#).expect_err("must refuse");
        assert_eq!(e.error, ERR_BAD_REQUEST);
        // An empty object is page 0, which is the documented default.
        assert_eq!(parse_request(b"{}").expect("parses").page, 0);
        assert_eq!(parse_request(br#"{"page":7}"#).expect("parses").page, 7);
        assert!(parse_request(b"not json").is_err());
    }

    #[test]
    fn an_empty_list_at_a_normal_mtu_is_an_empty_page_not_an_error() {
        let list = BeaconList::from_tags(&[]);
        match build_page(&list, 0, 185, DEFAULT_MAX_PAGE_BYTES) {
            ListRead::Page(p) => {
                assert_eq!(p.total, 0);
                assert_eq!(p.count, 0);
                assert!(p.items.is_empty());
            }
            ListRead::Error(e) => panic!("unexpected error {e:?}"),
        }
    }

    #[test]
    fn the_mac_is_never_shortened_even_when_the_name_must_be() {
        let long = "\"".repeat(64);
        let list = BeaconList::from_tags(&[tag("AA:BB:CC:DD:EE:FF", Some(&long))]);
        match build_page(&list, 0, 185, DEFAULT_MAX_PAGE_BYTES) {
            ListRead::Page(p) => {
                assert_eq!(p.count, 1, "at least one item per page, always");
                assert_eq!(
                    p.items[0].mac, "AA:BB:CC:DD:EE:FF",
                    "17 characters, untouched"
                );
                let name = p.items[0].name.as_deref().unwrap_or("");
                assert!(
                    name.ends_with(ELLIPSIS),
                    "a shortened name is marked: {name:?}"
                );
                assert!(name.chars().count() < long.chars().count());
                assert!(p.count > 0);
            }
            ListRead::Error(e) => panic!("a single item must always be servable: {e:?}"),
        }
    }

    #[test]
    fn nodes_sort_and_dedup_by_deveui() {
        let list = NodeList::from_items(vec![
            node("ffffffffffffffff", None, None),
            node("0011223344556677", Some("SN-1"), Some("Fridge 1")),
            // A duplicate row would be a defect upstream — the add path upserts — but the pager
            // must not serve the same device twice if one ever appears.
            node("0011223344556677", Some("SN-1-again"), None),
        ]);
        let euis: Vec<&str> = list.items.iter().map(|i| i.deveui.as_str()).collect();
        assert_eq!(euis, vec!["0011223344556677", "ffffffffffffffff"]);
    }

    #[test]
    fn a_node_loses_its_name_before_its_serial_and_its_serial_before_its_deveui() {
        let item = node("0011223344556677", Some("SN-000000001"), Some("Fridge 1"));
        let rev = "00000000";

        // Wide enough for everything.
        let full = fit_one(&item, rev, 1, 200).expect("fits");
        assert_eq!(full, item);

        // Tight enough that the label has to go, but the sticker's number survives.
        let unnamed = fit_one(&item, rev, 1, 120).expect("fits");
        assert_eq!(unnamed.deveui, item.deveui);
        assert_eq!(unnamed.serial, item.serial, "the serial outlives the name");

        // Pathological: only the wire identity is left, and it is not shortened.
        let bare = fit_one(&item, rev, 1, 100).expect("fits");
        assert_eq!(bare, item.minimal());
        assert_eq!(bare.deveui, "0011223344556677");
        assert!(bare.serial.is_none() && bare.name.is_none());

        // And below even that, the answer is "no", never a truncated DevEUI.
        assert!(fit_one(&item, rev, 1, 20).is_none());
    }

    #[test]
    fn a_beacon_ladder_has_no_rung_between_the_name_and_the_mac() {
        // This is why generalizing `fit_one` leaves FB0F byte-identical: for a `ListItem` the
        // "drop the name" rung and the "key alone" rung are the same item.
        let item = ListItem {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: Some("Freezer 1".to_string()),
        };
        assert_eq!(item.with_name(None), item.minimal());
    }
}
