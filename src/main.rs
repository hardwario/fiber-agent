mod model;
mod alarms;
mod acquisition;
mod system;
mod logging;
mod storage;
mod runtime;
mod hal;
mod drivers;
mod app;
mod sensors;
mod ui;
mod display;
mod config;
mod network;
mod buttons;
mod power;
mod audit;
mod audit_db;
mod blockchain;
mod compaction;
mod discovery;

use anyhow::Result;

fn main() -> Result<()> {
    app::run_cm4()
}
