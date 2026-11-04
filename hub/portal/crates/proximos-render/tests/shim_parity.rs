//! Parity with the shell shim this crate replaces.
//!
//! `system/meta-proximos/recipes-core/proximos-render/files/proximos-render.sh` was the
//! shipping renderer until this crate replaced it (deleted in the same MR — see git
//! history). This file is the parity proof its deletion was conditioned on.
//!
//! `tests/fixtures/shim-portal-config.yaml` is the shim's heredoc, expanded by hand for a
//! known `db.key` — the same substitution `sh` performs, with `DATA_DIR=/data/portal` and
//! `OVERRIDE_FILE=/data/portal/config.yaml`, and with the trailing newline the heredoc
//! leaves behind. It cannot be produced by running the script: every path in it is
//! absolute and compiled in, and generating the key needs `/proc/sys/kernel/random/uuid`.
//!
//! **The one intentional difference is the header comment**, and it has to be one: the
//! rendered file names the renderer that wrote it, and that is no longer a shell script.
//! So the assertion is in three parts, which together are stronger than a single
//! `assert_eq!` on the whole file would be:
//!
//! 1. every non-comment line is byte-identical, in order — the payload is the same bytes;
//! 2. both documents deserialize to the identical YAML value — the payload also *means*
//!    the same thing, comments being nothing to a parser;
//! 3. the header is still a comment block, still four lines, and still points an operator
//!    at an override file that exists.
//!
//! **Keys added after the shim.** The shim is deleted, so this file is now a regression
//! guard rather than the precondition for deleting it, and the product has to be able to
//! grow keys the shim never wrote. Those live in [`POST_SHIM_KEYS`] and are excluded from
//! all three comparisons above — which keeps the claim that actually matters ("nothing
//! the shim wrote has changed") exactly as strong as it was, while letting a new key be
//! added deliberately, in one named place, rather than by editing the fixture. The
//! fixture is never edited: it is the shim's heredoc, and it is evidence.
//!
//! A new key's own correctness is covered by `tests/render.rs`'s golden files, not here.

mod common;

use common::Device;

const SHIM: &str = include_str!("fixtures/shim-portal-config.yaml");

/// Top-level keys the renderer emits that the shim never did. Adding one here is the
/// deliberate act of saying "this is new, and the shim's payload is otherwise untouched".
///
/// - `maintenance_window`: the scheduled-VACUUM window (plan/portal/sqlite-vacuum.md §6),
///   which post-dates the shim entirely.
/// - `mqtt_client_id`: the portal's broker client id (plan/portal/mqtt/design.md §6.2,
///   §12), new in P2. The shim wrote no client id at all — the portal derived one — which
///   is exactly the accident 3.1.1's same-id takeover rule makes worth rendering.
/// - `thresholds`: the default bands a new temperature or humidity sensor gets
///   (plan/portal/threshold_defaults.md §2), 2026-09-28.
const POST_SHIM_KEYS: &[&str] = &["maintenance_window", "mqtt_client_id", "thresholds"];

/// Keys the shim **did** write and whose *value* the product has since deliberately
/// moved. Distinct from [`POST_SHIM_KEYS`], and deliberately so: adding a key here says
/// "the shim wrote this and we changed what it says"; adding it there would say "the shim
/// never wrote this", which for `topic_prefix` is simply false and would drop the key out
/// of all three comparisons for good.
///
/// Only the **value** is excused. The key must still be present, in the same position, in
/// a line of the same shape — so the key-order test below and the key-set half of the two
/// above keep their full strength, and the eight other keys keep byte-identity.
///
/// The new value is not unchecked: `tests/golden/lorawan/portal-config.yaml` pins the
/// rendered file byte for byte, and `control_schema.rs` pins the same root on the agent's
/// side. This list says "expect a difference here", not "stop looking".
///
/// - `topic_prefix`: `fiber` -> `proximos` (plan/portal/mqtt/design.md §9, P5). The rename
///   the whole section exists to finish; the shim predates it.
const MOVED_SINCE_SHIM: &[&str] = &["topic_prefix"];

/// The key a `key: value` line declares, if it is one.
fn key_of(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    line.split_once(':').map(|(k, _)| k.trim())
}

/// Whether `line` belongs to a [`POST_SHIM_KEYS`] block — the key line itself or one of
/// its indented children.
fn is_post_shim(line: &str, inside_block: &mut bool) -> bool {
    let indented = line.starts_with(char::is_whitespace);
    if *inside_block && indented {
        return true;
    }
    *inside_block = POST_SHIM_KEYS
        .iter()
        .any(|k| line.starts_with(&format!("{k}:")));
    *inside_block
}

/// Non-comment, non-post-shim lines: the payload the shim actually wrote.
fn body(s: &str) -> Vec<&str> {
    let mut inside = false;
    s.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| !is_post_shim(l, &mut inside))
        .collect()
}

fn header(s: &str) -> Vec<&str> {
    s.lines()
        .take_while(|l| l.trim_start().starts_with('#'))
        .collect()
}

/// A device that already carries a shim-written `db.key`: the upgrade case, and the only
/// one where "the same bytes" is a claim worth making.
fn rendered() -> String {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    d.read_run("portal/config.yaml")
}

