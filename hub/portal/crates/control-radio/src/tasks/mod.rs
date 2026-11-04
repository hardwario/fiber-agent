//! The three loop duties, as spawnable work rather than a daemon — decision #12 and
//! `plan/control/radio/plan.md` "Runtime form — in-process tasks, not a daemon".
//!
//! > The three loop duties become tasks in `proximos-control`: subscribe
//! > `application/{app_id}/device/{dev_eui}/event/{up,txack}` and run §7's uplink processing →
//! > publish northbound; poll gateway status every **60 s** and publish `radio/gateways`;
//! > maintain the **`node-get` cache**.
//!
//! **Nothing here owns a runtime, a connection or a timer.** Each module is one tick or one
//! frame, taking what it needs as an argument and returning what should be published — which
//! is what lets the whole uplink path be tested with no broker, no ChirpStack and no clock,
//! against the shipped code rather than a parallel arrangement. `proximos-control` supplies
//! the loop, the supervision and the sockets; the standalone guarantee (decision #12) means
//! this crate must remain drivable without it.

pub mod health;
pub mod heartbeat;
pub mod uplink;
