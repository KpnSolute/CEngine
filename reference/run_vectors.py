import json, sys, os
sys.path.insert(0, os.path.dirname(__file__))
from decimal import Decimal
from expr_ref import ev

def enc(v):
    if v is None or isinstance(v, (bool, str)): return v
    return format(v, 'f') if isinstance(v, Decimal) else str(v)

def dec_env(e):
    return {k: Decimal(v['dec']) if isinstance(v, dict) else v for k, v in e.items()}

bad = 0
path = os.path.join(os.path.dirname(__file__), '..', 'conformance', 'expression-vectors.json')
vs = json.load(open(path))['vectors']
for t in vs:
    got = enc(ev(t['expr'], dec_env(t['env'])))
    if got != t['out']:
        bad += 1; print('FAIL', t['expr'], t['out'], got)
print('python reference:', len(vs) - bad, '/', len(vs), 'pass')
sys.exit(1 if bad else 0)
