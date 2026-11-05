#!/usr/bin/env python3
"""Shadow-copy gate: what a release build runs for resident files is what v0.7.0 ran, apart from named exceptions.

Lane A4 wires the record consumers (key-value, hybrid search, the query language, grouped search, facets, named vectors,
graph retrieval) to compact records. Everything compact is `cfg(test)`, so the shipped (non-test) build must not change
for resident files. Where a function has a compact arm, the resident arm is written twice, once for test builds (the
`else` of `if compact`) and once for release builds (`#[cfg(not(test))]`), and `cargo test --lib` only ever compiles the
first. This script turns the second into a checked fact:

1. For each function in SITES it takes the shipped variant (statements, arms and items under `#[cfg(test)]` are removed,
   `#[cfg(not(test))]` is dropped from the ones that stay, `cfg(any(x, test))` becomes `cfg(x)`), normalizes it
   (comments, whitespace and trailing commas do not count) and compares it with the same function at the baseline
   revision (default `v0.7.0`, read with `git show`) after applying the edits listed with the site. Every edit is a named
   exception with a reason and has to match exactly once, so a second, unlisted change to the same function fails.
   Functions that are new are compared with their expected text; the whole-function compact copies
   (`search_hybrid_compact`, `execute_compact`) must exist only under `cfg(test)`.
2. Coverage: every shipped function of a changed source file that differs from the baseline (or is new or gone) has to
   be covered by a site or by OTHER_CHANGES, so a resident-visible change cannot slip in next to the listed ones.
3. The number of `#[cfg(not(test))]` attributes of each source file is the baseline's plus the shadow sites of the list,
   so an eleventh shadow copy cannot appear unnoticed.

The key-value visitor (`visit_at`, `list_at`) is lane K71's code and has no `v0.7.0` counterpart; its sites are compared
with the revision given by --kv-base (default: the commit that carries K71's visitor, 75921271), and the edits listed are
the part that lane A4 adds on top of it.

Standard library only. Run it from the repository root of the tree under test:

    python scripts/shadow_copy_check.py                  # exit 0 when every check holds
    python scripts/shadow_copy_check.py --base v0.7.0    # another baseline revision
    python scripts/shadow_copy_check.py --kv-base REV    # the revision that holds K71's visitor as K71 left it
    python scripts/shadow_copy_check.py --dump NAME      # the shipped text and the baseline diff of one site
    python scripts/shadow_copy_check.py --self-test      # the checks of the checker

The baseline revisions have to exist (`git fetch --tags` in a shallow clone). The script stops with exit code 2 when it
cannot read a file at a baseline, and with 1 when a check fails.
"""

import argparse
import collections
import difflib
import posixpath
import re
import subprocess
import sys
from dataclasses import dataclass, field

OPEN = {"(": ")", "[": "]", "{": "}"}
CLOSE = {")", "]", "}"}


class Unsupported(Exception):
    """A construct this script does not model; it is reported instead of being guessed at."""


class Unreadable(Exception):
    """A file that cannot be read at a baseline revision."""


# ---------------------------------------------------------------------------------------------------- tokenizer

TOKEN_RE = re.compile(
    r"""
    (?P<ws>\s+)
  | (?P<lc>//[^\n]*)
  | (?P<bc>/\*)
  | (?P<raw>(?:b|c)?r\#*")
  | (?P<str>(?:b|c)?")
  | (?P<chr>b?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^\\'\n])')
  | (?P<life>'[A-Za-z_][A-Za-z0-9_]*)
  | (?P<rid>r\#[A-Za-z_][A-Za-z0-9_]*)
  | (?P<id>[A-Za-z_][A-Za-z0-9_]*)
  | (?P<num>[0-9][0-9_]*(?:\.[0-9][0-9_]*)?(?:[eE][+-]?[0-9][0-9_]*)?(?:_?(?:[iu](?:8|16|32|64|128|size)|f(?:32|64)))?)
  | (?P<p3><<=|>>=|\.\.\.|\.\.=)
  | (?P<p2>::|->|=>|==|!=|<=|>=|&&|\|\||\+=|-=|\*=|/=|%=|\^=|&=|\|=|<<|>>|\.\.)
  | (?P<p1>.)
    """,
    re.X | re.S,
)


def tokenize(src):
    """Rust source to a list of token strings; comments and whitespace are dropped, literals stay whole."""
    toks, pos, n = [], 0, len(src)
    while pos < n:
        m = TOKEN_RE.match(src, pos)
        kind = m.lastgroup
        if kind in ("ws", "lc"):
            pos = m.end()
        elif kind == "bc":
            depth, pos = 1, m.end()
            while depth and pos < n:
                if src.startswith("/*", pos):
                    depth, pos = depth + 1, pos + 2
                elif src.startswith("*/", pos):
                    depth, pos = depth - 1, pos + 2
                else:
                    pos += 1
        elif kind == "raw":
            marker = '"' + "#" * m.group().count("#")
            end = src.find(marker, m.end())
            if end < 0:
                raise Unsupported("unterminated raw string")
            toks.append(src[pos : end + len(marker)])
            pos = end + len(marker)
        elif kind == "str":
            j = m.end()
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            toks.append(src[pos : j + 1])
            pos = j + 1
        else:
            toks.append(m.group())
            pos = m.end()
    return toks


def match_close(toks, i):
    """Index of the token that closes the group opened at toks[i]."""
    stack = []
    for k in range(i, len(toks)):
        t = toks[k]
        if t in OPEN:
            stack.append(OPEN[t])
        elif t in CLOSE:
            if not stack or stack.pop() != t:
                raise Unsupported("unbalanced %r near token %d" % (t, k))
            if not stack:
                return k
    raise Unsupported("unclosed group opened at token %d" % i)


def split_commas(toks):
    """The comma-separated parts of a token list, at depth 0."""
    parts, cur, i = [], [], 0
    while i < len(toks):
        if toks[i] in OPEN:
            k = match_close(toks, i)
            cur += toks[i : k + 1]
            i = k + 1
            continue
        if toks[i] == ",":
            parts.append(cur)
            cur = []
        else:
            cur.append(toks[i])
        i += 1
    if cur:
        parts.append(cur)
    return parts


# ---------------------------------------------------------------------------------------------------- cfg handling
# The shipped build is the one in which `test` is false.

TRUE, FALSE, DROP = "true", "false", "drop"


