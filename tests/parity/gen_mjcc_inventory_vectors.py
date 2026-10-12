#!/usr/bin/env python3
"""Generate synthetic inventory parity vectors by running the legacy MJCC formulas.

Usage:  python3 gen_mjcc_inventory_vectors.py /path/to/Compute/backend > mjcc_inventory_vectors.json

The legacy module (backend/inventory_formulas.py) is imported read-only; nothing in the Compute
checkout is modified. Inputs are synthetic (seeded RNG plus hand-picked edge cases): no MJCC data.
Quantities and prices are emitted as decimal strings so the CEngine side reads them exactly;
legacy outputs are the floats the Python code returns, written with repr.
"""
import importlib.util
import json
import random
import sys
from decimal import Decimal

backend = sys.argv[1]
spec = importlib.util.spec_from_file_location("inventory_formulas", f"{backend}/inventory_formulas.py")
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)

rng = random.Random(20261012)


def d(x, places):
    return str(Decimal(x).quantize(Decimal(1).scaleb(-places)))


def case(name, opening, recv, pull, price, ouc):
    """recv / pull are 3-tuples (weeks 1..3) of decimal strings."""
    row = {
        "opening_oh": opening,
        "w1_received": recv[0], "w2_received": recv[1], "w3_received": recv[2],
        "w1_pulled": pull[0], "w2_pulled": pull[1], "w3_pulled": pull[2],
        "unit_price": price,
        "opening_unit_cost": ouc,
    }
    out = f.resolve_row_financials(row)
    exp = {k: repr(float(out[k])) for k in (
        "opening_qty", "received_qty", "pulled_qty", "ending_qty", "opening_unit_cost",
        "opening_value", "received_value", "pulled_value", "ending_value", "ending_value_raw")}
    return {
        "name": name,
        "input": {
            "opening_oh": opening,
            "received": [recv[0], recv[1], recv[2]],
            "pulled": [pull[0], pull[1], pull[2]],
            "unit_price": price,
            "opening_unit_cost": ouc,
        },
        "legacy": exp,
    }


Z = ("0", "0", "0")
cases = [
    case("plain month", "10", ("5", "0", "2"), ("3", "4", "1"), "2.40", "2.40"),
    case("opening carried at old cost, new price higher", "10", ("0", "6", "0"), ("2", "2", "2"), "3.00", "2.50"),
    case("opening cost missing falls back to price", "8", Z, ("1", "1", "1"), "4.10", "0"),
    case("zero opening ignores stale opening cost", "0", ("4", "0", "0"), ("1", "0", "0"), "5.00", "9.99"),
    case("everything pulled: zero ending holds zero dollars", "3", ("2", "0", "0"), ("2", "2", "1"), "6.25", "6.00"),
    case("over-pull: displayed stock 0, raw residual kept", "2", ("1", "0", "0"), ("2", "2", "0"), "7.00", "7.00"),
    case("pulled worth more than received: clamp at zero", "0", ("7", "0", "0"), ("7", "0", "0"), "38.50", "0"),
    case("tie product 0.125 x 0.20", "0.125", Z, Z, "0.20", "0.20"),
    case("tie product 2.5 x 0.01", "2.5", Z, Z, "0.01", "0.01"),
    case("tie product 1.005 x 1", "1.005", Z, Z, "1", "1"),
    case("tie product 0.045 x 1 on received", "0", ("0.045", "0", "0"), Z, "1", "0"),
    case("fractional pounds", "12.375", ("4.625", "0", "3.125"), ("2.5", "2.5", "2.5"), "1.8750", "1.7500"),
    case("six decimal price", "100", ("50", "50", "50"), ("60", "60", "60"), "0.123456", "0.120000"),
    case("large quantities", "98765.432", ("12345.678", "0", "0"), ("23456.789", "0", "0"), "12.3400", "11.9900"),
]
for i in range(150):
    opening = d(rng.choice([0, 0, rng.uniform(0, 40), rng.randint(0, 200)]), rng.choice([0, 1, 3]))
    recv = tuple(d(rng.choice([0, 0, rng.uniform(0, 30)]), rng.choice([0, 3])) for _ in range(3))
    pull = tuple(d(rng.choice([0, rng.uniform(0, 20)]), rng.choice([0, 3])) for _ in range(3))
    price = d(rng.uniform(0.05, 80), rng.choice([2, 4, 6]))
    ouc = rng.choice(["0", d(rng.uniform(0.05, 80), rng.choice([2, 4, 6])), price])
    cases.append(case(f"random {i}", opening, recv, pull, price, ouc))

json.dump({"source": "backend/inventory_formulas.py resolve_row_financials", "cases": cases}, sys.stdout, separators=(',', ':'))
print()
