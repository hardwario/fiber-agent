//! The standalone CI/bench harness — decision #12 / `plan/portal/monolith.md` §11.7.
//!
//! > `control-ble` may depend on no other crate in the workspace, and its CI integration stack runs
//! > it through a test-harness binary with **no agent subsystems started**.
//!
//! That is what this binary is, and it is the acceptance criterion for the standalone guarantee in
//! the form that survives a shared process: it drives the real dispatch path — the shipped code, not
//! a parallel arrangement — with nothing else running.
//!
//! One-shot:
//!
//! ```sh
//! PROXIMOS_CONFIG=tests/integration/ble.yaml \
//! PROXIMOS_FLEET_CONFIG=/tmp/ble-harness/fleet.yaml \
//! PROXIMOS_BLE_DATA=/tmp/ble-harness \
//!   control-ble-harness beacon-add --mac 7C:D9:F4:13:10:DE --name "Bench 1"
//! ```
//!
//! Live — this is how the bench runs the scan loop and the FB00 server with no agent subsystem. The
//! process stays up until SIGINT and re-prints `status` every `scan_interval_s`:
//!
//! ```sh
//! PROXIMOS_CONFIG=tests/integration/ble.yaml \
//! PROXIMOS_FLEET_CONFIG=/tmp/ble-harness/fleet.yaml \
//! PROXIMOS_BLE_DATA=/tmp/ble-harness PROXIMOS_BLE_HARNESS_LIVE=1 \
//!   control-ble-harness status --role central
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use control_ble::cli::{Command, ControlBleCli};
use control_ble::dispatch::{self, Outcome};
use control_ble::host::Host;
use control_ble::mac::MacAddr;
use control_ble::store::Store;
use control_ble::tasks;

/// Set to `1` to keep the process alive with the loop duties running.
const ENV_LIVE: &str = "PROXIMOS_BLE_HARNESS_LIVE";

/// Commands that need the running tasks. Spawning for these is what makes the harness usable for
/// anything beyond `create`/`status`/enrollment.
fn needs_live(command: &Command) -> bool {
    matches!(
        command,
        Command::BeaconGet(_)
            | Command::BeaconScanList
            | Command::BeaconScanStart
            | Command::BeaconScanStop
            | Command::BeaconProvision(_)
            | Command::BeaconSend(_)
            | Command::BeaconDetect(_)
            | Command::BeaconRecording(_)
            | Command::BeaconHistory(_)
            | Command::PhoneSessionStart(_)
            | Command::PhoneSessionStop
            | Command::PhoneSessionStatus
            | Command::PhoneAdvertiseStart
            | Command::PhoneAdvertiseStop
    )
}

fn stay_alive() -> bool {
    std::env::var(ENV_LIVE).is_ok_and(|v| v == "1")
}

fn main() -> ExitCode {
    let cli = ControlBleCli::parse();

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("harness: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let live_wanted = stay_alive() || needs_live(&cli.inner.command);

    let result = rt.block_on(async {
        let config = dispatch::load_config()?;
        let host = Host::for_harness(dispatch::fleet_config_path());
        let store = Store::new(dispatch::store_root());

        // The harness's own address is only knowable from BlueZ; on a host with no controller it
        // stays all-zero and the QR preview says so honestly rather than inventing one.
        let hub_mac = harness_adapter_mac(&config).await;

        let runtime = if live_wanted {
            Some(tasks::spawn(
                &tokio::runtime::Handle::current(),
                &config,
                host.clone(),
                store.clone(),
                hub_mac,
            ))
        } else {
            None
        };

        // The scan loop is the `SubsystemControl` when one is hosted — which is what turns
        // `monitoring: unavailable` into `enabled_by_this_call` on the bench.
        let host = match runtime.as_ref().and_then(|r| r.handles.central.clone()) {
            Some(central) => Host {
                subsystem: Arc::new(central),
                ..host
            },
            None => host,
        };

        let outcome = dispatch::run_with_store(
            &cli.inner,
            &config,
            &host,
            runtime.as_ref().map(|r| &r.handles),
            store,
        )
        .await;

        // Print the first answer before settling in, so a bench operator sees something at once.
        if let Ok(o) = &outcome {
            print_outcome(o);
        }

        if stay_alive() {
            if let Some(rt) = runtime {
                let interval = std::time::Duration::from_secs(config.central.scan_interval_s.max(1));
                eprintln!(
                    "harness: live; re-printing status every {}s. Ctrl-C to stop.",
                    interval.as_secs()
                );
                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!("harness: shutting down");
                            rt.shutdown().await;
                            break;
                        }
                        _ = tokio::time::sleep(interval) => {
                            let status = ControlBleCli::try_parse_from([
                                "control.ble", "status", "--role",
                                if config.mode.allows(control_ble::Role::Central) { "central" } else { "peripheral" },
                            ]);
                            if let Ok(s) = status {
                                if let Ok(o) = dispatch::run_with_store(
                                    &s.inner, &config, &host, Some(&rt.handles),
                                    Store::new(dispatch::store_root()),
                                ).await {
                                    print_outcome(&o);
                                }
                            }
                        }
                    }
                }
            }
            return outcome.map(|_| ());
        }

        if let Some(rt) = runtime {
            rt.shutdown().await;
        }
        outcome.map(|_| ())
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The same envelope and the same exit code `proximosctl --json` produces, so the
            // integration stack asserts against the shipped contract rather than a harness-only one.
            match serde_json::to_string_pretty(err.detail()) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("harness: could not render the error: {e}"),
            }
            u8::try_from(err.exit_code())
                .map(ExitCode::from)
                .unwrap_or(ExitCode::FAILURE)
        }
    }
}

fn print_outcome(o: &Outcome) {
    let rendered = match o {
        Outcome::Status(r) => serde_json::to_string_pretty(r.as_ref()).ok(),
        Outcome::Json(v) => serde_json::to_string_pretty(v).ok(),
        Outcome::Text(t) => Some(t.clone()),
    };
    if let Some(s) = rendered {
        println!("{s}");
    }
}

/// The local adapter's address, or all-zero when there is no controller.
async fn harness_adapter_mac(config: &control_ble::config::BleConfig) -> MacAddr {
    let Ok(session) = bluer::Session::new().await else {
        return MacAddr::from([0u8; 6]);
    };
    let adapter = match &config.adapter {
        Some(name) => session.adapter(name),
        None => session.default_adapter().await,
    };
    match adapter {
        Ok(a) => a
            .address()
            .await
            .map(MacAddr::from)
            .unwrap_or_else(|_| MacAddr::from([0u8; 6])),
        Err(_) => MacAddr::from([0u8; 6]),
    }
}
