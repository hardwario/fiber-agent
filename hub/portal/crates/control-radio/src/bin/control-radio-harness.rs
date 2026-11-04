//! The standalone CI harness — decision #12 / `plan/portal/monolith.md` §11.7.
//!
//! > `control-radio` may depend on no other crate in the workspace, and its CI integration
//! > stack runs it through a test-harness binary with **no agent subsystems started**.
//!
//! That is what this binary is, and it is the acceptance criterion for the standalone
//! guarantee in the form that survives a shared process. It drives the real dispatch path —
//! the shipped code, not a parallel arrangement — against whatever ChirpStack and broker the
//! integration stack provides, with nothing else in the process.
//!
//! ```sh
//! PROXIMOS_CONFIG=tests/integration/radio.yaml \
//! PROXIMOS_RADIO_DATA=/tmp/radio-harness \
//!   control-radio-harness create --radio lorawan
//! ```

use std::process::ExitCode;

use clap::Parser;
use control_radio::cli::ControlRadioCli;

fn main() -> ExitCode {
    let cli = ControlRadioCli::parse();

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

    let result = rt.block_on(async {
        let config = control_radio::dispatch::load_config()?;

        // Connect only when this invocation actually touches ChirpStack. A P2P-only Hub runs
        // none, and a command that never reaches the LoRaWAN arm must not wait on it.
        let client = if control_radio::dispatch::needs_chirpstack(&cli.inner, &config) {
            let tokens = control_radio::lorawan::Tokens::new(control_radio::store::Store::new(
                control_radio::dispatch::store_root(),
            ));
            match control_radio::lorawan::tonic_client::TonicChirpStack::connect(tokens).await {
                Ok(c) => Some(c),
                // `status` must work when things are broken — that is when it is used. A
                // refused connection is reported inside the report (`api_status`), never as
                // exit 20 from the one command whose job is to say what is wrong.
                Err(_) if matches!(cli.inner.command, control_radio::cli::Command::Status(_)) => {
                    None
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let broker = control_radio::mqtt::RumqttcRetainedTopics::new();
        // The integration stack asserts the tombstone on a real broker, so the harness wires
        // the publish seam exactly as the shipped CLI does.
        let publisher = control_radio::mqtt::RumqttcPublisher::new();
        control_radio::dispatch::run(
            &cli.inner,
            &config,
            client
                .as_ref()
                .map(|c| c as &dyn control_radio::lorawan::client::ChirpStack),
            Some(&broker),
            Some(&publisher),
        )
        .await
    });

    match result {
        Ok(outcome) => {
            use control_radio::dispatch::Outcome;
            let rendered = match &outcome {
                Outcome::Status(r) => serde_json::to_string_pretty(r.as_ref()).ok(),
                Outcome::Json(v) => serde_json::to_string_pretty(v).ok(),
                Outcome::Text(t) => Some(t.clone()),
            };
            if let Some(s) = rendered {
                println!("{s}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            // The same envelope and the same exit code the CLI produces, so the integration
            // stack asserts against the shipped contract rather than a harness-only one.
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