def parse_cfg(toks, i=0):
    """A cfg predicate at toks[i:]: ('atom', tokens) | ('not', p) | ('any' | 'all', [p]); and the next index."""
    if i >= len(toks):
        raise Unsupported("an empty cfg predicate")
    name = toks[i]
    if name in ("any", "all", "not") and i + 1 < len(toks) and toks[i + 1] == "(":
        close = match_close(toks, i + 1)
        args, j = [], i + 2
        while j < close:
            arg, j = parse_cfg(toks, j)
            args.append(arg)
            if j < close:
                if toks[j] != ",":
                    raise Unsupported("a cfg predicate this script cannot read: " + " ".join(toks))
                j += 1
        if name == "not":
            if len(args) != 1:
                raise Unsupported("not() takes one predicate: " + " ".join(toks))
            return ("not", args[0]), close + 1
        return (name, args), close + 1
    if i + 2 < len(toks) and toks[i + 1] == "=":
        return ("atom", toks[i : i + 3]), i + 3
    return ("atom", [name]), i + 1


def simplify(p):
    """The predicate with test := false: TRUE, FALSE or what is left of it."""
    kind = p[0]
    if kind == "atom":
        return FALSE if p[1] == ["test"] else p
    if kind == "not":
        inner = simplify(p[1])
        return FALSE if inner == TRUE else TRUE if inner == FALSE else ("not", inner)
    parts = [simplify(x) for x in p[1]]
    if kind == "any":
        if TRUE in parts:
            return TRUE
        parts = [x for x in parts if x != FALSE]
        if not parts:
            return FALSE
    else:
        if FALSE in parts:
            return FALSE
        parts = [x for x in parts if x != TRUE]
        if not parts:
            return TRUE
    return parts[0] if len(parts) == 1 else (kind, parts)


def render_cfg(p):
    kind = p[0]
    if kind == "atom":
        return list(p[1])
    if kind == "not":
        return ["not", "("] + render_cfg(p[1]) + [")"]
    out = [kind, "("]
    for n, part in enumerate(p[1]):
        out += ([","] if n else []) + render_cfg(part)
    return out + [")"]


def cfg_value(toks):
    p, end = parse_cfg(toks, 0)
    if end != len(toks):
        raise Unsupported("a cfg predicate this script cannot read: " + " ".join(toks))
    return simplify(p)


def attr_effect(attr):
    """What an outer attribute does to the shipped build: None (nothing to do), DROP (the element is not compiled),
    or the list of attributes (token lists) that replace it."""
    inner = attr[2:-1]
    if not inner or inner[0] not in ("cfg", "cfg_attr") or "test" not in inner:
        return None
    if len(inner) < 3 or inner[1] != "(" or match_close(inner, 1) != len(inner) - 1:
        raise Unsupported("an attribute this script cannot read: " + " ".join(attr))
    body = inner[2:-1]
    if inner[0] == "cfg":
        value = cfg_value(body)
        if value == FALSE:
            return DROP
        if value == TRUE:
            return []
        return [["#", "[", "cfg", "("] + render_cfg(value) + [")", "]"]]
    condition, *rest = split_commas(body)
    value = cfg_value(condition)
    attrs = [["#", "["] + part + ["]"] for part in rest]
    if value == FALSE:
        return []
    if value == TRUE:
        return attrs
    joined = []
    for n, part in enumerate(rest):
        joined += ([","] if n else []) + part
    return [["#", "[", "cfg_attr", "("] + render_cfg(value) + [","] + joined + [")", "]"]]


def is_cfg_not_test(attr):
    return attr[2:-1] == ["cfg", "(", "not", "(", "test", ")", ")"]


def attr_group(toks, j):
    """The outer attributes that start at toks[j] and the index after them."""
    attrs = []
    while j + 1 < len(toks) and toks[j] == "#" and toks[j + 1] == "[":
        k = match_close(toks, j + 1)
        attrs.append(toks[j : k + 1])
        j = k + 1
    return attrs, j


def semi_end(toks, j):
    """Index after the next ';' at depth 0, or the end of toks (a tail expression has none)."""
    k = j
    while k < len(toks):
        if toks[k] in OPEN:
            k = match_close(toks, k)
        elif toks[k] == ";":
            return k + 1
        k += 1
    return len(toks)


def item_end(toks, j):
    """Index after an item that has a body (fn, impl, mod, ...) or ends at ';'."""
    k = j
    while k < len(toks):
        if toks[k] == "{":
            return match_close(toks, k) + 1
        if toks[k] in OPEN:
            k = match_close(toks, k)
        elif toks[k] == ";":
            return k + 1
        k += 1
    return len(toks)


def block_end(toks, j):
    """Index after a block-like statement: if/else chains, match, loops, unsafe and plain blocks (+ an optional ';')."""
    k = j
    if toks[k].startswith("'") and k + 1 < len(toks) and toks[k + 1] == ":":
        k += 2
    keyword = toks[k]
    while True:
        m = k
        while m < len(toks) and toks[m] != "{":
            m = match_close(toks, m) + 1 if toks[m] in OPEN else m + 1
        if m >= len(toks):
            raise Unsupported("a block-like statement without a body")
        k = match_close(toks, m) + 1
        if keyword == "if" and k < len(toks) and toks[k] == "else":
            k += 1
            if toks[k] == "if":
                continue
            k = match_close(toks, k) + 1
        break
    if k < len(toks) and toks[k] == ";":
        k += 1
    return k


ITEM_WORDS = {"fn", "impl", "mod", "struct", "enum", "trait", "union", "extern", "pub", "async", "macro_rules"}
SEMI_ITEM_WORDS = {"const", "static", "type", "use"}
BLOCK_WORDS = {"if", "match", "while", "for", "loop", "unsafe", "{"}


def element_end(toks, j):
    """Index after the statement or item that starts at toks[j] (the token after its attributes)."""
    t = toks[j]
    if t == "let":
        return semi_end(toks, j)
    after = toks[j + 1] if j + 1 < len(toks) else ""
    if t in ITEM_WORDS or (t == "unsafe" and after in ("fn", "impl", "trait", "extern")):
        return item_end(toks, j)
    if t == "const" and after == "fn":
        return item_end(toks, j)
    if t in SEMI_ITEM_WORDS:
        return semi_end(toks, j)
    if t in BLOCK_WORDS or (t.startswith("'") and after == ":"):
        return block_end(toks, j)
    return semi_end(toks, j)


def plain_end(toks, j):
    """Index after a match arm (`pat => expr,` or `pat => { .. }`) or a field or variant (`name: expr,`)."""
    k = j
    while k < len(toks):
        if toks[k] == "=>":
            k += 1
            if k < len(toks) and toks[k] == "{":
                k = match_close(toks, k) + 1
                return k + 1 if k < len(toks) and toks[k] == "," else k
            continue
        if toks[k] in OPEN:
            k = match_close(toks, k)
        elif toks[k] == ",":
            return k + 1
        k += 1
    return len(toks)


