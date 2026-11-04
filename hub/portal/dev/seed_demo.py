#!/usr/bin/env python3
"""Seed the running dev portal with what the mock Hubs cannot publish.

Calibration certificates (calibration.cs.md, !171/!177) live in the portal's
own DB and only arrive through the upload routes, and a Beacon's archive is
only read when the portal asks for it (beacon_history.md §2.1). This script
does both over HTTP, as an admin, against the demo cluster of
dev/mock/mock_publisher.py:

- certificates for Nodes, Beacons and Hub probe lines in every state:
  valid (> 30 d), warning (<= 30 d), second warning (<= 7 d), expired, and a
  few devices left without one (none). One probe gets an expired certificate
  replaced by a valid one, so its history has two rows;
- the two client Hubs registered in the server's cluster through Add hub
  (`POST /hubs/{server}/cluster/clients`, devices.hub.md §4), as a real
  Client must be before it can reach the broker. The server runs LoRaWAN,
  so Add hub requires a gateway EUI: each gets its profile's EUI, the truck
  included (registered with its gateway, the concentrator missing since).
  A hostname already in `cluster_clients` is skipped;
- a `beacon-history` request for each Beacon of the server Hub, which the
  mock answers with a three-day synthetic archive.

The session comes from `portal mint-session` (PORTAL_CONFIG=dev/config.yaml)
for the first enabled admin in the dev DB, or --user. No admin yet means
/setup has not been done: the script stops and says so. The token goes to a
0600 file in a private temp dir and is revoked at the end.

The certificate is a one-page PDF generated here, never a file in the repo.
Re-running uploads a new current certificate for the same devices (the old
one moves to history); the dates are relative to today, so a re-run moves
every device back into its intended state.

Usage: dev/seed_demo.py [--base URL] [--user NAME] [--skip-beacon-history]
       (run from portal/, or via `make demo-seed`)
"""

from __future__ import annotations

import argparse
import datetime as dt
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
PORTAL = os.path.dirname(HERE)

# The demo cluster's identities, as mock_publisher.py derives them from each
# profile's index (node_eui / eye_mac / hostname). Kept literal: the script
# must not need the mock's dependencies.
SERVER, COLD, TRUCK = "fiber-mock-001", "proximos-c01d0002", "proximos-7a0c0003"

# (kind, target, days until expiry, note). kind: node | beacon | probe.
# A probe target is (hub, line). Days <= 0 is expired: the expiry day is
# the first day not valid (critical on the day itself).
PLAN = [
    # valid
    ("node", "70b3d50000000000", 365, "valid"),
    ("beacon", "EE:E0:00:00:00:01", 200, "valid"),
    ("probe", (SERVER, 0), -40, "expired, replaced below"),
    ("probe", (SERVER, 0), 330, "valid (replaces an expired one)"),
    ("probe", (SERVER, 1), 120, "valid"),
    ("probe", (COLD, 0), 90, "valid"),
    # warning (<= 30 d)
    ("node", "70b3d50000000001", 21, "warning"),
    ("beacon", "EE:E0:00:00:00:02", 25, "warning"),
    ("probe", (SERVER, 2), 14, "warning"),
    # second warning (<= 7 d)
    ("node", "70b3d50000000010", 5, "second warning"),
    ("beacon", "EE:E0:00:00:00:03", 1, "second warning, last day"),
    ("probe", (SERVER, 3), 6, "second warning"),
    # expired
    ("node", "70b3d50100000020", -12, "expired"),
    ("node", "70b3d50100000021", 0, "expires today"),
    ("beacon", "EE:E0:00:00:01:01", -3, "expired"),
    ("probe", (SERVER, 4), -60, "expired"),
    ("probe", (TRUCK, 0), -1, "expired"),
    # none: Node 70b3d50100000022, Beacon EE:E0:00:00:02:01, probes 5..7 of
    # the server, 1..3 of the cold store, 1 of the truck.
]

# Add hub for each client: (hostname, gateway EUI, name), the EUI being the
# profile's `gateway_eui` (70b3d54900aa + index+1).
CLUSTER_CLIENTS = [
    (COLD, "70b3d54900aa0002", "Cold store"),
    (TRUCK, "70b3d54900aa0003", "Truck 12"),
]

