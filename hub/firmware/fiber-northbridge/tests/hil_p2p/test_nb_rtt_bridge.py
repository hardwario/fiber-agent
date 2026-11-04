#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Offline tests for the one piece of logic in `nb_rtt_bridge.py`: framing.

The bridge is a byte pipe, so almost nothing in it can be got wrong — except deciding where one
HDLC frame ends and the next begins. That decision has to be right: half a frame injected into
the modem's decoder is garbage, and two frames in one `p2p_inject` make the second unreachable.
Everything else in the bridge needs a probe and a target; this does not.

    python3 -m pytest test_nb_rtt_bridge.py -v
"""
import importlib.util
import sys
import types

# `nb_rtt_bridge` imports pylink at module scope for the RTT half, which these tests never touch.
# Stubbing it keeps the framing testable on a machine with no J-Link tooling installed.
if "pylink" not in sys.modules:
    stub = types.ModuleType("pylink")
    stub.JLink = object
    stub.enums = types.SimpleNamespace(JLinkInterfaces=types.SimpleNamespace(SWD=1))
    sys.modules["pylink"] = stub

_spec = importlib.util.spec_from_file_location(
    "nb_rtt_bridge", __file__.rsplit("/", 1)[0] + "/nb_rtt_bridge.py")
nb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(nb)

FLAG = b"\x7e"


def framed(payload: bytes) -> bytes:
    """`7E <escaped payload> 7E`, as `hdlc_encode()` and the Rust `hdlc::encode` both emit."""
    return FLAG + payload + FLAG


def test_a_whole_frame_is_peeled_off():
    frames, rest = nb.split_frames(bytearray(framed(b"\x01\x02\x03")))
    assert frames == [framed(b"\x01\x02\x03")]
    assert rest == b""


def test_two_frames_arriving_in_one_read_both_come_out():
    frames, rest = nb.split_frames(bytearray(framed(b"\xaa") + framed(b"\xbb\xcc")))
    assert frames == [framed(b"\xaa"), framed(b"\xbb\xcc")]
    assert rest == b""


def test_a_frame_split_across_reads_waits_rather_than_injecting_half():
    buf = bytearray(b"\x7e\x01\x02")
    frames, rest = nb.split_frames(buf)
    assert frames == [], "half a frame must never be injected"
    assert rest == b"\x7e\x01\x02", "and must be kept for the next read"

    rest.extend(b"\x03\x7e")
    frames, rest = nb.split_frames(rest)
    assert frames == [framed(b"\x01\x02\x03")]
    assert rest == b""


def test_noise_before_the_first_flag_is_discarded():
    # The realistic case is a desync: the bridge attaches, or `control-radio` restarts, part way
    # through a frame, leaving a tail with no opening flag. It is dropped, not injected.
    frames, rest = nb.split_frames(bytearray(b"\x11\x22\x33" + framed(b"\x05")))
    assert frames == [framed(b"\x05")]
    assert rest == b""


def test_shell_text_is_not_this_directions_problem():
    """Only the RTT side carries text, and that is the regex's job — which matters because the
    prompt contains a tilde.

    `CONFIG_SHELL_PROMPT_RTT="north:~$ "` includes `~` = 0x7E, so shell output fed through
    `split_frames` would look like flags. It never is: `split_frames` only ever sees the PTY
    master, and the only thing writing there is `control-radio`, which emits HDLC and nothing
    else. Text arrives on the RTT side, where `P2P_OUT_RE` picks out the frames and ignores
    everything around them.
    """
    assert b"~" in b"north:~$ " and ord("~") == nb.HDLC_FLAG
    assert nb.P2P_OUT_RE.search(b"north:~$ p2p_status") is None


def test_a_repeated_flag_does_not_produce_an_empty_frame():
    frames, rest = nb.split_frames(bytearray(b"\x7e\x7e\x11\x7e"))
    assert frames == [framed(b"\x11")]
    assert rest == b""


def test_a_run_longer_than_the_wire_allows_is_dropped_and_resyncs():
    # A desync, not a frame: injecting it would corrupt the modem's decoder state, and the
    # central retries anyway. The next real frame must still parse.
    buf = bytearray(b"\x7e" + b"\x00" * (nb.MAX_ENCODED_LEN + 10) + b"\x7e" + framed(b"\x09"))
    frames, rest = nb.split_frames(buf)
    assert frames == [framed(b"\x09")]
    assert rest == b""


def test_escaped_payload_bytes_pass_through_untouched():
    # 0x7E and 0x7D inside a payload arrive escaped, so the splitter must not see them as flags
    # and must not unescape anything — the modem's own decoder does that.
    body = bytes([0x7D, 0x5E, 0x7D, 0x5D])
    frames, rest = nb.split_frames(bytearray(framed(body)))
    assert frames == [framed(body)]
    assert rest == b""


def test_the_p2p_out_pattern_matches_the_firmware_line():
    # uart_link.c writes `P2P_OUT <hex>` with no prefix; nb_hil.py parses the same shape.
    line = b"P2P_OUT 7e41000102bbcc7e"
    m = nb.P2P_OUT_RE.search(line)
    assert m and bytes.fromhex(m.group(1).decode())[0] == 0x7E
    assert nb.P2P_OUT_RE.search(b"OK: injected 12 bytes") is None
