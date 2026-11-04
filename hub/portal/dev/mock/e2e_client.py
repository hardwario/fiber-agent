#!/usr/bin/env python3
"""E2E client for the mock device's signed-command flow (M4-E, v2).

Plays the portal's role against `mock_publisher.py`: pairs (decrypts the CA
seed with the pairing code, cross-checks the derived public key and the admin
certificate signature), mints itself a certificate under that CA, and then
drives one verb per level to show the two shapes the v2 envelope has:

  * `get_sticker_info`   — `read`,   single-shot: a response and **no** challenge
  * `sticker_force_send` — `operate`, single-shot
  * `set_threshold`      — `manage`,  challenge -> confirm -> response
  * the four Beacon actions — `beacon-add/-provision/-history/-remove`
  * `add_eye_tag`        — refused, with the device's own wording
  * a duplicate challenge (mock run with `--inject-duplicate-challenge`)
    answered with exactly **one** confirm

Canonicalization is imported from the mock rather than re-derived: the two
files disagreeing about it is the one bug this client cannot detect, and
`mock_publisher.py --selftest` already pins that function against
`fiber-protocol/vectors/`.

Exits non-zero on the first failed check.

Usage:
    python e2e_client.py [--host localhost] [--port 1883]
                         [--pairing-code ABC123] [--hostname fiber-mock-001]
                         [--expect-duplicate-challenge]
"""

from __future__ import annotations

import argparse
import base64
import json
import queue
import secrets
import sys
import time
import uuid

import paho.mqtt.client as mqtt
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.pbkdf2 import PBKDF2HMAC

# The mock is the canonicalization authority here, and its `--selftest` is what
# proves that authority against the checked-in vectors. Importing keeps one
# implementation in this directory instead of two that can drift apart.
from mock_publisher import (
    CATALOGUE,
    COMMAND_SPECS,
    ENVELOPE_VERSION,
    LEVEL_PERMISSIONS,
    _canonical as canonical,
    _rfc3339 as rfc3339,
    confirm_preimage,
    request_preimage,
)

FAILURES: list[str] = []


