//! The peripheral role: the FB00 GATT server the phone app dials into.
//! Design: `plan/control/ble/peripheral.md`.

pub mod advertise;
pub mod buffers;
pub mod events;
pub mod gatt;
pub mod paging;
pub mod session;