def plain_brace(toks, stmt_start, i):
    """True when the '{' at toks[i] opens a match body or a struct literal, where attributes are not on statements."""
    k, last_match = stmt_start, None
    while k < i:
        if toks[k] == "match":
            last_match = k
        if toks[k] in OPEN:
            k = match_close(toks, k)
        k += 1
    if last_match is not None:
        j, plain = last_match, True
        while j < i:
            if toks[j] == "{":
                plain = False
                break
            j = match_close(toks, j) + 1 if toks[j] in OPEN else j + 1
        if plain:
            return True
    prev = toks[i - 1] if i else ""
    return bool(prev) and prev[0].isupper() and re.fullmatch(r"[A-Za-z_]\w*", prev) is not None


def last_else_block(toks):
    """The tokens inside the final `else { .. }` of an if/else chain, or None when the chain has no final else."""
    k = 0
    while True:
        m = k
        while m < len(toks) and toks[m] != "{":
            m = match_close(toks, m) + 1 if toks[m] in OPEN else m + 1
        if m >= len(toks):
            return None
        k = match_close(toks, m) + 1
        if k >= len(toks) or toks[k] != "else":
            return None
        k += 1
        if toks[k] == "if":
            continue
        close = match_close(toks, k)
        return toks[k + 1 : close] if close == len(toks) - 1 else None


def resident_arm(elem):
    """What a shipped statement has to be when `elem` (`if compact { .. } else { R }`, or a `let` whose value is one)
    is its test-build twin: the tokens of R in the same position as in elem, or None when elem is not that shape."""
    if elem and elem[0] == "if":
        return last_else_block(elem)
    if elem and elem[0] == "let":
        eq = next((k for k, t in enumerate(elem) if t == "="), None)
        if eq is None or elem[eq + 1] != "if" or elem[-1] != ";":
            return None
        arm = last_else_block(elem[eq + 1 : -1])
        return None if arm is None else elem[: eq + 1] + arm + [";"]
    return None


def strip(toks, mode="block", pairs=None):
    """The shipped variant of a token list: cfg(test) elements removed, cfg(not(test)) dropped from the ones that stay.

    mode is 'block' for a list of statements, 'plain' for the inside of a match or a struct literal (a list of arms or
    fields), and 'expr' for anything else (the inside of parentheses and the like), where an attribute that mentions
    test is refused. When `pairs` is a list, every `#[cfg(not(test))]` element that directly follows a `#[cfg(test)]`
    element is recorded in it as (the test element, the shipped element): the two copies of one resident arm.
    """
    out, i, n, stmt_start, dropped = [], 0, len(toks), 0, None
    while i < n:
        t = toks[i]
        if t == "#" and i + 1 < n and toks[i + 1] == "[":
            attrs, j = attr_group(toks, i)
            effects = [attr_effect(a) for a in attrs]
            if any(e is not None for e in effects):
                if mode == "expr":
                    raise Unsupported("a cfg attribute outside a statement list: " + " ".join(attrs[0]))
                if j >= n:
                    raise Unsupported("a cfg attribute without a statement")
                end = element_end(toks, j) if mode == "block" else plain_end(toks, j)
                if DROP in effects:
                    dropped = (end, toks[j:end])
                    i = stmt_start = end
                    continue
                if pairs is not None and dropped is not None and dropped[0] == i and [] in effects:
                    pairs.append((dropped[1], toks[j:end]))
                dropped = None
                for attr, effect in zip(attrs, effects):
                    if effect is None:
                        out.extend(attr)
                    else:
                        for replacement in effect:
                            out.extend(replacement)
                out.extend(strip(toks[j:end], "expr", pairs))
                i = stmt_start = end
                continue
            dropped = None
            for attr in attrs:
                out.extend(attr)
            i = j
            continue
        dropped = None
        if t in OPEN:
            k = match_close(toks, i)
            inner_mode = "expr"
            if t == "{":
                inner_mode = "plain" if plain_brace(toks, stmt_start, i) else "block"
            out.append(t)
            out.extend(strip(toks[i + 1 : k], inner_mode, pairs))
            out.append(toks[k])
            i = k + 1
            continue
        out.append(t)
        if t == ";":
            stmt_start = i + 1
        i += 1
    return out


def normalize(toks):
    """Trailing commas before a closing bracket are a formatting matter; everything else counts."""
    return [t for i, t in enumerate(toks) if not (t == "," and i + 1 < len(toks) and toks[i + 1] in CLOSE)]


# ---------------------------------------------------------------------------------------------------- items


@dataclass
class Fn:
    name: str
    attrs: list
    sig_start: int
    body_open: int
    body_close: int
    contexts: list = field(default_factory=list)  # enclosing '{' headers: (header tokens, attributes), outermost first

    @property
    def shipped(self):
        return not gated(self.attrs, self.contexts)

    @property
    def impl_header(self):
        return " ".join(self.contexts[-1][0]) if self.contexts else None


def gated(attrs, contexts):
    """True when an attribute of the item or of a block around it keeps it out of the shipped build."""
    groups = [attrs] + [a for _, a in contexts]
    return any(attr_effect(a) == DROP for group in groups for a in group)


def scan(toks):
    """The `fn` items (with the headers of the blocks around them) and the `mod name;` declarations of a file."""
    fns, mods, groups, boundary = [], [], [], 0
    for i, t in enumerate(toks):
        top_level = not groups or groups[-1][0] == "{"
        if t == "{":
            attrs, h0 = attr_group(toks[boundary:i], 0)
            groups.append(("{", toks[boundary:i][h0:], attrs))
            boundary = i + 1
        elif t in ("(", "["):
            groups.append((t, None, None))
        elif t == "}":
            groups.pop()
            if not groups or groups[-1][0] == "{":
                boundary = i + 1
        elif t in (")", "]"):
            groups.pop()
        elif t == ";" and top_level:
            boundary = i + 1
        elif t == "mod" and top_level and i + 2 < len(toks) and toks[i + 2] == ";":
            attrs, _ = attr_group(toks, boundary)
            contexts = [(g[1], g[2]) for g in groups if g[0] == "{"]
            if contexts:
                raise Unsupported("a `mod %s;` inside an inline module is not modeled" % toks[i + 1])
            literal = next((a[4][1:-1] for a in attrs if a[2:4] == ["path", "="] and len(a) == 6), None)
            mods.append((toks[i + 1], gated(attrs, contexts), literal))
        elif t == "fn" and top_level and i + 1 < len(toks) and re.fullmatch(r"[A-Za-z_]\w*", toks[i + 1]):
            attrs, sig_start = attr_group(toks, boundary)
            k = i + 2
            while k < len(toks) and toks[k] not in ("{", ";"):
                k = match_close(toks, k) + 1 if toks[k] in OPEN else k + 1
            if k < len(toks) and toks[k] == "{":
                contexts = [(g[1], g[2]) for g in groups if g[0] == "{"]
                fns.append(Fn(toks[i + 1], attrs, sig_start, k, match_close(toks, k), contexts))
    return fns, mods


