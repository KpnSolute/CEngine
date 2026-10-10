"""Tiny reference evaluator for the CEngine expression language draft.
Used only to prove the conformance vectors are self-consistent. Not the engine."""
import re
from decimal import Decimal, ROUND_HALF_UP, getcontext
getcontext().prec = 60
TOK = re.compile(r"\s*(?:(\d+(?:\.\d+)?)|('(?:[^']|'')*')|(==|!=|<=|>=|[-+*/%<>(),.\[\]])|([a-z][a-z0-9_]*))")
KW = {'and', 'or', 'not', 'in', 'true', 'false', 'null'}

def lex(s):
    out, i = [], 0
    while i < len(s):
        if s[i].isspace(): i += 1; continue
        m = TOK.match(s, i)
        if not m: raise ValueError('bad token at %d' % i)
        i = m.end()
        n, st, op, idn = m.groups()
        if n is not None: out.append(('num', n))
        elif st is not None: out.append(('str', st[1:-1].replace("''", "'")))
        elif op is not None: out.append(('op', op))
        else: out.append(('kw' if idn in KW else 'id', idn))
    return out

def q(x, n): return x.quantize(Decimal(1).scaleb(-n), rounding=ROUND_HALF_UP)

class P:
    def __init__(s, toks, env): s.t, s.i, s.env = toks, 0, env
    def peek(s): return s.t[s.i] if s.i < len(s.t) else (None, None)
    def eat(s, v=None):
        k, x = s.peek(); s.i += 1; return k, x
    def is_(s, v): return s.peek()[1] == v and s.peek()[0] in ('op', 'kw')
    def expr(s): return s.or_()
    def or_(s):
        a = s.and_()
        while s.is_('or'):
            s.eat(); b = s.and_()
            a = True if (a is True or b is True) else (None if (a is None or b is None) else False)
        return a
    def and_(s):
        a = s.not_()
        while s.is_('and'):
            s.eat(); b = s.not_()
            a = False if (a is False or b is False) else (None if (a is None or b is None) else True)
        return a
    def not_(s):
        if s.is_('not'):
            s.eat(); v = s.cmp(); return None if v is None else (not v)
        return s.cmp()
    def cmp(s):
        a = s.add()
        k, x = s.peek()
        if x in ('==', '!=', '<', '<=', '>', '>=', 'in') and k in ('op', 'kw'):
            s.eat(); b = s.add()
            if x == 'in': return None if a is None else a in (b or [])
            if a is None and b is None: return True if x == '==' else (False if x == '!=' else None)
            if a is None or b is None: return None
            return {'==': a == b, '!=': a != b, '<': a < b, '<=': a <= b, '>': a > b, '>=': a >= b}[x]
        return a
    def add(s):
        a = s.mul()
        while s.peek()[1] in ('+', '-') and s.peek()[0] == 'op':
            op = s.eat()[1]; b = s.mul()
            a = None if a is None or b is None else (a + b if op == '+' else a - b)
        return a
    def mul(s):
        a = s.un()
        while s.peek()[1] in ('*', '/', '%') and s.peek()[0] == 'op':
            op = s.eat()[1]; b = s.un()
            if a is None or b is None: a = None; continue
            if op == '*': a = a * b
            elif op == '/': a = None if b == 0 else q(Decimal(a) / Decimal(b), 18)
            else: a = None if b == 0 else a % b
        return a
    def un(s):
        if s.is_('-'):
            s.eat(); v = s.post(); return None if v is None else -v
        return s.post()
    def post(s):
        k, x = s.eat()
        if k == 'num': return Decimal(x) if '.' in x else int(x)
        if k == 'str': return x
        if k == 'kw' and x in ('true', 'false'): return x == 'true'
        if k == 'kw' and x == 'null': return None
        if k == 'op' and x == '(':
            v = s.expr(); s.eat(); return v
        if k == 'op' and x == '[':
            items = []
            while not s.is_(']'):
                items.append(s.expr())
                if s.is_(','): s.eat()
            s.eat(); return items
        if k == 'id':
            if s.is_('('):
                s.eat(); args = []
                while not s.is_(')'):
                    args.append(s.expr())
                    if s.is_(','): s.eat()
                s.eat(); return call(x, args)
            v = s.env.get(x)
            while s.is_('.'):
                s.eat(); f = s.eat()[1]; v = None if v is None else v.get(f)
            return v
        raise ValueError('unexpected %r' % (x,))

def call(f, a):
    if f == 'round': return None if a[0] is None else q(Decimal(a[0]), a[1])
    if f == 'coalesce': return next((v for v in a if v is not None), None)
    if f == 'when': return a[1] if a[0] is True else a[2]
    if f == 'abs': return None if a[0] is None else abs(a[0])
    if f == 'min': return min(a)
    if f == 'max': return max(a)
    if f == 'len': return len(a[0])
    if f == 'lower': return a[0].lower()
    if f == 'upper': return a[0].upper()
    if f == 'is_null': return a[0] is None
    if f == 'concat': return ''.join(a)
    if f == 'starts_with': return a[0].startswith(a[1])
    if f == 'contains': return a[1] in a[0]
    raise ValueError('unsupported in reference: ' + f)

def ev(src, env=None):
    p = P(lex(src), env or {})
    v = p.expr()
    if p.i != len(p.t): raise ValueError('trailing tokens')
    return v
