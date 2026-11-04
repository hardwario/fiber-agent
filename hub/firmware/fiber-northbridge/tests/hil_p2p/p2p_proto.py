#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
Pure-logic half of the two-probe P2P HIL harness (no hardware, no pylink).

Two layers, both host-side:

  1. The **host<->modem** protocol this northbridge firmware speaks
     (HDLC framing + the GET_INFO/SET_RADIO_CONFIG/RX_START/TX_SCHEDULE
     command set and EVT_* events). Spec: proximos-v2
     plan/system/lora/design.md sec 3-4, mirrored in app/src/proto.h.

  2. The **P2P over-the-air** protocol the *central* runs against a STICKER
     device under test -- join tag/session-key/data-plane crypto. This mirrors
     sticker-firmware app/src/app_p2p.c + doc/p2p.md; the northbridge itself is
     a dumb keyless modem and does none of it, so the whole central lives here.

This module is deliberately hardware-free so it can be unit-tested offline in
CI against sticker-firmware's own known-answer vectors (see test_p2p_proto.py).
nb_hil.py adds the RTT transport and drives a real northbridge with it.

Requires pycryptodome (Crypto.Hash.CMAC, Crypto.Cipher.AES).
"""
from __future__ import annotations
import struct
from Crypto.Hash import CMAC
from Crypto.Cipher import AES

# ---------------------------------------------------------------------------
# host<->modem protocol constants (app/src/proto.h)
# ---------------------------------------------------------------------------
HDLC_FLAG, HDLC_ESC, HDLC_ESC_XOR = 0x7E, 0x7D, 0x20

CMD_GET_INFO, CMD_SET_RADIO_CONFIG = 0x01, 0x02
CMD_RX_START, CMD_RX_STOP = 0x03, 0x04
CMD_TX_SCHEDULE, CMD_TX_CANCEL = 0x05, 0x06
RSP_BIT = 0x40

EVT_BOOT, EVT_RX, EVT_TX_DONE, EVT_LOG = 0x80, 0x81, 0x82, 0x83

BW_125K, BW_250K, BW_500K = 0, 1, 2
TX_IMMEDIATE = 0

# ---------------------------------------------------------------------------
# P2P over-the-air protocol constants (app_p2p.c / doc/p2p.md)
# ---------------------------------------------------------------------------
P2P_HDR_LEN = 11
P2P_JOIN_TAG_LEN = 16   # full CMAC output for the join handshake
P2P_CCM_TAG_LEN = 4     # data-plane CCM tag
P2P_NONCE_LEN = 13
DIR_TX, DIR_RX = 0x00, 0x01   # device->network / network->device

FRAME_JOIN_REQUEST, FRAME_JOIN_ACCEPT = 0xF0, 0xF1
FRAME_TELEMETRY, FRAME_ALARM = 0x02, 0x03
FRAME_RESPONSE, FRAME_COMMAND, FRAME_ACK = 0x55, 0x56, 0xFA

JOIN_TAG_LABEL = b"HIO-P2P-JOIN"
JOINACCEPT_TAG_LABEL = b"HIO-P2P-ACC"
SESSION_KEY_LABEL = b"HIO-P2P-SES"

JOIN_REQUEST_BODY_LEN = 14   # product(1) proto(1) dev_eui(8 MSB-first) fw(4), #417/GitLab#73
DEV_EUI_LEN = 8
JOIN_ACCEPT_BODY_LEN = 15    # net_id(4) dev_addr(2) central_nonce(4) rx1(1) rsv(4)

ACK_FLAG_PENDING = 0x01
ACK_FLAG_TIME = 0x02


# ===========================================================================
# HDLC framing (CRC-16/CCITT-FALSE, poly 0x1021, init 0xFFFF, no reflection)
# ===========================================================================
def crc16_ccitt_false(data: bytes) -> int:
    crc = 0xFFFF
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if (crc & 0x8000) else (crc << 1) & 0xFFFF
    return crc


def hdlc_encode(ftype: int, seq: int, body: bytes) -> bytes:
    payload = bytes([ftype, seq]) + body
    payload += struct.pack("<H", crc16_ccitt_false(payload))
    out = bytearray([HDLC_FLAG])
    for b in payload:
        if b in (HDLC_FLAG, HDLC_ESC):
            out += bytes([HDLC_ESC, b ^ HDLC_ESC_XOR])
        else:
            out.append(b)
    out.append(HDLC_FLAG)
    return bytes(out)


def hdlc_decode_frames(raw: bytes):
    """Yield (type, seq, body) for every complete, CRC-valid frame in raw."""
    frame = bytearray()
    in_frame = False
    escape = False
    for b in raw:
        if b == HDLC_FLAG:
            if len(frame) >= 4:
                if crc16_ccitt_false(frame[:-2]) == struct.unpack("<H", frame[-2:])[0]:
                    yield frame[0], frame[1], bytes(frame[2:-2])
            frame.clear()
            in_frame = True
            escape = False
            continue
        if not in_frame:
            continue
        if b == HDLC_ESC:
            escape = True
            continue
        if escape:
            b ^= HDLC_ESC_XOR
            escape = False
        frame.append(b)


# ===========================================================================
# host -> modem command encoders / modem -> host event parsers
# ===========================================================================
def enc_get_info(seq=0):
    return hdlc_encode(CMD_GET_INFO, seq, b"")


def enc_set_radio_config(freq_hz, sf, bw, cr, preamble, tx_power_dbm, flags, seq=0):
    body = struct.pack("<IBBBHbB", freq_hz, sf, bw, cr, preamble, tx_power_dbm, flags)
    return hdlc_encode(CMD_SET_RADIO_CONFIG, seq, body)


def enc_rx_start(seq=0):
    return hdlc_encode(CMD_RX_START, seq, b"")


def enc_rx_stop(seq=0):
    return hdlc_encode(CMD_RX_STOP, seq, b"")


def enc_tx_schedule(tx_id, t_ms, frame: bytes, seq=0):
    body = struct.pack("<BQB", tx_id, t_ms, len(frame)) + frame
    return hdlc_encode(CMD_TX_SCHEDULE, seq, body)


def parse_evt_rx(body: bytes):
    """EVT_RX body: t_ms(8) rssi(2,i16) snr(1,i8) reserved(1) len(1) frame[len]."""
    t_ms, rssi, snr, _rsvd, ln = struct.unpack_from("<QhbBB", body, 0)
    frame = body[13:13 + ln]
    return {"t_ms": t_ms, "rssi": rssi, "snr": snr, "frame": frame}


def parse_evt_tx_done(body: bytes):
    tx_id, status, t_actual = struct.unpack("<BBQ", body)
    return {"tx_id": tx_id, "status": status, "t_actual_ms": t_actual}


def parse_evt_boot(body: bytes):
    return {"proto_ver": body[0], "fw": tuple(body[1:4]), "reset_cause": body[4]}


# ===========================================================================
# P2P crypto + over-the-air frame construction (the central's job)
# ===========================================================================
def aes_cmac(key: bytes, msg: bytes) -> bytes:
    c = CMAC.new(key, ciphermod=AES)
    c.update(msg)
    return c.digest()


def p2p_header(net_id, dev_addr, frame_type, counter) -> bytes:
    return struct.pack(">IHBI", net_id, dev_addr, frame_type, counter)


def p2p_nonce(counter, dev_addr, frame_type, direction) -> bytes:
    return struct.pack(">IHBB", counter, dev_addr, frame_type, direction) + b"\x00" * 5


def session_key(app_key: bytes, dev_nonce, central_nonce, dev_eui: bytes) -> bytes:
    """CMAC(app_key, label | 0x01 | dev_nonce(4 BE) | central_nonce(4 BE) | dev_eui(8)) -> 32 B.

    The last field was serial(4 BE) until #417 / GitLab #73 made the DevEUI the join
    identity; the input went 24 -> 28 octets and is still zero-padded to 32, so it is
    still two CMAC blocks. `dev_eui` is MSB-first, exactly as the hex string reads --
    deliberately NOT LoRaWAN's LSB-first on-air order (decision D1).

    Strict about the length: a caller still passing the 4-byte serial must fail here
    rather than have it silently padded into a key four bytes different from the node's.
    """
    if len(dev_eui) != DEV_EUI_LEN:
        raise ValueError(f"dev_eui must be {DEV_EUI_LEN} bytes MSB-first, got {len(dev_eui)}")
    block = (SESSION_KEY_LABEL + b"\x01"
             + struct.pack(">II", dev_nonce, central_nonce) + dev_eui)
    block += b"\x00" * (32 - len(block))
    return aes_cmac(app_key, block)


def build_join_request(app_key: bytes, dev_eui: bytes, dev_nonce,
                       fw=(1, 4, 0), product_type=1, proto_version=1) -> bytes:
    """The 41-byte uplink a node sends -- the inverse of parse_join_request()."""
    if len(dev_eui) != DEV_EUI_LEN:
        raise ValueError(f"dev_eui must be {DEV_EUI_LEN} bytes MSB-first, got {len(dev_eui)}")
    header = p2p_header(0, 0, FRAME_JOIN_REQUEST, dev_nonce)
    body = (bytes([product_type, proto_version]) + dev_eui
            + bytes(tuple(fw) + (0,))[:4])
    return header + body + aes_cmac(app_key, JOIN_TAG_LABEL + header + body)


def parse_join_request(frame: bytes, app_key: bytes) -> dict:
    """Verify a JoinRequest (0xF0) and return its identity fields, or raise."""
    if len(frame) != P2P_HDR_LEN + JOIN_REQUEST_BODY_LEN + P2P_JOIN_TAG_LEN:
        raise ValueError(f"JoinRequest wrong length {len(frame)}")
    net_id, dev_addr, ftype, counter = struct.unpack_from(">IHBI", frame, 0)
    if ftype != FRAME_JOIN_REQUEST:
        raise ValueError(f"not a JoinRequest (type 0x{ftype:02x})")
    hdr_body = frame[:P2P_HDR_LEN + JOIN_REQUEST_BODY_LEN]
    tag = frame[P2P_HDR_LEN + JOIN_REQUEST_BODY_LEN:]
    if aes_cmac(app_key, JOIN_TAG_LABEL + hdr_body) != tag:
        raise ValueError("JoinRequest CMAC tag mismatch")
    body = frame[P2P_HDR_LEN:P2P_HDR_LEN + JOIN_REQUEST_BODY_LEN]
    product_type, proto_version, dev_eui = struct.unpack(">BB8s", body[:10])
    # fw_version is 4 bytes on the wire: major, minor, patch, reserved. `fw` is the
    # 3-part version; the reserved byte is returned separately rather than dropped,
    # so a bench comparison against the Rust central (whose JoinRequest carries the
    # raw 4-byte field) has something to compare.
    fw = tuple(body[10:13])
    return {"dev_nonce": counter, "product_type": product_type,
            "proto_version": proto_version, "dev_eui": dev_eui.hex(), "fw": fw,
            "fw_reserved": body[13]}


def build_join_accept(app_key, dev_nonce, net_id, dev_addr, central_nonce,
                      rx1_delay_s=1, reserved=b"\x00\x00\x00\x00") -> bytes:
    """The downlink the central schedules into the node's RX1 window."""
    header = p2p_header(0, 0, FRAME_JOIN_ACCEPT, dev_nonce)   # pre-join addr, ctr echoes dev_nonce
    body = struct.pack(">IHI", net_id, dev_addr, central_nonce) + bytes([rx1_delay_s]) + reserved
    tag = aes_cmac(app_key, JOINACCEPT_TAG_LABEL + header + body)
    return header + body + tag