BEACON_HISTORY = ["EE:E0:00:00:00:01", "EE:E0:00:00:00:02", "EE:E0:00:00:00:03"]


def fail(msg: str) -> "NoReturn":  # noqa: F821
    print(f"seed_demo: {msg}", file=sys.stderr)
    sys.exit(1)


def config_value(path: str, key: str) -> str | None:
    with open(path, encoding="utf-8") as f:
        for line in f:
            m = re.match(rf'^{key}:\s*"?([^"#\n]*?)"?\s*(#.*)?$', line)
            if m:
                return m.group(1).strip()
    return None


def first_admin(db_path: str) -> str | None:
    if not os.path.exists(db_path):
        return None
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        row = con.execute(
            "SELECT username FROM users WHERE enabled = 1 "
            "AND (user_role = 'admin' OR is_superuser = 1) ORDER BY id LIMIT 1"
        ).fetchone()
    finally:
        con.close()
    return row[0] if row else None


def make_pdf(title: str, lines: list[str]) -> bytes:
    """A one-page PDF with a few lines of Helvetica, offsets computed."""
    def esc(t: str) -> str:
        t = t.encode("latin-1", "replace").decode("latin-1")
        return t.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
    text = ["BT", "/F1 18 Tf", "72 760 Td", f"({esc(title)}) Tj", "/F1 11 Tf"]
    for line in lines:
        text += ["0 -20 Td", f"({esc(line)}) Tj"]
    text.append("ET")
    stream = "\n".join(text).encode("latin-1")
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for n, body in enumerate(objs, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % n + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    return bytes(out)


class Portal:
    def __init__(self, base: str, token: str):
        self.base = base.rstrip("/")
        self.token = token

    def post(self, path: str, body: bytes, content_type: str) -> tuple[int, str]:
        req = urllib.request.Request(self.base + path, data=body, method="POST", headers={
            "Cookie": f"portal_session={self.token}",
            "Origin": self.base,
            "HX-Request": "true",
            "Content-Type": content_type,
        })
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.status, r.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode("utf-8", "replace")

    def upload(self, path: str, pdf: bytes, valid_until: str, note: str) -> tuple[int, str]:
        boundary = uuid.uuid4().hex
        parts = []
        for name, value, filename in (("file", pdf, "kalibracni-list-demo.pdf"),
                                      ("valid_until", valid_until.encode(), None),
                                      ("note", note.encode(), None)):
            head = f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"'
            if filename:
                head += f'; filename="{filename}"\r\nContent-Type: application/pdf'
            parts.append(head.encode() + b"\r\n\r\n" + value + b"\r\n")
        body = b"".join(parts) + f"--{boundary}--\r\n".encode()
        return self.post(path, body, f"multipart/form-data; boundary={boundary}")


def cal_path(kind: str, target) -> str:
    if kind == "node":
        return f"/nodes/{target}/calibrations"
    if kind == "beacon":
        return f"/beacons/{urllib.parse.quote(target)}/calibrations"
    hub, line = target
    return f"/hubs/{hub}/sensors/{line}/calibrations"


def label(kind: str, target) -> str:
    return f"{kind} {target[0]} line {target[1] + 1}" if kind == "probe" else f"{kind} {target}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base", default="http://127.0.0.1:8088")
    ap.add_argument("--config", default=os.path.join(PORTAL, "dev", "config.yaml"))
    ap.add_argument("--bin", default=os.path.join(PORTAL, "target", "debug", "portal"))
    ap.add_argument("--user", help="admin to mint the session for (default: first admin)")
    ap.add_argument("--skip-beacon-history", action="store_true")
    args = ap.parse_args()

    data_dir = config_value(args.config, "data_dir") or "./dev-data"
    if not os.path.isabs(data_dir):
        data_dir = os.path.join(PORTAL, data_dir)
    user = args.user or first_admin(os.path.join(data_dir, "portal.db"))
    if not user:
        fail(f"no admin account in {data_dir}/portal.db. Open {args.base}/setup, "
             "create the admin, then run this again.")
    if not os.access(args.bin, os.X_OK):
        fail(f"{args.bin} not built — run `make build` (or `make demo`) first")

    tmp = tempfile.mkdtemp(prefix="seed-demo-")
    token_file = os.path.join(tmp, "session")
    env = dict(os.environ, PORTAL_CONFIG=os.path.abspath(args.config))
    mint = subprocess.run([args.bin, "mint-session", "--user", user, "--out", token_file,
                           "--ttl", "10m"], cwd=PORTAL, env=env, capture_output=True, text=True)
    if mint.returncode != 0:
        fail(f"mint-session for {user!r} failed: {mint.stderr.strip() or mint.stdout.strip()}")
    try:
        with open(token_file, encoding="utf-8") as f:
            portal = Portal(args.base, f.read().strip())
        failures = seed_cluster(portal, os.path.join(data_dir, "portal.db"))
        return seed(portal, user, args.skip_beacon_history) or (1 if failures else 0)
    finally:
        subprocess.run([args.bin, "revoke-session", "--token-file", token_file],
                       cwd=PORTAL, env=env, capture_output=True)
        if os.path.exists(token_file):
            os.remove(token_file)
        os.rmdir(tmp)