def pick(fns, name, impl):
    """The functions called `name` directly inside an impl whose header matches `impl` (None: not inside any block)."""
    found = []
    for fn in fns:
        if fn.name != name:
            continue
        if impl is None:
            if fn.contexts:
                continue
        elif fn.impl_header is None or not re.search(impl, fn.impl_header):
            continue
        found.append(fn)
    return found


def shipped_text(toks, fn, pairs=None):
    """Signature and body of fn as the shipped build sees them (normalized), or None when fn is test-only."""
    if not fn.shipped:
        return None
    sig = toks[fn.sig_start : fn.body_open]
    body = strip(toks[fn.body_open + 1 : fn.body_close], "block", pairs)
    return normalize(sig + ["{"] + body + ["}"])


def shipped_map(toks):
    """{(impl header, name, occurrence): shipped text} of every shipped function of a file."""
    out, seen = {}, collections.Counter()
    for fn in scan(toks)[0]:
        if fn.shipped:
            key = (fn.impl_header, fn.name)
            seen[key] += 1
            out[key + (seen[key],)] = shipped_text(toks, fn)
    return out


def find_attrs(toks, start, end):
    """Every outer attribute inside toks[start:end]."""
    out, i = [], start
    while i < end:
        if toks[i] == "#" and i + 1 < end and toks[i + 1] == "[":
            k = match_close(toks, i + 1)
            out.append(toks[i : k + 1])
            i = k
        i += 1
    return out


def count_not_test(toks):
    return sum(1 for a in find_attrs(toks, 0, len(toks)) if is_cfg_not_test(a))


MOD_DECL = re.compile(r"\bmod\s+[A-Za-z_]\w*\s*;")
NOT_TEST = re.compile(r"cfg\s*\(\s*not\s*\(\s*test\s*\)\s*\)")
NOT_TEST_GIT = r"cfg[[:space:]]*\([[:space:]]*not[[:space:]]*\([[:space:]]*test[[:space:]]*\)[[:space:]]*\)"


def shipped_files(paths, read):
    """The files that a non-test build compiles: reachable from a crate root without a `#[cfg(test)] mod x;`."""
    known, children, referenced = set(paths), {}, set()
    for path in paths:
        text = read(path)
        if not MOD_DECL.search(text):
            continue
        directory, _, name = path.rpartition("/")
        stem = name[:-3]
        crate_root = stem in ("lib", "main", "mod") or directory == "src/bin"
        home = directory if crate_root else (directory + "/" + stem if directory else stem)
        for mod, is_gated, literal in scan(tokenize(text))[1]:
            if literal is not None:
                candidates = [posixpath.normpath(posixpath.join(directory, literal))]
            else:
                candidates = [home + "/" + mod + ".rs", home + "/" + mod + "/mod.rs"]
            for candidate in candidates:
                if candidate in known:
                    break
            else:
                if literal is not None and not candidates[0].startswith("src/"):
                    continue  # a #[path] to a file outside src/ is not part of the checked tree
                raise Unsupported("cannot resolve `mod %s;` of %s" % (mod, path))
            children.setdefault(path, []).append((candidate, is_gated))
            referenced.add(candidate)
    shipped, stack = set(), [p for p in paths if p not in referenced]
    while stack:
        path = stack.pop()
        if path not in shipped:
            shipped.add(path)
            stack += [child for child, is_gated in children.get(path, []) if not is_gated]
    return shipped


# ---------------------------------------------------------------------------------------------------- the sites


@dataclass(frozen=True)
class Edit:
    old: str
    new: str
    why: str


@dataclass(frozen=True)
class Site:
    name: str
    file: str
    fn: str
    impl: object = None  # regex on the header of the enclosing impl, None for a free function
    base_fn: object = None  # the name at the baseline when it differs
    base_ref: str = "base"  # "base" (--base) or "kv" (--kv-base, K71's visitor)
    edits: tuple = ()  # named exceptions that turn the baseline text into the shipped text
    expect: object = None  # a new function: its exact shipped text
    test_only: bool = False  # must exist, and only under cfg(test)
    shadows: int = 0  # `#[cfg(not(test))]` attributes the function holds


KV_ACQUIRE = "the residency check is made by `acquire`, which is `ensure_resident_in` for a resident collection"
KV_PHYSICAL = "a record is read through `get_physical`, whose resident arm is the map lookup it replaces"
CLOCK = "retrieve.rs read clock: the pivot search, the pivot re-validation and the evidence read at the graph's clock"
RESIDENT_AT = "the resident search takes the one clock that `search_hybrid` captures"

