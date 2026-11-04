//! The standalone guarantee, restated at crate level — decision #12, `plan/portal/monolith.md`
//! §11.7, `plan/control/ble/plan.md` §5 "Runtime form": `control-ble` may depend on no other crate
//! in the workspace. A `path = "../…"` line in Cargo.toml is exactly the thing that would break it.

#[test]
fn the_crate_depends_on_no_other_workspace_crate() {
    let manifest = include_str!("../Cargo.toml");
    let offenders: Vec<String> = manifest
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = line.split('#').next().unwrap_or("");
            code.contains("path") && code.contains('=') && code.contains("..")
        })
        .map(|(n, line)| format!("  Cargo.toml:{}: {}", n + 1, line.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "control-ble must depend on no other crate in this workspace:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_sibling_crate_is_named_as_a_dependency() {
    let manifest = include_str!("../Cargo.toml");
    for sibling in [
        "control-core",
        "control-radio",
        "portal-http",
        "portal-domain",
        "portal-storage",
        "fiber-protocol",
        "proximosctl",
    ] {
        for line in manifest.lines() {
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.contains(sibling),
                "control-ble must not depend on {sibling}: {}",
                line.trim()
            );
        }
    }
}
