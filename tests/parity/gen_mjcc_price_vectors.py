#!/usr/bin/env python3
"""Generate synthetic price-decision vectors by running the legacy observed-price protocol.

Usage:  python3 gen_mjcc_price_vectors.py /path/to/Compute/backend > mjcc_price_vectors.json

Imports backend/pricing.py read-only from a local Compute checkout. Prices are synthetic
(hand-picked boundaries plus a seeded RNG) and written as decimal strings.
"""
import importlib.util
import json
import random
import sys
from decimal import Decimal

spec = importlib.util.spec_from_file_location("pricing", f"{sys.argv[1]}/pricing.py")
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
rng = random.Random(20261012)

pairs = [
    ("40", "50"), ("40", "50.01"), ("40", "49.99"), ("40", "30"), ("40", "29.99"), ("40", "40.004"),
    ("40", "40.005"), ("40", "39.996"), ("40", "0"), ("0", "50"), ("0", "0"), ("40", "-1"),
    ("1.000000", "1.250000"), ("1.000000", "1.250001"), ("0.12", "0.15"), ("0.12", "0.09"),
    ("0.004", "0.008"), ("0.01", "0.0149"), ("74.41", "47.20"), ("63.11", "63.11"), ("62.52", "63.11"),
    ("36.66", "47.20"), ("0.80", "1.00"), ("0.80", "1.01"), ("12.5", "9.375"), ("12.5", "9.374999"),
]
for _ in range(300):
    prev = rng.choice([0, rng.uniform(0.01, 90), rng.uniform(0.01, 5)])
    ratio = rng.choice([1, 1.1, 1.249, 1.25, 1.251, 0.75, 0.7499, 0.6, 1.9, 1.0001])
    prev_s = str(Decimal(prev).quantize(Decimal("0.000001"))) if prev else "0"
    obs_s = str((Decimal(prev_s) * Decimal(str(ratio))).quantize(Decimal("0.000001")))
    pairs.append((prev_s, obs_s))

cases = []
for prev, obs in pairs:
    decision, change = p.decide_price(float(prev), float(obs))
    cases.append({"previous": prev, "observed": obs, "decision": decision})
json.dump({"source": "backend/pricing.py decide_price", "threshold": 0.25, "tolerance": 0.005, "cases": cases}, sys.stdout, separators=(",", ":"))
print()
