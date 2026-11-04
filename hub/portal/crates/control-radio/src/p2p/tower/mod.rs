//! P2P over TOWER frames — the Hub half of sticker-firmware #470, phase P2
//! (`plan/control/radio/p2p_tower_gateway.md`).
//!
//! The Northbridge is a **TOWER gateway**: it holds the peer table and the session keys in RAM,
//! opens and ACKs the Nodes' frames itself within the 20 ms turnaround, and hands the central
//! plaintext. The central stays the join server and the network server — it verifies joins,
//! derives and installs the session keys, issues the gateway's TX counter blocks, decodes, and
//! queues downlinks. The gateway image and the Hub software ship together (contract §1, flag
//! day): there is no other P2P wire and nothing to negotiate.
//!
//! - [`frame`]: the TOWER radio frame (header, AES-128-CCM, M = 8) and the TOWER key
//!   derivation, pinned by the two KAT fixtures.
//! - [`control`]: the `0x91` control TLV list the Node and the central exchange.
//! - [`link`]: the `TWR_*` commands and events of the gateway link (contract §2).
//! - [`delivery`]: which ring events to process and when to ack them (link doc §4.1).

pub mod control;
pub mod delivery;
pub mod frame;
pub mod link;
#[cfg(test)]
mod link_vectors;

/// The longest fPort-85 command body a TOWER downlink carries: the frame's payload less the
/// `0x81 86` envelope (contract §3.4), 72 B since #486 U-1. `node-send` refuses longer ones.
pub const MAX_COMMAND_BODY: usize = frame::MAX_PAYLOAD - 2;

/// The TOWER address of a Node: the low 32 bits of its DevEUI (#470 D6).
///
/// `dev_eui` is the canonical 16-hex spelling the registry keys by; `None` for anything else.
pub fn addr_of(dev_eui: &str) -> Option<u32> {
    if dev_eui.len() != 16 || !dev_eui.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Truncation is the definition: the low 32 bits.
    u64::from_str_radix(dev_eui, 16).ok().map(|v| v as u32)
}

/// Refuse a Node whose TOWER address another Node on P2P already holds (contract §3.1,
/// amendment 3): `node-add --radio p2p` and `node-move --to p2p` of the second one.
///
/// `on_p2p` is every DevEUI on the registry's P2P view; `dev_eui` itself may be among them.
pub fn check_address_free<'a>(
    dev_eui: &str,
    on_p2p: impl IntoIterator<Item = &'a str>,
) -> crate::error::Result<()> {
    let Some(addr) = addr_of(dev_eui) else {
        return Ok(());
    };
    let others: Vec<&str> = on_p2p
        .into_iter()
        .filter(|other| *other != dev_eui && addr_of(other) == Some(addr))
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    Err(crate::error::Error::new(
        crate::error::Kind::AddressCollision,
        crate::error::Transport::P2p,
        crate::error::Component::NodeRegistry,
        format!(
            "dev_eui {dev_eui} has the P2P address {addr:08x} (its low 32 bits), which {} \
             already holds on p2p; the Hub's P2P radio cannot serve both — keep one of them on \
             LoRaWAN",
            others.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_node_on_the_same_address_is_refused_with_both_dev_euis() {
        let on = ["5876070000000413", "70b3d57ed0000abe"];
        assert!(check_address_free("5876070000000413", on).is_ok());
        assert!(check_address_free("1111111100000001", on).is_ok());
        let err = check_address_free("70b3d57e00000413", on).expect_err("collision");
        assert_eq!(err.kind(), crate::error::Kind::AddressCollision);
        assert_eq!(err.component(), crate::error::Component::NodeRegistry);
        assert!(
            err.message().contains("5876070000000413"),
            "{}",
            err.message()
        );
        assert!(
            err.message().contains("70b3d57e00000413"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn the_address_is_the_low_half_of_the_dev_eui() {
        assert_eq!(addr_of("5876070000000413"), Some(1043));
        assert_eq!(addr_of("70b3d57ed0000abe"), Some(0xd000_0abe));
        assert_eq!(addr_of("0413"), None);
        assert_eq!(addr_of("58760700zz000413"), None);
        assert_eq!(addr_of("587607000000041z"), None);
    }
}