#[test]
fn every_key_line_is_byte_identical_to_the_shims() {
    let ours = rendered();
    let (mine, theirs) = (body(&ours), body(SHIM));

    // Same number of lines, in the same order, declaring the same keys — that half is
    // never excused.
    assert_eq!(
        mine.iter().map(|l| key_of(l)).collect::<Vec<_>>(),
        theirs.iter().map(|l| key_of(l)).collect::<Vec<_>>(),
        "the rendered payload's key set or order diverged from proximos-render.sh's\n\
         --- shim ---\n{SHIM}\n--- proximosctl render ---\n{ours}"
    );

    for (mine, theirs) in mine.iter().zip(theirs.iter()) {
        if key_of(mine).is_some_and(|k| MOVED_SINCE_SHIM.contains(&k)) {
            assert_ne!(
                mine, theirs,
                "{mine:?} is listed in MOVED_SINCE_SHIM but is identical to the shim's — \
                 drop it from that list rather than leaving a dead exception"
            );
            continue;
        }
        assert_eq!(
            mine, theirs,
            "the rendered payload diverged from proximos-render.sh's\n--- shim ---\n{SHIM}\n\
             --- proximosctl render ---\n{ours}"
        );
    }
}

#[test]
fn both_documents_parse_to_the_same_value() {
    let ours = rendered();
    let a: serde_yaml::Value = serde_yaml::from_str(SHIM).expect("shim fixture parses");
    let mut b: serde_yaml::Value = serde_yaml::from_str(&ours).expect("rendered config parses");
    let mut a = a;
    if let Some(map) = b.as_mapping_mut() {
        for key in POST_SHIM_KEYS {
            assert!(
                map.remove(serde_yaml::Value::from(*key)).is_some(),
                "{key} is listed as a post-shim key but the renderer no longer emits it"
            );
        }
        // Dropped from *both* sides, so the key set is still compared and only the
        // value is excused.
        for key in MOVED_SINCE_SHIM {
            let k = serde_yaml::Value::from(*key);
            assert!(
                map.remove(&k).is_some(),
                "{key} is listed in MOVED_SINCE_SHIM but the renderer no longer emits it"
            );
            assert!(
                a.as_mapping_mut().unwrap().remove(&k).is_some(),
                "{key} is listed in MOVED_SINCE_SHIM but the shim never wrote it — it \
                 belongs in POST_SHIM_KEYS instead"
            );
        }
    }
    assert_eq!(a, b);
}

#[test]
fn the_key_order_is_preserved_as_well_as_the_key_set() {
    // Not redundant with the value comparison above: serde_yaml's Mapping compares
    // order-insensitively, and a diff in a review is read line by line.
    let ours = rendered();
    let keys = |s: &str| -> Vec<String> {
        let mut inside = false;
        s.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .filter(|l| !is_post_shim(l, &mut inside))
            .filter_map(|l| l.split(':').next().map(str::to_string))
            .collect()
    };
    assert_eq!(keys(&ours), keys(SHIM));
    assert_eq!(
        keys(SHIM),
        vec![
            "bind_addr",
            "mqtt_host",
            "mqtt_port",
            "topic_prefix",
            "data_dir",
            "templates_dir",
            "static_dir",
            "db_key",
            "cookie_secure",
        ],
        "the fixture itself drifted from what the shim's heredoc writes"
    );
}

#[test]
fn the_shim_written_db_key_is_carried_forward_rather_than_regenerated() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    assert!(d
        .read_run("portal/config.yaml")
        .contains(Device::SHIM_DB_KEY));
    assert_eq!(
        std::fs::read_to_string(d.paths.data.join("portal/db.key")).unwrap(),
        Device::SHIM_DB_KEY,
        "the existing key file was rewritten — every row in the portal database just \
         became unreadable"
    );
}

#[test]
fn the_header_is_still_a_four_line_comment_pointing_at_an_override_file() {
    let ours = rendered();
    let h = header(&ours);
    assert_eq!(h.len(), 4, "{h:?}");
    assert_eq!(header(SHIM).len(), 4);
    assert!(h.iter().any(|l| l.contains("proximosctl render")));
    assert!(
        h.iter().any(|l| l.contains("/data/portal/config.yaml")),
        "the header must still name a path an operator can actually write to: {h:?}"
    );
    assert!(h.iter().any(|l| l.contains("lost on the next render")));
}

/// The shim generated a key on a virgin device. So does this, and the resulting file has
/// the same shape: 64 lowercase hex characters, no trailing newline, so the two
/// implementations are interchangeable across a reboot.
#[test]
fn a_generated_key_has_the_same_on_disk_shape_as_the_shims() {
    let d = Device::new().with_api_secret();
    d.render().expect("render");
    let key = std::fs::read_to_string(d.paths.data.join("portal/db.key")).unwrap();

    assert_eq!(key.len(), 64, "{key:?}");
    assert!(
        !key.ends_with('\n'),
        "the shim's printf '%s' wrote no newline"
    );
    assert!(key
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    assert!(d
        .read_run("portal/config.yaml")
        .contains(&format!("db_key: \"{key}\"")));
}

/// The zero-byte `db.key` the shim's first shipped version left behind, and the reason its
/// guard is `! -s` plus a length check rather than `! -f`.
#[test]
fn an_unusable_key_file_is_regenerated_rather_than_reused() {
    for broken in ["", "cafebabe", &" ".repeat(64)] {
        let d = Device::new()
            .with_api_secret()
            .write_data("portal/db.key", broken);
        d.render().expect("render");
        let key = std::fs::read_to_string(d.paths.data.join("portal/db.key")).unwrap();
        assert_eq!(key.len(), 64, "starting from {broken:?}");
        assert_ne!(key, broken);
        assert!(d.read_run("portal/config.yaml").contains(&key));
    }
}