SITES = [
    # key-value reads: kv.rs acquire / rows / get_physical
    Site("kv_view", "src/kv.rs", "kv_view", r"^impl Database$", shadows=1),
    Site(
        "kv_get_at",
        "src/kv.rs",
        "get_at",
        edits=(Edit("db.ensure_resident_in(collection)?;", "acquire(db, collection)?;", KV_ACQUIRE),),
    ),
    Site(
        "kv_probe",
        "src/kv.rs",
        "probe",
        edits=(
            Edit("let records = state.records.resident()?;", "", KV_PHYSICAL),
            Edit(
                "let Some(record) = records.get(&slot) else {",
                "let Some(record) = state.records.get_physical(slot)? else {",
                KV_PHYSICAL,
            ),
        ),
    ),
    Site(
        "kv_entry",
        "src/kv.rs",
        "entry",
        edits=(
            Edit(
                "let records = state.records.resident()?; let Some(record) = records.get(&slot) else {",
                "let Some(record) = state.records.get_physical(slot)? else {",
                KV_PHYSICAL,
            ),
        ),
    ),
    Site(
        "kv_check_kv_collection",
        "src/kv.rs",
        "check_kv_collection",
        edits=(
            Edit(
                "if db.state(id).dimensions == 1 { Ok(()) } else { Err(Error::NotKvCollection) }",
                "check_kv_dimension(db, id)",
                "the dimension check moved, unchanged, into `check_kv_dimension`",
            ),
        ),
    ),
    Site(
        "kv_check_kv_dimension",
        "src/kv.rs",
        "check_kv_dimension",
        expect="fn check_kv_dimension(db: &Database, id: u16) -> Result<()> {"
        " if db.state(id).dimensions == 1 { Ok(()) } else { Err(Error::NotKvCollection) } }",
    ),
    Site(
        "kv_acquire",
        "src/kv.rs",
        "acquire",
        expect="fn acquire(db: &Database, collection: u16) -> Result<()> { db.ensure_resident_in(collection) }",
    ),
    Site(
        "kv_rows",
        "src/kv.rs",
        "rows",
        expect="fn rows(state: &crate::collection::Collection) -> Result<Rows<'_>> {"
        " Ok(Rows::Resident(state.records.resident()?.iter())) }",
    ),
    Site(
        "kv_rows_next",
        "src/kv.rs",
        "next",
        r"Iterator for Rows",
        expect="fn next(&mut self) -> Option<Self::Item> {"
        " match self { Self::Resident(rows) => rows.next().map(|(&id, record)| Ok((id, record))), } }",
    ),
    Site("records_get_physical", "src/records.rs", "get_physical", r"impl CollectionRecords"),
    # K71's visitor, with what A4 adds on top of it
    Site(
        "kv_visit_at",
        "src/kv.rs",
        "visit_at",
        base_ref="kv",
        edits=(
            Edit("db.ensure_resident_in(collection)?;", "acquire(db, collection)?;", KV_ACQUIRE),
            Edit("let records = state.records.resident()?;", "", KV_PHYSICAL),
            Edit(
                "BinaryHeap::with_capacity(capacity.min(records.len()));",
                "BinaryHeap::with_capacity(capacity.min(state.records.len()));",
                "the physical row count, as `records.len()` was",
            ),
            Edit(
                "for (&id, record) in records.iter() {",
                "for row in rows(state)? { let (id, record) = row?;",
                "the scan goes through `rows`, whose resident arm is the map iterator it replaces",
            ),
            Edit(
                "let Some((_, _, value)) = records.get(&id).and_then(|record| decode(&record.payload))",
                "let Some((_, _, value)) = state.records.get_physical(id)?.and_then(|record| decode(&record.payload))",
                KV_PHYSICAL,
            ),
        ),
    ),
    Site("kv_visit_at_method", "src/kv.rs", "visit_at", r"impl KvView", base_ref="kv"),
    Site("kv_list_at", "src/kv.rs", "list_at", base_ref="kv"),
    # graph retrieval
    Site(
        "retrieve_with_visibility",
        "src/property_graph/retrieve.rs",
        "retrieve_with_visibility",
        r"impl GraphRef",
        shadows=1,
        edits=(
            Edit(
                "let at = self.with_clock(self.now());",
                "let row_now_ms = self.now(); let at = self.with_clock(row_now_ms);",
                CLOCK,
            ),
            Edit(
                "pivot_collection.search_hybrid(&query)?",
                "pivot_collection.search_hybrid_at(&query, row_now_ms)?",
                CLOCK,
            ),
            Edit(
                "db.get_in(pivot_collection.id, hit.id)?",
                "pivot_collection.get_at(hit.id, row_now_ms)?",
                CLOCK,
            ),
            Edit(
                "let Some(record) = db.get_in(collection, record_id)? else {",
                "let record = db.get_at_in(collection, record_id, row_now_ms)?; let Some(record) = record else {",
                CLOCK,
            ),
        ),
    ),
    # hybrid search
    Site(
        "hybrid_resident_at",
        "src/hybrid.rs",
        "search_hybrid_resident_at",
        r"impl CollectionRef",
        base_fn="search_hybrid",
        edits=(
            Edit(
                "pub fn search_hybrid(&self, query: &HybridQuery) -> Result<(Vec<HybridHit>, SearchStats)> {",
                "fn search_hybrid_resident_at(&self, query: &HybridQuery, now_ms: u64)"
                " -> Result<(Vec<HybridHit>, SearchStats)> {",
                RESIDENT_AT,
            ),
            Edit(
                "VectorLeg::Exact => db.search_in(self.id, vector, candidates, &query.conditions, query.metric, &mut stats)?,",
                "VectorLeg::Exact => db.search_in_at(self.id, vector, candidates, &query.conditions, query.metric,"
                " now_ms, &mut stats)?,",
                "the exact vector leg reads at the same clock as the text leg",
            ),
            Edit(
                "*expiry <= crate::current_time_ms()",
                "*expiry <= now_ms",
                "the text leg reads at the same clock as the vector leg",
            ),
        ),
    ),
    Site(
        "hybrid_search_hybrid",
        "src/hybrid.rs",
        "search_hybrid",
        r"impl CollectionRef",
        expect="pub fn search_hybrid(&self, query: &HybridQuery) -> Result<(Vec<HybridHit>, SearchStats)> {"
        " self.search_hybrid_at(query, crate::current_time_ms()) }",
    ),
    Site(
        "hybrid_search_hybrid_at",
        "src/hybrid.rs",
        "search_hybrid_at",
        r"impl CollectionRef",
        expect="pub(crate) fn search_hybrid_at(&self, query: &HybridQuery, now_ms: u64)"
        " -> Result<(Vec<HybridHit>, SearchStats)> { self.search_hybrid_resident_at(query, now_ms) }",
    ),
    Site("hybrid_compact_copy", "src/hybrid.rs", "search_hybrid_compact", r"impl CollectionRef", test_only=True),
    # the query language
    Site("query_execute", "src/query_language.rs", "execute", r"impl ParsedQuery"),
    Site("query_compact_copy", "src/query_language.rs", "execute_compact", r"impl ParsedQuery", test_only=True),
    # grouped search and facets
    Site(
        "search_grouped",
        "src/records.rs",
        "search_grouped",
        r"impl CollectionRef",
        shadows=3,
        edits=(
            Edit(
                "let Some(value) = state.records.resident()?[&hit.id].metadata.get(field) else {",
                "let record = state.records.resident()?.get(&hit.id);"
                " let Some(value) = record.and_then(|record| record.metadata.get(field)) else {",
                "a hit without a record is skipped instead of indexing the map; it cannot happen for a resident"
                " collection, whose hits come from the same map under the same borrow",
            ),
        ),
    ),
    Site("facets", "src/records.rs", "facets", r"impl CollectionRef", shadows=2),
    # named vectors and re-embedding
    Site("record_vectors", "src/vectors.rs", "record_vectors", r"CollectionRef", shadows=1),
    Site("named_records", "src/vectors.rs", "named_records", r"CollectionRef", shadows=1),
    Site("reembed_pending", "src/vectors.rs", "reembed_pending", r"CollectionRef", shadows=1),
    Site(
        "search_named_at",
        "src/vectors.rs",
        "search_named_at",
        r"CollectionRef",
        edits=(
            Edit(
                "let (ids, rows): (Vec<u64>, Vec<&[f32]>) = rs.iter()"
                ".filter(|(id, _)| self.get_at(**id, now_ms).ok().flatten().is_some())"
                ".map(|(&id, r)| (id, &r.vector[..])).unzip();",
                "let mut ids = Vec::new(); let mut rows = Vec::new();"
                " for (&id, record) in rs.iter() { if self.get_at(id, now_ms)?.is_some()"
                " { ids.push(id); rows.push(&record.vector[..]); } }",
                "search_named_at Poisoned: a read error of the primary record is returned, not dropped as a missing row",
            ),
        ),
    ),
    Site(
        "search_named_ann_at",
        "src/vectors.rs",
        "search_named_ann_at",
        r"CollectionRef",
        edits=(
            Edit(
                "let (s, rs) = self.named_records(n, version)?;",
                "self.db.ensure_resident_in(self.id)?; let (s, rs) = self.named_records(n, version)?;",
                "an early residency check on the named ANN path; `named_records` makes the same check",
            ),
        ),
    ),
]

