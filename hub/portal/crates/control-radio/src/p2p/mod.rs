//! The LoRa P2P arm — `plan/control/radio/p2p.md`, over the link
//! `plan/system/lora/design.md` specifies.
//!
//! **The wire.** The Northbridge is a TOWER gateway (`plan/control/radio/p2p_tower_gateway.md`,
//! sticker-firmware #470): it holds the peers and their session keys, opens and ACKs the
//! Nodes' frames itself and hands this side plaintext over the `TWR_*` link ([`tower`]). The
//! central stays the join server and the network server. The gateway image and the Hub
//! software ship together (contract §1, flag day), so there is no other P2P wire to speak.
//!
//! **The link is `/dev/ttyAMA3`, not `/dev/ttyAMA4`.** An earlier revision framed UART
//! contention as this plan's largest risk; that conflated the Hub's two bridge MCUs. The
//! housekeeping traffic (LEDs, rails, VIN polling) terminates at the *southbridge* on
//! `ttyAMA4`; the northbridge has its own physically separate USART1 -> `ttyAMA3`, already
//! wired and unused by the v1 agent. So P2P gets a dedicated link at zero hardware cost and
//! there is no arbitration to build.

pub mod airtime;
pub mod db;
pub mod fake;
pub mod gateway_fwd;
pub mod gateways;
pub mod gw_board;
pub mod hdlc;
pub mod modem;
pub mod modem_io;
pub mod nb_flash;
pub mod nb_update;
pub mod northbridge;
pub mod outbox;
pub mod plane;
pub mod publisher;
pub mod remote;
pub mod replay;
pub mod runtime;
pub mod session;
pub mod tower;
pub mod uart;
