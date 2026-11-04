//! The raw ATT bearer over L2CAP CID 4 — `plan/control/ble/beacon-btsmp1.md` §3,
//! `plan/control/ble/central.md` §2.7.
//!
//! **This is the crate's only `unsafe` module,** and it exists for one reason: BlueZ's D-Bus GATT
//! interface leaves the ATT MTU at its 23-byte default, and the archive is read in chunks far larger
//! than that. Owning the bearer means doing the MTU exchange ourselves, which means a socket.
//!
//! Two kernel facts constrain everything here:
//!
//! - The kernel permits userspace exactly **one** fixed CID on LE — the ATT channel — and both bind
//!   and connect enforce it. A connect on that CID *creates the LE link* and needs no capability;
//!   only privileged PSM binds do. So the unit needs `AF_BLUETOOTH` in `RestrictAddressFamilies`
//!   and nothing more.
//! - There is **one ATT bearer per LE link**, and a raw dial is invisible to BlueZ. It must never
//!   race a BlueZ-side connection to the same beacon, which is why every caller claims the shared
//!   adapter slot first.
//!
//! Every `unsafe` block below is a single libc call whose invariant is stated on the line above it.
//! The blocking API is deliberate: callers run it under `spawn_blocking` inside the contained job.

#![allow(unsafe_code)]

use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

use crate::mac::MacAddr;

use super::protocol::parse_mtu_response;

/// `AF_BLUETOOTH`. Not in `libc`'s constants for every target, so it is spelled out.
pub const AF_BLUETOOTH: libc::c_int = 31;
/// `BTPROTO_L2CAP`.
pub const BTPROTO_L2CAP: libc::c_int = 0;
/// The one CID the kernel lets userspace use on LE: the ATT channel.
pub const ATT_CID: u16 = 4;
/// `l2_bdaddr_type` values.
pub const ADDR_TYPE_LE_PUBLIC: u8 = 1;
pub const ADDR_TYPE_LE_RANDOM: u8 = 2;

/// The ATT default, and the floor any negotiation falls back to.
pub const ATT_DEFAULT_MTU: usize = 23;
/// What we ask for. 517 is the ATT maximum.
pub const CLIENT_RX_MTU: u16 = 517;

/// v1 had no socket read timeout at all; the plan's `session_timeout_s` needs a bearer-level bound
/// or a silent beacon wedges the blocking thread until the job deadline.
pub const RECV_TIMEOUT: Duration = Duration::from_secs(10);

const RECV_BUF: usize = 1024;

/// ATT opcodes used here.
const OP_EXCHANGE_MTU_REQ: u8 = 0x02;
const OP_READ_BY_TYPE_REQ: u8 = 0x08;
const OP_READ_BY_TYPE_RSP: u8 = 0x09;
const OP_READ_REQ: u8 = 0x0A;
const OP_READ_RSP: u8 = 0x0B;
const OP_READ_BLOB_REQ: u8 = 0x0C;
const OP_WRITE_REQ: u8 = 0x12;
const OP_ERROR_RSP: u8 = 0x01;
/// The characteristic-declaration type, `0x2803`, little-endian on the wire.
const TYPE_CHARACTERISTIC: [u8; 2] = [0x03, 0x28];

/// The kernel's `struct sockaddr_l2`.
///
/// `#[repr(C, packed)]` matters: the natural alignment of `l2_cid` would otherwise insert padding
/// after the 6-byte address and the kernel would read the wrong field.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct SockAddrL2 {
    l2_family: u16,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

impl SockAddrL2 {
    fn new(bdaddr: [u8; 6], bdaddr_type: u8) -> SockAddrL2 {
        SockAddrL2 {
            l2_family: AF_BLUETOOTH as u16,
            // PSM 0: a CID-addressed channel has no PSM, and a non-zero one is a privileged bind.
            l2_psm: 0,
            l2_bdaddr: bdaddr,
            l2_cid: ATT_CID,
            l2_bdaddr_type: bdaddr_type,
        }
    }
}

/// A connected ATT bearer. Blocking; the socket is closed on `Drop`.
#[derive(Debug)]
pub struct Att {
    fd: RawFd,
    mtu: usize,
}

