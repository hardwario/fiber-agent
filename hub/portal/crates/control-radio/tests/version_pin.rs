//! The `chirpstack_api` ↔ ChirpStack-binary pin alignment check — `lorawan.md` §3.
//!
//! > the `chirpstack_api` crate version and the chirpstack binary pin must agree on
//! > `major.minor` … one version constant, one CI check that fails on `major.minor` drift.
//!
//! Relaxed to `major.minor` by MR !14 finding 1.1: the generated stubs change on minor bumps,
//! not on patches, and exact equality was unsatisfiable anyway — crates.io tops out at 4.19.0
//! while the binary pin is 4.19.1, so an equality check would have failed on its first run.
//!
//! This is a test rather than a bespoke CI job so it runs in `cargo test --workspace`, which
//! CI already invokes, and fails on a developer's machine at the moment of the edit.

/// `major.minor` of a `x.y.z` version string.
fn minor_of(v: &str) -> (u32, u32) {
    let mut it = v.split('.');
    let major = it.next().and_then(|s| s.parse().ok());
    let minor = it.next().and_then(|s| s.parse().ok());
    match (major, minor) {
        (Some(a), Some(b)) => (a, b),
        _ => panic!("not a x.y.z version: {v:?}"),
    }
}

/// The resolved crate version, from the lockfile rather than the manifest: the manifest says
/// `"4.19.0"`, which is a caret requirement, and what actually builds is what is locked.
fn locked_chirpstack_api() -> String {
    let lock = include_str!("../../../Cargo.lock");
    let idx = lock
        .find("name = \"chirpstack_api\"")
        .expect("chirpstack_api is a dependency");
    lock[idx..]
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|l| l.strip_suffix('"'))
        .expect("the package stanza carries a version")
        .to_string()
}

/// The binary pin, from the image tag the integration stack actually runs.
fn compose_chirpstack_image() -> String {
    let compose = include_str!("integration/docker-compose.yml");
    compose
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("image: chirpstack/chirpstack:"))
        .expect("the integration stack pins a chirpstack image")
        .to_string()
}

#[test]
fn the_crate_and_the_binary_pin_agree_on_major_minor() {
    let crate_v = locked_chirpstack_api();
    let binary_v = compose_chirpstack_image();
    assert_eq!(
        minor_of(&crate_v),
        minor_of(&binary_v),
        "chirpstack_api {crate_v} and the chirpstack binary pin {binary_v} disagree on \
         major.minor. The generated stubs move on minor bumps, so this pair has to be bumped \
         together (lorawan.md §3)."
    );
}

/// The **shipped** binary pin: the Yocto recipe's own filename version.
///
/// `include_str!` cannot glob and the recipe's name moves with the version, so this reads the
/// directory at run time, relative to `CARGO_MANIFEST_DIR`.
///
/// `None` when the directory is not there. That is **tolerated on purpose and loudly**: this
/// crate is built standalone out of tree (decision #12's standalone guarantee, and the
/// `control-radio-harness` integration stack), where `system/meta-proximos/` simply does not
/// exist. Inside the superproject the recipe does exist — it is
/// `recipes-connectivity/chirpstack/chirpstack_4.19.1.bb` — and the assertion below runs for
/// real. A silent skip would be worthless, so the test prints which branch it took.
fn yocto_recipe_version() -> Option<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../system/meta-proximos/recipes-connectivity/chirpstack");
    let entries = std::fs::read_dir(dir).ok()?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix("chirpstack_") {
            if let Some(version) = rest.strip_suffix(".bb") {
                // `chirpstack_%.bbappend` and friends are not a pin; only `_<version>.bb` is.
                if version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    found.push(version.to_string());
                }
            }
        }
    }
    found.sort();
    // More than one would mean two pins in one layer, which bitbake resolves by preference —
    // an ambiguity this check must not paper over by picking one.
    assert!(
        found.len() < 2,
        "several chirpstack recipes pin different versions: {found:?}"
    );
    found.pop()
}

/// The recipe that actually ships on the image must agree with the locked crate on
/// `major.minor`, for the same reason the compose pin must: the generated stubs move on minor
/// bumps, and a Hub running a server whose API the client's stubs do not match fails on the
/// wire, at run time, on a device.
#[test]
fn the_crate_and_the_shipped_recipe_agree_on_major_minor() {
    let crate_v = locked_chirpstack_api();
    match yocto_recipe_version() {
        Some(recipe_v) => {
            assert_eq!(
                minor_of(&crate_v),
                minor_of(&recipe_v),
                "chirpstack_api {crate_v} and the Yocto recipe pin {recipe_v} disagree on \
                 major.minor — bump them together (lorawan.md §3)."
            );
            println!("checked chirpstack_api {crate_v} against the recipe pin {recipe_v}");
        }
        None => println!(
            "system/meta-proximos/recipes-connectivity/chirpstack/ is absent — this is an \
             out-of-tree build of the standalone crate, so there is no image pin to check. \
             Inside the superproject this branch must NOT be taken."
        ),
    }
}

/// Patch-level divergence is allowed on purpose, and is the shipped state: crates.io has no
/// 4.19.1. A check that forbade it would fail on every run.
#[test]
fn patch_level_divergence_is_permitted() {
    assert_eq!(minor_of("4.19.0"), minor_of("4.19.1"));
    assert_ne!(minor_of("4.19.0"), minor_of("4.20.0"));
}