def registered(db_path: str) -> set[str]:
    try:
        con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            return {r[0] for r in con.execute("SELECT hostname FROM cluster_clients")}
        finally:
            con.close()
    except sqlite3.Error:
        return set()


def seed_cluster(portal: Portal, db_path: str) -> int:
    """Add hub for each client not yet in the registry. The completion
    screen's join bundle is never revealed: the mock clients already talk to
    the broker. A real Client's first status after the join is what turns
    the row complete, and an already-online mock sends none, so each new one
    is taken off the air and back (dev/hub.sh) to play that join."""
    failures = 0
    have = registered(db_path)
    print("cluster clients:")
    for host, eui, name in CLUSTER_CLIENTS:
        if host in have:
            print(f"  skip  {host}: already registered")
            continue
        body = urllib.parse.urlencode({"hostname": host, "gateway_eui": eui, "name": name})
        status, html = portal.post(f"/hubs/{SERVER}/cluster/clients", body.encode(),
                                   "application/x-www-form-urlencoded")
        if status == 200 and host in registered(db_path):
            print(f"  ok    {host} gateway {eui} ({name})")
            for verb in ("offline", "online"):
                subprocess.run([os.path.join(HERE, "hub.sh"), verb, host], cwd=PORTAL,
                               capture_output=True)
        else:
            text = " ".join(re.sub(r"<[^>]+>", " ", html).split())
            print(f"  FAIL  {host}: HTTP {status} {text[:200]}")
            failures += 1
    return failures


def seed(portal: Portal, user: str, skip_beacon_history: bool) -> int:
    today = dt.datetime.now(dt.timezone.utc).date()
    failures = 0
    print(f"calibrations (as {user}):")
    for kind, target, days, note in PLAN:
        until = (today + dt.timedelta(days=days)).isoformat()
        pdf = make_pdf("Kalibracni list - DEMO", [
            f"Zarizeni: {label(kind, target)}",
            f"Platnost do: {until}",
            f"Vystaveno: {today.isoformat()} (mock data, dev/seed_demo.py)",
        ])
        status, body = portal.upload(cal_path(kind, target), pdf, until, f"demo: {note}")
        if status == 404:
            print(f"  skip  {label(kind, target)}: not in the live state (Hub not running?)")
            failures += 1
            continue
        if status != 200 or 'class="alert alert-error"' in body:
            m = re.search(r'alert-error"><span>([^<]*)', body)
            print(f"  FAIL  {label(kind, target)}: HTTP {status} {m.group(1) if m else ''}")
            failures += 1
            continue
        print(f"  ok    {label(kind, target):38} until {until}  ({note})")

    if not skip_beacon_history:
        print("beacon history:")
        for mac in BEACON_HISTORY:
            status, body = portal.post(f"/beacons/{urllib.parse.quote(mac)}/history", b"",
                                       "application/x-www-form-urlencoded")
            if status == 200 and 'data-status="error"' not in body:
                print(f"  ok    {mac} archive requested")
            else:
                text = " ".join(re.sub(r"<[^>]+>", " ", body).split())
                print(f"  FAIL  {mac}: HTTP {status} {text[:160]}")
                failures += 1
    print("done" if failures == 0 else f"done, {failures} step(s) failed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