impl Att {
    /// Open a socket, bind it to the local ATT channel and connect to `mac`.
    ///
    /// The address goes into `sockaddr_l2` **byte-reversed** — the kernel wants it little-endian,
    /// while every human-facing spelling is big-endian. Getting this wrong produces `EHOSTUNREACH`
    /// against a beacon that is sitting right there.
    pub fn connect(mac: &MacAddr, addr_type: u8) -> io::Result<Att> {
        // SAFETY: `socket` takes three integers and returns a fd or -1; no pointers involved.
        let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET, BTPROTO_L2CAP) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let att = Att {
            fd,
            mtu: ATT_DEFAULT_MTU,
        };

        att.set_recv_timeout(RECV_TIMEOUT)?;

        let local = SockAddrL2::new([0u8; 6], addr_type);
        // SAFETY: `local` outlives the call, and the length passed is exactly its size, so the
        // kernel reads only initialised bytes of a correctly-shaped sockaddr.
        let rc = unsafe {
            libc::bind(
                att.fd,
                std::ptr::addr_of!(local).cast::<libc::sockaddr>(),
                std::mem::size_of::<SockAddrL2>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut octets = mac.octets();
        octets.reverse();
        let peer = SockAddrL2::new(octets, addr_type);
        // SAFETY: same contract as the bind above — a live, fully-initialised sockaddr and its
        // exact length.
        let rc = unsafe {
            libc::connect(
                att.fd,
                std::ptr::addr_of!(peer).cast::<libc::sockaddr>(),
                std::mem::size_of::<SockAddrL2>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(att)
    }

    fn set_recv_timeout(&self, d: Duration) -> io::Result<()> {
        let tv = libc::timeval {
            tv_sec: d.as_secs() as libc::time_t,
            tv_usec: d.subsec_micros() as libc::suseconds_t,
        };
        // SAFETY: `tv` is a live, fully-initialised `timeval` and the length matches its type,
        // which is what `SO_RCVTIMEO` expects.
        let rc = unsafe {
            libc::setsockopt(
                self.fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                std::ptr::addr_of!(tv).cast::<libc::c_void>(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub const fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn send(&self, pdu: &[u8]) -> io::Result<()> {
        // SAFETY: the pointer and length describe `pdu` exactly, and the kernel only reads from it.
        let n = unsafe { libc::send(self.fd, pdu.as_ptr().cast::<libc::c_void>(), pdu.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize != pdu.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short ATT write on a SEQPACKET socket",
            ));
        }
        Ok(())
    }

    pub fn recv(&self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; RECV_BUF];
        // SAFETY: the pointer and length describe `buf`'s allocation exactly; the kernel writes at
        // most `len` bytes and returns how many, and the vector is truncated to that below.
        let n = unsafe {
            libc::recv(
                self.fd,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// Send a request and read its response.
    pub fn req(&self, pdu: &[u8]) -> io::Result<Vec<u8>> {
        self.send(pdu)?;
        self.recv()
    }

    /// Exchange MTU. Must be the first thing on the bearer, before any read sized against it.
    pub fn exchange_mtu(&mut self) -> io::Result<usize> {
        let mut pdu = vec![OP_EXCHANGE_MTU_REQ];
        pdu.extend_from_slice(&CLIENT_RX_MTU.to_le_bytes());
        let rsp = self.req(&pdu)?;
        self.mtu = parse_mtu_response(&rsp);
        Ok(self.mtu)
    }

    pub fn write_req(&self, handle: u16, value: &[u8]) -> io::Result<Vec<u8>> {
        let mut pdu = vec![OP_WRITE_REQ];
        pdu.extend_from_slice(&handle.to_le_bytes());
        pdu.extend_from_slice(value);
        self.req(&pdu)
    }

    pub fn read_req(&self, handle: u16) -> io::Result<Vec<u8>> {
        let mut pdu = vec![OP_READ_REQ];
        pdu.extend_from_slice(&handle.to_le_bytes());
        let rsp = self.req(&pdu)?;
        Ok(value_of_read_rsp(&rsp))
    }

    fn read_blob(&self, handle: u16, offset: u16) -> io::Result<Vec<u8>> {
        let mut pdu = vec![OP_READ_BLOB_REQ];
        pdu.extend_from_slice(&handle.to_le_bytes());
        pdu.extend_from_slice(&offset.to_le_bytes());
        let rsp = self.req(&pdu)?;
        Ok(value_of_read_rsp(&rsp))
    }

    /// A long read: the first Read Request, then Read Blob until a slice comes back short.
    ///
    /// The continuation rule is "keep going while the previous slice filled the payload" — an
    /// `mtu − 1` byte slice means there is probably more.
    pub fn read_long(&self, handle: u16) -> io::Result<Vec<u8>> {
        let payload = self.mtu.saturating_sub(1).max(1);
        let mut out = self.read_req(handle)?;
        while out.len() % payload == 0 && !out.is_empty() {
            let offset = match u16::try_from(out.len()) {
                Ok(o) => o,
                // Past what a 16-bit offset can address: stop rather than wrap around and re-read
                // the start of the value as though it were the end.
                Err(_) => break,
            };
            let more = self.read_blob(handle, offset)?;
            if more.is_empty() {
                break;
            }
            out.extend_from_slice(&more);
        }
        Ok(out)
    }

    /// Walk the whole attribute database for characteristic declarations.
    ///
    /// Discovery is over `0x0001..=0xFFFF` and **by UUID**, never by handle: the same characteristic
    /// sits at different handles on the recorder and standard variants, and a handle-indexed write
    /// silently hits the wrong attribute on the others.
    pub fn read_by_type_chars(&self) -> io::Result<Vec<CharDecl>> {
        let mut out = Vec::new();
        let mut start: u16 = 0x0001;
        loop {
            let mut pdu = vec![OP_READ_BY_TYPE_REQ];
            pdu.extend_from_slice(&start.to_le_bytes());
            pdu.extend_from_slice(&0xFFFFu16.to_le_bytes());
            pdu.extend_from_slice(&TYPE_CHARACTERISTIC);
            let rsp = self.req(&pdu)?;

            let Some(found) = parse_read_by_type_rsp(&rsp) else {
                // `0x01` (error) is the documented end of the walk, not a failure.
                return Ok(out);
            };
            if found.is_empty() {
                return Ok(out);
            }
            let last = found.iter().map(|c| c.decl_handle).max().unwrap_or(start);
            out.extend(found);
            match last.checked_add(1) {
                Some(next) if next > start => start = next,
                // Either we wrapped at 0xFFFF or the peer stopped advancing; either way, stop.
                _ => return Ok(out),
            }
        }
    }
}

impl Drop for Att {
    fn drop(&mut self) {
        // SAFETY: `self.fd` came from `socket` and is closed exactly once, here.
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// One characteristic declaration from a Read By Type response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharDecl {
    pub decl_handle: u16,
    pub properties: u8,
    pub value_handle: u16,
    /// The 16 UUID bytes, **in the little-endian order the wire carries them**.
    pub uuid_le: [u8; 16],
}

impl CharDecl {
    /// The UUID in normal (big-endian) order.
    pub fn uuid(&self) -> bluer::Uuid {
        let mut be = self.uuid_le;
        be.reverse();
        bluer::Uuid::from_bytes(be)
    }
}

/// The value bytes of a Read Response / Read Blob Response, or empty for anything else.
fn value_of_read_rsp(rsp: &[u8]) -> Vec<u8> {
    match rsp.split_first() {
        Some((&OP_READ_RSP, rest)) => rest.to_vec(),
        _ => Vec::new(),
    }
}

/// `0000xxxx-0000-1000-8000-00805f9b34fb` in wire (little-endian) order, `xxxx` at bytes 12..14.
const BASE_UUID_LE: [u8; 16] = [
    0xFB, 0x34, 0x9B, 0x5F, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Parse a Read By Type response into declarations.
///
/// `None` means "not a Read By Type response" — an `0x01` error response is how the peer says the
/// walk is finished, so the caller treats it as the end rather than as a fault.
pub fn parse_read_by_type_rsp(rsp: &[u8]) -> Option<Vec<CharDecl>> {
    let (&op, rest) = rsp.split_first()?;
    if op == OP_ERROR_RSP {
        return None;
    }
    if op != OP_READ_BY_TYPE_RSP {
        return None;
    }
    let (&len, records) = rest.split_first()?;
    // A declaration is 2 + 1 + 2 bytes plus the UUID: 21 bytes with a 128-bit UUID, 7 with a 16-bit
    // one. One response carries one length only, so a page of 16-bit declarations comes before the
    // 128-bit ones and must be consumed too: an empty answer would end the walk there, before it
    // reaches the recorder's characteristics (2026-10-09, 7C:D9:F4:10:6E:30 at MTU 247 — all ten
    // 16-bit declarations fit the first page). A 16-bit UUID is widened onto the Bluetooth base
    // UUID, so it can never match the 128-bit ones this component looks for.
    let len = len as usize;
    if len != 21 && len != 7 {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for rec in records.chunks_exact(len) {
        let mut uuid_le = BASE_UUID_LE;
        if len == 21 {
            uuid_le.copy_from_slice(&rec[5..21]);
        } else {
            uuid_le[12] = rec[5];
            uuid_le[13] = rec[6];
        }
        out.push(CharDecl {
            decl_handle: u16::from_le_bytes([rec[0], rec[1]]),
            properties: rec[2],
            value_handle: u16::from_le_bytes([rec[3], rec[4]]),
            uuid_le,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sockaddr_is_packed_to_the_kernel_layout() {
        // 2 + 2 + 6 + 2 + 1 = 13. Natural alignment would pad this to 14 and shift `l2_cid`, so the
        // kernel would read the wrong CID and the bind would be rejected — or worse, accepted.
        assert_eq!(std::mem::size_of::<SockAddrL2>(), 13);
    }

    #[test]
    fn the_address_goes_on_the_wire_byte_reversed() {
        let mac = MacAddr::parse("7C:D9:F4:13:10:DE").expect("mac");
        let mut octets = mac.octets();
        octets.reverse();
        assert_eq!(
            octets,
            [0xDE, 0x10, 0x13, 0xF4, 0xD9, 0x7C],
            "little-endian in sockaddr_l2, big-endian everywhere a human reads it"
        );
        let sa = SockAddrL2::new(octets, ADDR_TYPE_LE_PUBLIC);
        // Field reads go through locals because the struct is packed.
        let family = sa.l2_family;
        let psm = sa.l2_psm;
        let cid = sa.l2_cid;
        let ty = sa.l2_bdaddr_type;
        assert_eq!(family, AF_BLUETOOTH as u16);
        assert_eq!(psm, 0, "a CID-addressed channel has no PSM");
        assert_eq!(cid, ATT_CID);
        assert_eq!(ty, ADDR_TYPE_LE_PUBLIC);
    }

    #[test]
    fn a_read_response_yields_its_value_and_anything_else_yields_nothing() {
        assert_eq!(value_of_read_rsp(&[0x0B, 1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(value_of_read_rsp(&[0x0B]), Vec::<u8>::new());
        // An error response is not a zero-length value.
        assert_eq!(
            value_of_read_rsp(&[0x01, 0x0A, 0x00, 0x00, 0x0A]),
            Vec::<u8>::new()
        );
        assert_eq!(value_of_read_rsp(&[]), Vec::<u8>::new());
    }

    #[test]
    fn read_by_type_parses_the_declaration_records() {
        // Two 128-bit declarations, 21 bytes each.
        let mut uuid_a = [0u8; 16];
        uuid_a[15] = 0xE6;
        uuid_a[14] = 0x1C;
        uuid_a[12] = 0x01;
        let mut rsp = vec![OP_READ_BY_TYPE_RSP, 21];
        // decl 0x0020, props 0x0A, value 0x0021, uuid
        rsp.extend_from_slice(&0x0020u16.to_le_bytes());
        rsp.push(0x0A);
        rsp.extend_from_slice(&0x0021u16.to_le_bytes());
        rsp.extend_from_slice(&uuid_a);
        // decl 0x0030, props 0x08, value 0x0031, same uuid shape
        rsp.extend_from_slice(&0x0030u16.to_le_bytes());
        rsp.push(0x08);
        rsp.extend_from_slice(&0x0031u16.to_le_bytes());
        rsp.extend_from_slice(&uuid_a);

        let decls = parse_read_by_type_rsp(&rsp).expect("is a read-by-type response");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].decl_handle, 0x0020);
        assert_eq!(decls[0].properties, 0x0A);
        assert_eq!(decls[0].value_handle, 0x0021);
        assert_eq!(decls[1].value_handle, 0x0031);
        assert_eq!(
            decls[0].uuid_le[15], 0xE6,
            "the discriminator bytes sit at the top of the LE array"
        );
    }

    #[test]
    fn an_error_response_ends_the_walk_rather_than_failing_it() {
        assert!(parse_read_by_type_rsp(&[OP_ERROR_RSP, 0x08, 0x01, 0x00, 0x0A]).is_none());
        assert!(parse_read_by_type_rsp(&[]).is_none());
        assert!(parse_read_by_type_rsp(&[OP_READ_RSP, 1, 2]).is_none());
    }

    #[test]
    fn a_sixteen_bit_uuid_page_is_consumed_so_the_walk_goes_on() {
        // 7-byte records are the 16-bit UUID form. They must come back as declarations: the walk
        // advances past the last one, and an empty page would end it before the 128-bit
        // characteristics (the 2026-10-09 recorder failure).
        let rsp = vec![
            OP_READ_BY_TYPE_RSP,
            7,
            0x20,
            0x00,
            0x0A,
            0x21,
            0x00,
            0x00,
            0x2A,
            0x22,
            0x00,
            0x02,
            0x23,
            0x00,
            0x01,
            0x2A,
        ];
        let decls = parse_read_by_type_rsp(&rsp).expect("is a response");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[1].decl_handle, 0x0022);
        assert_eq!(decls[1].value_handle, 0x0023);
        assert_eq!(
            decls[0].uuid(),
            bluer::Uuid::from_u128(0x00002a00_0000_1000_8000_00805f9b34fb),
            "a 16-bit UUID is widened onto the Bluetooth base UUID"
        );
    }

    #[test]
    fn an_unknown_record_length_yields_no_declarations() {
        let rsp = vec![
            OP_READ_BY_TYPE_RSP,
            9,
            0x20,
            0x00,
            0x0A,
            0x21,
            0x00,
            1,
            2,
            3,
            4,
        ];
        assert!(parse_read_by_type_rsp(&rsp)
            .expect("is a response")
            .is_empty());
    }

    #[test]
    fn the_uuid_is_reversed_back_for_matching() {
        // The wire order is little-endian; the constants in provision.rs are written the human way.
        let target = bluer::Uuid::from_u128(0xe61c0001_7df8_4d4e_8e6d_c611745b92e9);
        let mut le = *target.as_bytes();
        le.reverse();
        let d = CharDecl {
            decl_handle: 1,
            properties: 0,
            value_handle: 2,
            uuid_le: le,
        };
        assert_eq!(d.uuid(), target);
        // §3's discriminator table, checked against a real recorder UUID.
        assert_eq!(d.uuid_le[15], 0xE6);
        assert_eq!(d.uuid_le[14], 0x1C);
        assert_eq!(d.uuid_le[13], 0x00);
        assert_eq!(d.uuid_le[12], 0x01);
        assert_eq!(d.uuid_le[11], 0x7D);
        assert_eq!(d.uuid_le[10], 0xF8, "0xF8 is the recorder service");
    }

    #[test]
    fn the_mtu_request_is_the_documented_three_bytes() {
        let mut pdu = vec![OP_EXCHANGE_MTU_REQ];
        pdu.extend_from_slice(&CLIENT_RX_MTU.to_le_bytes());
        assert_eq!(pdu, vec![0x02, 0x05, 0x02], "02 05 02");
    }

    #[test]
    fn the_pdu_builders_use_little_endian_handles() {
        // Not exercised through a socket here — the byte layout is the part worth pinning.
        let mut write = vec![OP_WRITE_REQ];
        write.extend_from_slice(&0x0037u16.to_le_bytes());
        write.extend_from_slice(b"123456");
        assert_eq!(&write[..3], &[0x12, 0x37, 0x00]);

        let mut read = vec![OP_READ_REQ];
        read.extend_from_slice(&0x0021u16.to_le_bytes());
        assert_eq!(read, vec![0x0A, 0x21, 0x00]);

        let mut blob = vec![OP_READ_BLOB_REQ];
        blob.extend_from_slice(&0x0021u16.to_le_bytes());
        blob.extend_from_slice(&0x0016u16.to_le_bytes());
        assert_eq!(blob, vec![0x0C, 0x21, 0x00, 0x16, 0x00]);

        let mut by_type = vec![OP_READ_BY_TYPE_REQ];
        by_type.extend_from_slice(&0x0001u16.to_le_bytes());
        by_type.extend_from_slice(&0xFFFFu16.to_le_bytes());
        by_type.extend_from_slice(&TYPE_CHARACTERISTIC);
        assert_eq!(
            by_type,
            vec![0x08, 0x01, 0x00, 0xFF, 0xFF, 0x03, 0x28],
            "08 <start LE> <end LE> 03 28"
        );
    }

    #[test]
    fn the_constants_are_the_kernel_and_att_ones() {
        assert_eq!(AF_BLUETOOTH, 31);
        assert_eq!(BTPROTO_L2CAP, 0);
        assert_eq!(ATT_CID, 4);
        assert_eq!(ADDR_TYPE_LE_PUBLIC, 1);
        assert_eq!(ADDR_TYPE_LE_RANDOM, 2);
        assert_eq!(ATT_DEFAULT_MTU, 23);
        assert_eq!(CLIENT_RX_MTU, 517);
        assert_eq!(RECV_TIMEOUT, Duration::from_secs(10));
    }
}