def check(cond: bool, label: str, detail: str = ""):
    print(f"  [{'OK' if cond else 'FAIL'}] {label}" + (f" — {detail}" if detail else ""))
    if not cond:
        FAILURES.append(label)
        sys.exit(1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", type=int, default=1883)
    ap.add_argument("--prefix", default="proximos")
    ap.add_argument("--hostname", default="fiber-mock-001")
    ap.add_argument("--pairing-code", default="ABC123")
    ap.add_argument("--expect-duplicate-challenge", action="store_true",
                    help="Assert the mock redelivers each challenge (run it with "
                         "--inject-duplicate-challenge); this client then confirms every "
                         "frame and asserts the device answers one SUCCESS and refuses the "
                         "rest with ChallengeNotFound on errors — the device's half of "
                         "design.md §6.2, and why the portal keeps a seen-set.")
    args = ap.parse_args()

    t = lambda suffix: f"{args.prefix}/{args.hostname}/{suffix}"
    inbox: queue.Queue = queue.Queue()

    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="e2e-client")
    client.on_message = lambda _c, _u, m: inbox.put((m.topic, json.loads(m.payload.decode())))
    client.connect(args.host, args.port)
    client.loop_start()
    # `lorawan/sensors/#`, not `lorawan/sensors`: a `read`'s data lands on
    # `…/lorawan/sensors/{dev_eui}/info` (design.md §5.1's second publish), and
    # an exact-topic subscription one level up receives nothing. Until the
    # 2026-09-15 second review this list also carried `responses/sticker_info`,
    # a topic nothing has published since P4.
    for suffix in ("pair/response", "config/challenge", "config/response", "errors",
                   "lorawan/sensors/#", "eye/sensors"):
        client.subscribe(t(suffix), qos=2)
    time.sleep(0.5)  # let SUBACKs land

    # What `wait_for` skipped on its way to the topic it wanted. Until the
    # second review it *dropped* those frames, which made every negative
    # assertion below unfalsifiable: a challenge the mock wrongly minted for
    # a `read` is published **before** the response, so `wait_for("config/response")`
    # ate it and `drain` saw nothing. Now the skipped frames are handed to the
    # next `drain`, and "no challenge" means no challenge.
    stray: list[tuple[str, dict]] = []

    def wait_for(suffix: str, timeout: float = 10.0, required: bool = True):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                topic, payload = inbox.get(timeout=max(0.05, deadline - time.time()))
            except queue.Empty:
                break
            if topic == t(suffix):
                return payload
            stray.append((topic, payload))
        if not required:
            return None
        print(f"  [FAIL] timed out waiting for {t(suffix)}")
        sys.exit(1)

    def drain(seconds: float = 1.0) -> list[tuple[str, dict]]:
        """Everything `wait_for` skipped so far plus everything that arrives in
        the next `seconds`. Used for the negative assertions — 'no challenge'
        cannot be proved by waiting for one."""
        seen, deadline = stray[:], time.time() + seconds
        stray.clear()
        while time.time() < deadline:
            try:
                seen.append(inbox.get(timeout=max(0.05, deadline - time.time())))
            except queue.Empty:
                break
        return seen

    # ---- 1. Pairing ----------------------------------------------------
    print("== Pairing ==")
    pair_request_id = str(uuid.uuid4())
    client.publish(t("pair/request"), json.dumps({
        "request_id": pair_request_id,
        "timestamp": int(time.time()),
        "admin_username": "e2e-admin",
    }), qos=1)
    pr = wait_for("pair/response")
    check(pr.get("request_id") == pair_request_id, "request_id echoed")
    check(pr.get("success") is True, "success true")
    check(pr.get("ca_id") == "mock-ca-1", "ca_id", pr.get("ca_id", ""))

    epk = pr["encrypted_private_key"]
    kdf = PBKDF2HMAC(algorithm=hashes.SHA256(), length=32,
                     salt=base64.b64decode(epk["salt"]), iterations=480_000)
    key = kdf.derive(args.pairing_code.encode())
    ca_seed = AESGCM(key).decrypt(base64.b64decode(epk["nonce"]),
                                  base64.b64decode(epk["ciphertext"]), None)
    check(len(ca_seed) == 32, "decrypted plaintext is 32 bytes", str(len(ca_seed)))

    ca_key = Ed25519PrivateKey.from_private_bytes(ca_seed)
    ca_public = ca_key.public_key()
    derived_hex = ca_public.public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw).hex()
    check(derived_hex == pr["ca_public_key"], "derived CA pubkey matches ca_public_key", derived_hex)

    admin_cert = pr["admin_certificate"]
    cert_pre = {k: v for k, v in admin_cert.items() if k != "certificate_signature"}
    ca_public.verify(base64.b64decode(admin_cert["certificate_signature"]), canonical(cert_pre))
    check(True, "admin_certificate signature verifies against CA (device->client direction)")
    check(len(cert_pre) == 8, "admin_certificate has 8 signed fields", ",".join(sorted(cert_pre)))

    # ---- 2. Mint a user key + CA-issued certificate ---------------------
    print("== User certificate (issued with the paired CA key) ==")
    user_key = Ed25519PrivateKey.generate()
    user_pub_hex = user_key.public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw).hex()
    now = int(time.time())
    # An admin's certificate: every verb permission plus all five `level:*`
    # strings, derived from the fixture the way `perms.rs` derives them from the
    # registry (design.md §4.3). A hand-typed list would go stale on the next
    # verb, and step 8 would refuse it with no obvious cause.
    user_cert = {
        "signer_id": "e2e-user",
        "full_name": "E2E User",
        "role": "admin",
        "public_key_ed25519": user_pub_hex,
        "permissions": sorted(
            {c["permission"] for c in CATALOGUE["commands"]} | set(LEVEL_PERMISSIONS.values())
        ),
        # RFC3339 strings — `crypto/certificate.rs` declares them `String` and
        # `parse_certificate` refuses the frame before the envelope is read
        # otherwise. Integers here made every message this client sent
        # unreadable to a real Hub while the mock accepted them all.
        "issued_at": rfc3339(now),
        "expires_at": rfc3339(now + 3600),
        "issuer": "device_ca",
    }
    user_cert["certificate_signature"] = base64.b64encode(
        ca_key.sign(canonical(user_cert))).decode()
    print(f"  user key {user_pub_hex[:16]}… certified for "
          f"{len(user_cert['permissions'])} permissions")

    # ---- signing helpers -------------------------------------------------
    def sign_request(command_type: str, params: dict, reason: str = "") -> dict:
        """The portal's `build_request`: the level comes from the catalogue,
        never from the caller (design.md §4.4 step 4)."""
        wire = {
            "command": "config_request",
            "v": ENVELOPE_VERSION,
            "command_type": command_type,
            "level": COMMAND_SPECS[command_type]["level"],
            "nonce": secrets.token_hex(16),
            "params": params,
            "reason": reason,
            "request_id": str(uuid.uuid4()),
            "signer_id": "e2e-user",
            "timestamp": int(time.time()),
        }
        wire["signature"] = base64.b64encode(
            user_key.sign(canonical(request_preimage(wire)))).decode()
        wire["certificate"] = user_cert
        return wire

    def sign_confirm(challenge_id: str, level: str, command_type: str) -> dict:
        wire = {
            "command": "config_confirm",
            "v": ENVELOPE_VERSION,
            "challenge_id": challenge_id,
            "confirmation": "APPROVED",
            "level": level,
            "nonce": secrets.token_hex(16),
            "signer_id": "e2e-user",
            "timestamp": int(time.time()),
            "command_type": command_type,   # informational, unsigned
        }
        wire["signature"] = base64.b64encode(
            user_key.sign(canonical(confirm_preimage(wire)))).decode()
        wire["certificate"] = user_cert
        return wire

    def single_shot(command_type: str, params: dict, label: str, data_topic: str | None = None):
        """A `read`/`operate` verb: one request, one response, **no** challenge
        (design.md §5.1). The negative half is the point — v1 minted a challenge
        for every verb and held a slot for 300 s per read. `data_topic` is the
        verb's own data publish, the second of §5.1's two device publishes,
        asserted when the caller names it."""
        print(f"== {command_type} ({COMMAND_SPECS[command_type]['level']}, single-shot) ==")
        drain(0.2)  # start from a clean slate: nothing stray counts against this verb
        wire = sign_request(command_type, params)
        client.publish(t("commands/config/request"), json.dumps(wire), qos=1)
        resp = wait_for("config/response")
        check(resp.get("request_id") == wire["request_id"], f"{label}: response echoes request_id")
        check(resp.get("status") == "SUCCESS", f"{label}: SUCCESS", resp.get("message", ""))
        check(isinstance(resp.get("applied_at"), int), f"{label}: applied_at set")
        extra = [topic for topic, _ in drain(0.8)]
        check(t("config/challenge") not in extra,
              f"{label}: no challenge was minted", ",".join(extra) or "nothing else")
        if data_topic is not None:
            check(t(data_topic) in extra, f"{label}: the data landed on {data_topic}",
                  ",".join(extra) or "nothing else")
        return resp

    def confirmed(command_type: str, params: dict, label: str, reason: str = ""):
        """`manage` and above: challenge -> confirm -> response."""
        spec = COMMAND_SPECS[command_type]
        print(f"== {command_type} ({spec['level']}, challenge/confirm) ==")
        drain(0.2)
        wire = sign_request(command_type, params, reason)
        client.publish(t("commands/config/request"), json.dumps(wire), qos=1)
        ch = wait_for("config/challenge")
        check(ch.get("request_id") == wire["request_id"], f"{label}: challenge echoes request_id")
        check(ch.get("signer_id") == "e2e-user", f"{label}: challenge carries signer_id")
        check(ch.get("preview", {}).get("command_type") == command_type,
              f"{label}: challenge preview names the verb")
        check(isinstance(ch.get("expires_at"), int) and ch["expires_at"] > time.time(),
              f"{label}: challenge expires_at in the future")

        # Every frame of this challenge the device sent: one, or — with the
        # mock's `--inject-duplicate-challenge` — two, the way rumqttc replays
        # one on reconnect.
        frames = 1 + sum(
            1 for topic, payload in drain(0.6)
            if topic == t("config/challenge") and payload.get("challenge_id") == ch["challenge_id"]
        )
        if args.expect_duplicate_challenge:
            check(frames >= 2, f"{label}: the mock redelivered the challenge", f"{frames} frame(s)")

        # This client deliberately does **not** model the portal's seen-set:
        # it confirms every frame it received, as a portal without design.md
        # §6.2 would. What is then asserted is the *device's* side of that
        # rule — one SUCCESS for the first confirm, and `Challenge not found`
        # on `errors` (and no second `config/response`) for each duplicate,
        # which is exactly why the portal needs the seen-set. Until the second
        # review this sent one confirm regardless and asserted one response —
        # a check that could not fail. The portal's own seen-set is pinned
        # where it lives, by `relay.rs::a_redelivered_challenge_produces_one_confirm`.
        for _ in range(frames):
            confirm = sign_confirm(ch["challenge_id"], spec["level"], command_type)
            client.publish(t("commands/config/confirm"), json.dumps(confirm), qos=1)
        resp = wait_for("config/response")
        check(resp.get("challenge_id") == ch["challenge_id"], f"{label}: response echoes challenge_id")
        check(resp.get("request_id") == wire["request_id"], f"{label}: response echoes request_id")
        check(resp.get("status") == "SUCCESS", f"{label}: SUCCESS", resp.get("message", ""))
        check(isinstance(resp.get("applied_at"), int), f"{label}: applied_at set")

        if frames > 1:
            after = drain(1.0)
            responses = [p for topic, p in after if topic == t("config/response")]
            refusals = [p for topic, p in after if topic == t("errors")
                        and p.get("message") == f"Challenge not found: {ch['challenge_id']}"]
            check(not responses, f"{label}: a duplicate confirm produced no second response",
                  f"{len(responses)} extra response(s)")
            check(len(refusals) == frames - 1,
                  f"{label}: each duplicate confirm was refused on errors with ChallengeNotFound",
                  f"{len(refusals)} refusal(s) for {frames - 1} duplicate(s)")
        return resp

    # ---- 3. read: single-shot -------------------------------------------
    dev_eui = "70b3d50000000000"
    single_shot("get_sticker_info", {"dev_eui": dev_eui}, "get_sticker_info",
                data_topic=f"lorawan/sensors/{dev_eui}/info")

    # ---- 4. operate: single-shot ----------------------------------------
    single_shot("sticker_force_send", {"dev_eui": dev_eui}, "sticker_force_send")

    # ---- 5. manage: challenge/confirm -----------------------------------
    # control-core's shape: the bounds under `thresholds` (the flat body is
    # refused, as the device refuses it).
    params = {"line": 0, "thresholds": {
        "critical_low": -30.0, "alarm_low": 1.5, "warning_low": 1.5,
        "warning_high": 7.5, "alarm_high": 7.5, "critical_high": 40.0}}
    confirmed("set_threshold", params, "set_threshold", reason="e2e threshold test")

    # The command took effect, not merely "the device said SUCCESS": the
    # retained `config/state` — republished on apply — carries the thresholds
    # under the device-side keys. The v1 client had this readback; the v2
    # rewrite dropped it, and the second review put it back.
    print("== config/state readback ==")
    client.subscribe(t("config/state"), qos=1)  # retained: the post-apply document
    cs = wait_for("config/state")
    line0 = next(s for s in cs["sensors"] if s["line"] == 0)
    th = line0["thresholds"]
    expected = {
        "critical_low_celsius": -30.0, "low_alarm_celsius": 1.5, "warning_low_celsius": 1.5,
        "warning_high_celsius": 7.5, "high_alarm_celsius": 7.5, "critical_high_celsius": 40.0,
    }
    check(th == expected, "line 0 thresholds carry the applied values", json.dumps(th))
    check(line0["has_override"] is True, "has_override true after set_threshold")

    # ---- 6. the four Beacon actions -------------------------------------
    mac = "AA:BB:CC:DD:EE:01"
    confirmed("beacon-add", {"mac": mac, "name": "E2E Beacon"}, "beacon-add")
    confirmed("beacon-provision", {"mac": mac}, "beacon-provision")
    single_shot("beacon-history", {"mac": mac}, "beacon-history")
    confirmed("beacon-remove", {"mac": mac}, "beacon-remove")

    # ---- 7. the device's refusal, word for word -------------------------
    #
    # `add_eye_tag` is not in the catalogue and has not been a verb the device
    # accepts since the rename. In v1 the mock answered SUCCESS to it — the
    # `_apply` fall-through — so the dev loop reported a green run for a command
    # no Hub would execute. This is the assertion that the fall-through now
    # fails, and fails the way a device fails.
    print("== add_eye_tag (withdrawn verb) ==")
    stale = {
        "command": "config_request",
        "v": ENVELOPE_VERSION,
        "command_type": "add_eye_tag",
        "level": "manage",
        "nonce": secrets.token_hex(16),
        "params": {"mac": mac},
        "reason": "",
        "request_id": str(uuid.uuid4()),
        "signer_id": "e2e-user",
        "timestamp": int(time.time()),
    }
    stale["signature"] = base64.b64encode(
        user_key.sign(canonical(request_preimage(stale)))).decode()
    stale["certificate"] = user_cert
    client.publish(t("commands/config/request"), json.dumps(stale), qos=1)
    err = wait_for("errors")
    check(err.get("error") == "authorization_failed",
          "add_eye_tag refused with the device's slug", str(err.get("error")))
    check(err.get("message") == "Invalid command: Unknown command type: add_eye_tag",
          "…and the device's exact wording", str(err.get("message")))
    nothing = [topic for topic, _ in drain(0.6)]
    check(t("config/challenge") not in nothing,
          "refused before any crypto — no challenge, no slot held")

    # ---- 8. a v1-shaped envelope is refused -----------------------------
    print("== v1-shaped envelope ==")
    v1 = sign_request("get_sticker_info", {"dev_eui": dev_eui})
    del v1["v"]
    del v1["level"]
    client.publish(t("commands/config/request"), json.dumps(v1), qos=1)
    err = wait_for("errors")
    check(err.get("error") == "unsupported_envelope",
          "a v1-shaped request is refused", str(err.get("error")))
    check(err.get("message") == f"v={ENVELOPE_VERSION} required",
          "…with design.md §3.2's message", str(err.get("message")))
    check(err.get("command") == "config_request",
          "…under the command §3.2 fixes", str(err.get("command")))

    # ---- 9. the other two envelope refusals, word for word ---------------
    #
    # `v` is judged first and alone: a wrong `v` is an unsupported envelope
    # with the value quoted; a v2 frame with a bad `level` is `InvalidCommand`.
    # Both halves were made to say these exact strings in the second review
    # (design.md §3.2, amended 2026-09-15).
    print("== wrong v / bad level ==")
    wrong_v = sign_request("get_sticker_info", {"dev_eui": dev_eui})
    wrong_v["v"] = 3
    client.publish(t("commands/config/request"), json.dumps(wrong_v), qos=1)
    err = wait_for("errors")
    check(err.get("error") == "unsupported_envelope" and err.get("message") == "v=2 required, got 3",
          "v=3 is refused with the value quoted", f"{err.get('error')}: {err.get('message')}")
    bad_level = sign_request("get_sticker_info", {"dev_eui": dev_eui})
    bad_level["level"] = "root"
    client.publish(t("commands/config/request"), json.dumps(bad_level), qos=1)
    err = wait_for("errors")
    check(err.get("error") == "authorization_failed"
          and err.get("message") == "Invalid command: unknown level: root",
          "an unknown level is InvalidCommand", f"{err.get('error')}: {err.get('message')}")

    print("\nALL CHECKS PASSED")
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
