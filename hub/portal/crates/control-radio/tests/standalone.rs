//! The standalone guarantee, restated at crate level — decision #12 and
//! `plan/portal/monolith.md` §11.7.
//!
//! > `control-radio` may depend on no other crate in the workspace, and its CI integration
//! > stack runs it through a test-harness binary with no agent subsystems started.
//!
//! The plan set's original form of this guarantee was "runs with no `proximos-app` installed
//! at all". Living in one workspace makes that unenforceable by deployment, so it is enforced
//! by dependency direction instead: nothing here may reach into `control-core`,
//! `portal-http`, `portal-storage`, `portal-domain` or `fiber-protocol`. A `path = "../..."`
//! line in Cargo.toml is exactly the thing that would break it, silently, in a commit that
//! looks like a convenience.

#[test]
fn the_crate_depends_on_no_other_workspace_crate() {
    let manifest = include_str!("../Cargo.toml");

    let mut offenders = Vec::new();
    for (n, line) in manifest.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        if code.contains("path") && code.contains('=') && code.contains("..") {
            offenders.push(format!("  Cargo.toml:{}: {}", n + 1, line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "control-radio must depend on no other crate in this workspace \
         (decision #12, monolith.md §11.7), found:\n{}",
        offenders.join("\n")
    );
}

/// The sibling crates by name, in case a future dependency arrives as a registry name rather
/// than a path (a rename, a vendored publish).
#[test]
fn no_sibling_crate_is_named_as_a_dependency() {
    let manifest = include_str!("../Cargo.toml");
    for sibling in [
        "control-core",
        "portal-http",
        "portal-domain",
        "portal-storage",
        "fiber-protocol",
    ] {
        for line in manifest.lines() {
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.contains(sibling),
                "control-radio must not depend on {sibling}: {}",
                line.trim()
            );
        }
    }
}