def decrypt_data_frame(sess_key, frame: bytes, direction=DIR_TX) -> dict:
    """Authenticate + decrypt a data-plane frame (uplink from the DUT)."""
    net_id, dev_addr, ftype, counter = struct.unpack_from(">IHBI", frame, 0)
    header = frame[:P2P_HDR_LEN]
    ct = frame[P2P_HDR_LEN:-P2P_CCM_TAG_LEN]
    tag = frame[-P2P_CCM_TAG_LEN:]
    nonce = p2p_nonce(counter, dev_addr, ftype, direction)
    c = AES.new(sess_key, AES.MODE_CCM, nonce=nonce, mac_len=P2P_CCM_TAG_LEN)
    c.update(header)
    pt = c.decrypt_and_verify(ct, tag)   # raises ValueError on auth failure
    return {"net_id": net_id, "dev_addr": dev_addr, "frame_type": ftype,
            "counter": counter, "body": pt}


def build_data_frame(sess_key, net_id, dev_addr, frame_type, counter,
                     body: bytes, direction=DIR_RX) -> bytes:
    """Encrypt a downlink data-plane frame (e.g. ACK 0xFA, COMMAND 0x56)."""
    header = p2p_header(net_id, dev_addr, frame_type, counter)
    nonce = p2p_nonce(counter, dev_addr, frame_type, direction)
    c = AES.new(sess_key, AES.MODE_CCM, nonce=nonce, mac_len=P2P_CCM_TAG_LEN)
    c.update(header)
    ct, tag = c.encrypt_and_digest(body)
    return header + ct + tag


def build_ack(sess_key, net_id, dev_addr, counter, rssi, snr,
              pending=False, unix_time=None) -> bytes:
    """ACK (0xFA) echoing the uplink counter; B1 rssi/snr, optional B5 time tail."""
    flags = (ACK_FLAG_PENDING if pending else 0) | (ACK_FLAG_TIME if unix_time is not None else 0)
    body = struct.pack("bb", _clamp_i8(rssi), _clamp_i8(snr))
    body = bytes([flags]) + body
    if unix_time is not None:
        body += struct.pack(">I", unix_time)
    return build_data_frame(sess_key, net_id, dev_addr, FRAME_ACK, counter, body, DIR_RX)


def _clamp_i8(v: int) -> int:
    return max(-128, min(127, int(v)))