# Shipped functions that differ from the baseline and are not lane A4's: (file, impl regex, name, reason).
OTHER_CHANGES = [
    ("src/gpu.rs", None, "resident_preflight", "lane B4 (8e63e049): the GPU search checks residency through a helper"),
    ("src/gpu.rs", r"impl GpuExactSearcher", "search", "lane B4 (8e63e049): the GPU search calls that helper"),
]


# ---------------------------------------------------------------------------------------------------- the checks


class Repo:
    """Reads the tree under test from disk and the baselines from git."""

    def __init__(self, root, base, kv_base):
        self.root, self.base, self.kv_base = root, base, kv_base
        self.cache = {}

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, capture_output=True)

    def read(self, path):
        with open("%s/%s" % (self.root, path), encoding="utf-8") as handle:
            return handle.read()

    def show(self, rev, path):
        proc = self.git("show", "%s:%s" % (rev, path))
        if proc.returncode:
            raise Unreadable(
                "cannot read %s at %s: %s (is the revision fetched? try `git fetch --tags`)"
                % (path, rev, proc.stderr.decode(errors="replace").strip())
            )
        return proc.stdout.decode("utf-8")

    def exists(self, rev, path):
        return self.git("cat-file", "-e", "%s:%s" % (rev, path)).returncode == 0

    def files(self):
        out = self.git("ls-files", "src").stdout.decode()
        return [p for p in out.split("\n") if p.endswith(".rs")]

    def files_with_not_test(self, rev):
        """The files of src at rev whose text mentions cfg(not(test)); only they can hold a shadow site."""
        out = self.git("grep", "-l", "-E", NOT_TEST_GIT, rev, "--", "src").stdout.decode()
        return {line.split(":", 1)[1] for line in out.split("\n") if ":" in line}

    def changed(self, rev):
        out = self.git("diff", "--name-only", rev, "--", "src").stdout.decode()
        return [p for p in out.split("\n") if p.endswith(".rs")]

    def tokens(self, rev, path):
        key = (rev, path)
        if key not in self.cache:
            self.cache[key] = tokenize(self.read(path) if rev is None else self.show(rev, path))
        return self.cache[key]

    def revision(self, ref):
        return self.kv_base if ref == "kv" else self.base


def render(toks):
    lines, cur = [], []
    for t in toks:
        cur.append(t)
        if t in ("{", "}", ";"):
            lines.append(" ".join(cur))
            cur = []
    if cur:
        lines.append(" ".join(cur))
    return lines


def find_sub(toks, frag):
    return [i for i in range(len(toks) - len(frag) + 1) if toks[i : i + len(frag)] == frag]


def apply_edits(toks, edits):
    """The baseline text with every edit applied, and the problems: an edit has to match once and change something."""
    problems = []
    for edit in edits:
        old, new = normalize(tokenize(edit.old)), normalize(tokenize(edit.new))
        hits = find_sub(toks, old)
        if len(hits) != 1:
            problems.append("the edit [%s] matches %d times in the baseline (it has to match once)" % (edit.why, len(hits)))
        elif old == new:
            problems.append("the edit [%s] changes nothing" % edit.why)
        else:
            toks = toks[: hits[0]] + new + toks[hits[0] + len(old) :]
    return toks, problems


def check_site(repo, site):
    """(ok, report lines) for one site."""
    label = "%s::%s" % (site.file, site.fn)
    head = repo.tokens(None, site.file)
    cands = pick(scan(head)[0], site.fn, site.impl)
    if site.test_only:
        if not cands:
            return False, ["FAIL %s: the test-only copy is gone" % label]
        if any(c.shipped for c in cands):
            return False, ["FAIL %s: it is shipped; it has to be under cfg(test)" % label]
        return True, ["ok   %s: exists only under cfg(test)" % label]
    shipped = [c for c in cands if c.shipped]
    if len(shipped) != 1:
        return False, ["FAIL %s: %d shipped definitions found (expected 1)" % (label, len(shipped))]
    fn = shipped[0]
    pairs = []
    got = shipped_text(head, fn, pairs)
    problems, lines = [], []
    count = sum(1 for a in find_attrs(head, fn.sig_start, fn.body_close) if is_cfg_not_test(a))
    if count != site.shadows:
        problems.append("it holds %d cfg(not(test)) attributes, the site list says %d" % (count, site.shadows))
    if len(pairs) != count:
        problems.append("%d of its %d cfg(not(test)) statements do not directly follow a cfg(test) twin" % (count - len(pairs), count))
    for twin, kept in pairs:
        arm = resident_arm(twin)
        if arm is None:
            problems.append("the test-build twin of a shadow statement is not `if compact { .. } else { resident }`: " + " ".join(twin[:12]))
            continue
        left, right = normalize(strip(arm, "block")), normalize(strip(kept, "block"))
        if left != right:
            problems.append("the resident arm of the test build is not the shipped statement:")
            lines += ["    " + d for d in difflib.unified_diff(render(left), render(right), "test build", "shipped", lineterm="", n=2)]
    if site.expect is not None:
        want, note = normalize(tokenize(site.expect)), "the new function has the expected text"
    else:
        rev = repo.revision(site.base_ref)
        base_toks = repo.tokens(rev, site.file)
        base_cands = [c for c in pick(scan(base_toks)[0], site.base_fn or site.fn, site.impl) if c.shipped]
        if len(base_cands) != 1:
            return False, ["FAIL %s: %d definitions at %s (expected 1)" % (label, len(base_cands), rev)]
        want, edit_problems = apply_edits(shipped_text(base_toks, base_cands[0]), site.edits)
        problems += edit_problems
        note = "equals %s%s" % (rev, " with %d named edit(s)" % len(site.edits) if site.edits else "")
    if got != want:
        problems.append("the shipped text is not the expected one:")
        lines += ["    " + d for d in difflib.unified_diff(render(want), render(got), "expected", "shipped", lineterm="", n=2)]
    if problems:
        return False, ["FAIL %s: %s" % (label, problems[0])] + ["     " + p for p in problems[1:]] + lines
    twins = ", %d shadow pair(s) with equal resident arms" % count if count else ""
    return True, ["ok   %s: %s%s" % (label, note, twins)]


