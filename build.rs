fn main() {
    println!("cargo:rerun-if-env-changed=FIBER_VERSION");

    // Generate Rust types from the STICKER protocol schema (sticker-firmware
    // v1.4.0 app/src/app_config.proto). Single source of truth for fPort 2
    // Telemetry, fPort 3 AlarmReport and fPort 85 Command/Response decoding.
    println!("cargo:rerun-if-changed=proto/app_config.proto");
    prost_build::compile_protos(&["proto/app_config.proto"], &["proto"])
        .expect("failed to compile proto/app_config.proto with prost-build");
}
