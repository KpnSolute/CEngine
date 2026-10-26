#!/usr/bin/env python3
"""Generate synthetic reorder parity vectors from the legacy MJCC reorder rule.

Usage:  python3 gen_mjcc_reorder_vectors.py /path/to/Compute/backend > mjcc_reorder_vectors.json

Legacy rule: `GET /api/inventory/reorders` (backend/routes/inventory.py::get_reorders) reads
on_hand = max(0, int(on_hand)) and par = max(0, int(par)) and lists the item when par > 0 and
on_hand < par, with short = par - on_hand. `inventory_formulas.is_below_par` is the same test on
the ending quantity. The route needs a database, so its arithmetic is repeated here and the
helper is imported read-only; nothing in the Compute checkout is modified. Synthetic inputs only.
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

rng = random.Random(20261026)
cases = []


def case(name, opening, received, pulled, par):
    ending = f.ending_oh(opening, received, pulled)
    on_hand = max(0, int(f.num(ending)))
    p = max(0, int(f.num(par)))
    listed = p > 0 and on_hand < p
    assert listed == (f.is_below_par(on_hand, p)), name
    cases.append({
        "name": name,
        "input": {"opening_oh": opening, "received": received, "pulled": pulled, "par_level": str(par)},
        "legacy": {"on_hand": on_hand, "below_par": listed, "short": p - on_hand if listed else 0},
    })


case("no par set never lists", "0", "0", "0", 0)
case("no par set, stock on hand", "4", "0", "0", 0)
case("exactly at par is not low", "6", "0", "0", 6)
case("one under par", "5", "0", "0", 6)
case("empty with a par", "0", "0", "0", 4)
case("fraction under par is truncated down", "2.5", "0", "0", 3)
case("fraction at 2.999 truncates to 2", "2.999", "0", "0", 3)
case("fraction above par stays out", "3.4", "0", "0", 3)
case("short line counts as zero on hand", "2", "0", "5", 4)
case("received this month lifts it over par", "1", "9", "0", 8)
case("pulled this month drops it under par", "10", "0", "7", 5)
for i in range(120):
    opening = Decimal(rng.randint(0, 400)) / Decimal(rng.choice([1, 1, 2, 4]))
    received = Decimal(rng.randint(0, 200)) / Decimal(rng.choice([1, 1, 2]))
    pulled = Decimal(rng.randint(0, 600)) / Decimal(rng.choice([1, 1, 2, 4]))
    par = rng.choice([0, 0, 1, 2, 5, 10, 24, 48, 100])
    case(f"random {i}", str(opening), str(received), str(pulled), par)

json.dump({"source": "backend/routes/inventory.py get_reorders + inventory_formulas.is_below_par", "cases": cases},
          sys.stdout, separators=(",", ":"))