def covers(site_file, site_impl, site_fn, path, header, name):
    if site_file != path or site_fn != name:
        return False
    return header is None if site_impl is None else header is not None and re.search(site_impl, header) is not None


def check_coverage(repo, shipped):
    """Every shipped function of a changed file that differs from the baseline is covered by a site or listed."""
    problems, covered = [], 0
    for path in repo.changed(repo.base):
        if path not in shipped:
            continue
        head = shipped_map(repo.tokens(None, path))
        base = shipped_map(repo.tokens(repo.base, path)) if repo.exists(repo.base, path) else {}
        diffs = [(k, "NEW" if k not in base else "CHANGED") for k, v in head.items() if base.get(k) != v]
        diffs += [(k, "REMOVED") for k in base if k not in head]
        for (header, name, _), kind in diffs:
            by_site = any(covers(s.file, s.impl, s.fn, path, header, name) for s in SITES)
            by_list = any(covers(f, i, n, path, header, name) for f, i, n, _ in OTHER_CHANGES)
            if kind == "REMOVED" or not (by_site or by_list):
                problems.append("FAIL %s: %s function %s%s is not covered by a site or by OTHER_CHANGES" % (path, kind, (header + " :: ") if header else "", name))
            else:
                covered += 1
    if problems:
        return False, problems
    return True, ["ok   coverage: %d shipped function(s) differ from %s and every one is covered by a site or a listed change" % (covered, repo.base)]


def check_shadow_counts(repo):
    """Per file, the cfg(not(test)) attributes of the tree are the baseline's plus the shadow sites of the list."""
    accounted = collections.Counter()
    for site in SITES:
        accounted[site.file] += site.shadows
    problems = []
    head_files = {p for p in repo.files() if NOT_TEST.search(repo.read(p))}
    base_files = repo.files_with_not_test(repo.base)
    for path in sorted(head_files | base_files | set(accounted)):
        head = count_not_test(repo.tokens(None, path)) if path in head_files else 0
        before = count_not_test(repo.tokens(repo.base, path)) if path in base_files else 0
        if head != before + accounted.get(path, 0):
            problems.append(
                "FAIL %s: %d cfg(not(test)) attributes, expected %d (baseline %d + %d shadow site(s))"
                % (path, head, before + accounted.get(path, 0), before, accounted.get(path, 0))
            )
    if problems:
        return False, problems
    return True, ["ok   cfg(not(test)) attributes: the baseline's plus the %d shadow site(s) of the list" % sum(accounted.values())]


def run(repo, only=None):
    ok, report = True, []
    for site in SITES:
        if only is None or only == site.name:
            good, lines = check_site(repo, site)
            ok, report = ok and good, report + lines
    if only is None:
        shipped = shipped_files(repo.files(), repo.read)
        for good, lines in (check_coverage(repo, shipped), check_shadow_counts(repo)):
            ok, report = ok and good, report + lines
    return ok, report


def dump(repo, name):
    site = next((s for s in SITES if s.name == name), None)
    if site is None:
        raise SystemExit("no site called %r; the sites are: %s" % (name, ", ".join(s.name for s in SITES)))
    head = repo.tokens(None, site.file)
    fn = [c for c in pick(scan(head)[0], site.fn, site.impl) if c.shipped][0]
    got = shipped_text(head, fn)
    print("== shipped %s::%s (%d tokens)" % (site.file, site.fn, len(got)))
    print("\n".join(render(got)))
    if site.expect is None and not site.test_only:
        rev = repo.revision(site.base_ref)
        base_toks = repo.tokens(rev, site.file)
        bf = [c for c in pick(scan(base_toks)[0], site.base_fn or site.fn, site.impl) if c.shipped][0]
        print("== diff %s -> shipped" % rev)
        print("\n".join(difflib.unified_diff(render(shipped_text(base_toks, bf)), render(got), rev, "shipped", lineterm="", n=1)))


# ---------------------------------------------------------------------------------------------------- self-test

SAMPLE = r'''
mod outer {
    /// doc
    impl Thing<'_> {
        pub fn a(&self, x: &str) -> Result<()> {
            // comment with { brace
            let s = "}{"; let c = '{'; let r = r#"}"#;
            #[cfg(test)]
            let compact = self.is_compact();
            #[cfg(test)]
            if compact {
                self.one()?;
            } else if other {
                self.two()?;
            } else {
                self.three()?;
            }
            #[cfg(not(test))]
            self.three()?;
            #[cfg(test)]
            let hits = if compact { f(1) } else { f(2) };
            #[cfg(not(test))]
            let hits = f(2,);
            for h in hits {
                #[cfg(test)]
                let rec = if compact { 1 } else { 2 };
                #[cfg(not(test))]
                let rec = 2;
                use_it(rec);
            }
            Ok(())
        }
        #[cfg(test)]
        fn only_in_tests(&self) {}
        fn dup(&self) {}
    }
    impl Other {
        fn dup(&self) { let x = 1; }
    }
    #[cfg(test)]
    mod tests {
        impl Thing<'_> { fn a(&self) {} }
    }
}
fn free(a: u8) -> u8 { a }
'''


