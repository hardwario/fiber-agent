fn main() {
    println!("cargo:rerun-if-env-changed=FIBER_VERSION");

    // Generate Rust types from the STICKER protocol schema (sticker-firmware
    // app/src/app_config.proto at 408eeaf, release v2.0.0 PR #503, verbatim). Single source of truth for fPort 2 Telemetry, fPort 3
    // AlarmReport and fPort 85 Command/Response decoding.
    println!("cargo:rerun-if-changed=proto/app_config.proto");
    prost_build::compile_protos(&["proto/app_config.proto"], &["proto"])
        .expect("failed to compile proto/app_config.proto with prost-build");
}
