#!/usr/bin/env python3
"""Migrate fiber.config.yaml LoRaWAN sensors from v1 (temp_*, humidity_*) to v2 (field_thresholds).

Usage:  python3 migrate_config_v2.py /data/fiber/config/fiber.config.yaml

Backs up the original to <path>.v1.bak before writing.
"""
import sys
import shutil
import yaml
from pathlib import Path

LEGACY_FIELD_PAIRS = [
    ("temperature", "temp_critical_low", "temp_warning_low",
                    "temp_warning_high", "temp_critical_high"),
    ("humidity",    "humidity_critical_low", "humidity_warning_low",
                    "humidity_warning_high", "humidity_critical_high"),
]


def convert_sensor(sensor: dict) -> dict:
    field_thresholds = list(sensor.get("field_thresholds", []))
    fields_seen = {t["field"] for t in field_thresholds}

    for field, cl, wl, wh, ch in LEGACY_FIELD_PAIRS:
        if field in fields_seen:
            continue
        if any(sensor.get(k) is not None for k in (cl, wl, wh, ch)):
            entry = {"field": field}
            for k_legacy, k_new in ((cl, "critical_low"), (wl, "warning_low"),
                                    (wh, "warning_high"), (ch, "critical_high")):
                v = sensor.get(k_legacy)
                if v is not None:
                    entry[k_new] = v
            field_thresholds.append(entry)

    for _, *keys in LEGACY_FIELD_PAIRS:
        for k in keys:
            sensor.pop(k, None)

    if field_thresholds:
        sensor["field_thresholds"] = field_thresholds
    return sensor


def main(path_str: str) -> int:
    path = Path(path_str)
    if not path.exists():
        print(f"file not found: {path}", file=sys.stderr)
        return 1

    backup = path.with_suffix(path.suffix + ".v1.bak")
    shutil.copy2(path, backup)
    print(f"backup: {backup}")

    with path.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f) or {}

    sensors = (((data.get("lorawan") or {}).get("sensors")) or [])
    for s in sensors:
        convert_sensor(s)

    out = yaml.safe_dump(data, sort_keys=False, allow_unicode=True)
    yaml.safe_load(out)  # round-trip validation

    with path.open("w", encoding="utf-8") as f:
        f.write(out)
    print(f"wrote: {path}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: migrate_config_v2.py <yaml_path>", file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1]))