def self_test():
    failures = []

    def expect(label, got, want):
        if got != want:
            failures.append("%s:\n  got  %r\n  want %r" % (label, got, want))

    def raises(label, thunk):
        try:
            thunk()
            failures.append(label + ": it has to be refused")
        except Unsupported:
            pass

    toks = tokenize(SAMPLE)
    fns, _ = scan(toks)
    expect(
        "locator",
        [(f.name, f.impl_header, f.shipped) for f in fns],
        [
            ("a", "impl Thing < '_ >", True),
            ("only_in_tests", "impl Thing < '_ >", False),
            ("dup", "impl Thing < '_ >", True),
            ("dup", "impl Other", True),
            ("a", "impl Thing < '_ >", False),
            ("free", None, True),
        ],
    )
    a = pick(fns, "a", r"impl Thing")
    expect("one function of a name is under a cfg(test) mod", [f.shipped for f in a], [True, False])
    want = normalize(
        tokenize(
            r'''pub fn a(&self, x: &str) -> Result<()> {
                let s = "}{"; let c = '{'; let r = r#"}"#;
                self.three()?;
                let hits = f(2);
                for h in hits { let rec = 2; use_it(rec); }
                Ok(())
            }'''
        )
    )
    expect("statements, if/else chains, let-with-if and loop bodies", shipped_text(toks, a[0]), want)
    expect("a test-only function has no shipped text", shipped_text(toks, a[1]), None)
    expect("a free function has no context", pick(fns, "free", None)[0].contexts, [])
    expect("trailing commas", normalize(tokenize("f(a, b,); g[1,]; S { x, }")), tokenize("f(a, b); g[1]; S { x }"))
    expect("count", count_not_test(toks), 3)
    pairs = []
    shipped_text(toks, a[0], pairs)
    expect("every cfg(not(test)) statement that follows a cfg(test) twin is paired", len(pairs), 3)
    expect(
        "the resident arm of each test twin is its shipped statement",
        [normalize(strip(resident_arm(twin), "block")) == normalize(strip(kept, "block")) for twin, kept in pairs],
        [True, True, True],
    )
    twin = tokenize("if compact { one(); } else if other { two(); } else { three(); self.go()?; }")
    expect("the final else arm of an if/else chain", resident_arm(twin), tokenize("three(); self.go()?;"))
    expect("an if without else has no resident arm", resident_arm(tokenize("if compact { one(); }")), None)
    expect(
        "the resident arm of a let whose value is an if",
        resident_arm(tokenize("let hits = if compact { f(1) } else { f(2, 3) };")),
        tokenize("let hits = f(2, 3);"),
    )
    expect("a plain statement is not a twin", resident_arm(tokenize("let compact = x();")), None)
    expect("cfg(any(unix, test)) is cfg(unix)", cfg_value(tokenize("any(unix, test)")), ("atom", ["unix"]))
    expect("cfg(all(unix, test)) is false", cfg_value(tokenize("all(unix, test)")), FALSE)
    expect("cfg(not(test)) is true", cfg_value(tokenize("not(test)")), TRUE)
    expect("cfg(any(feature = \"server\", test))", render_cfg(cfg_value(tokenize('any(feature = "server", test)'))), ["feature", "=", '"server"'])
    expect("cfg_attr(not(test), allow(x))", attr_effect(tokenize("#[cfg_attr(not(test), allow(unused_mut))]")), [tokenize("#[allow(unused_mut)]")])
    expect("cfg_attr(test, derive(Debug))", attr_effect(tokenize("#[cfg_attr(test, derive(Debug))]")), [])
    expect("an attribute without test has no effect", attr_effect(tokenize('#[cfg(feature = "x")]')), None)
    raises("a cfg attribute in an argument list", lambda: strip(tokenize("f(#[cfg(test)] a, b)"), "block"))
    raises("a cfg this script cannot read", lambda: attr_effect(tokenize("#[cfg(test = 1 2)]")))
    expect(
        "cfg on match arms",
        normalize(strip(tokenize("match x { #[cfg(test)] A => 1, #[cfg(not(test))] A => 2, B => { g(); } }"), "block")),
        normalize(tokenize("match x { A => 2, B => { g(); } }")),
    )
    expect(
        "cfg on a block arm without a comma and on struct fields",
        normalize(strip(tokenize("let v = match x { #[cfg(test)] A => { h(); } B => 3 }; S { #[cfg(test)] a: 1, b: 2 };"), "block")),
        normalize(tokenize("let v = match x { B => 3 }; S { b: 2 };")),
    )
    base = normalize(tokenize("fn f() { let a = g(1); let b = g(1); h(a, b); }"))
    new, problems = apply_edits(base, [Edit("h(a, b)", "h(b, a)", "swap"), Edit("let b = g(1);", "", "drop")])
    expect("edits apply once", (new, problems), (normalize(tokenize("fn f() { let a = g(1); h(b, a); }")), []))
    expect("an edit that matches twice", len(apply_edits(base, [Edit("g(1)", "g(2)", "twice")])[1]), 1)
    expect("an edit that matches nowhere", len(apply_edits(base, [Edit("nothing_here", "x", "none")])[1]), 1)
    expect("an edit that changes nothing", len(apply_edits(base, [Edit("h(a, b)", "h(a, b)", "no-op")])[1]), 1)
    expect("labels and lifetimes", tokenize("'outer: loop { f::<'a>(); }")[:4], ["'outer", ":", "loop", "{"])
    expect("nested block comments", tokenize("a /* x /* y */ z */ b"), ["a", "b"])
    expect("raw strings", tokenize('r##"a"#b"## c'), ['r##"a"#b"##', "c"])
    sources = {
        "src/lib.rs": "mod a; #[cfg(test)] mod b; #[cfg(any(unix, test))] mod c; #[path = \"x/y.rs\"] mod z;",
        "src/a.rs": "mod inner;",
        "src/a/inner.rs": "",
        "src/b.rs": "mod below;",
        "src/b/below.rs": "",
        "src/c.rs": "",
        "src/x/y.rs": "",
        "src/bin/tool.rs": "#[path = \"../x/y.rs\"] mod shared;",
    }
    expect(
        "the files a non-test build compiles",
        sorted(shipped_files(sorted(sources), sources.__getitem__)),
        ["src/a.rs", "src/a/inner.rs", "src/bin/tool.rs", "src/c.rs", "src/lib.rs", "src/x/y.rs"],
    )
    raises("an unresolved module", lambda: shipped_files(["src/lib.rs"], {"src/lib.rs": "mod missing;"}.__getitem__))
    if failures:
        sys.stderr.write("self-test FAILED\n" + "\n".join(failures) + "\n")
        return 1
    print("self-test ok")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--base", default="v0.7.0", help="baseline revision (default v0.7.0)")
    parser.add_argument("--kv-base", default="75921271", help="the revision that holds K71's key-value visitor")
    parser.add_argument("--root", default=".", help="repository root of the tree under test")
    parser.add_argument("--dump", metavar="SITE", help="print the shipped text and the baseline diff of one site")
    parser.add_argument("--self-test", action="store_true", help="run the checks of the checker")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    repo = Repo(args.root, args.base, args.kv_base)
    try:
        if args.dump:
            dump(repo, args.dump)
            return 0
        ok, report = run(repo)
    except Unreadable as error:
        sys.stderr.write("%s\n" % error)
        return 2
    except Unsupported as error:
        sys.stderr.write("shadow-copy gate: a construct this script does not model: %s\n" % error)
        return 1
    print("\n".join(report))
    print("shadow-copy gate: %s" % ("ok" if ok else "FAILED"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
