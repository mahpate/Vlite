"""Independent vlite-cypher-1 front end and read oracle, Python 3.10+, stdlib only.

Source of truth: docs/projects/roadmap/gap6-cypher-subset.md, sections 3/4/5/6/7/12.
Committed graph inputs and fake clocks only; no fixture generation, dynamic
execution, wall-clock sampling, writes, or network operations.
"""

from __future__ import annotations

import argparse
import copy
from dataclasses import dataclass, field
from functools import cmp_to_key
import hashlib
import json
import math
from pathlib import Path
import random
import re
import sys
import unicodedata


MUTATION = None
RESERVED = frozenset("""MATCH WHERE RETURN ORDER BY SKIP LIMIT AS OF KEY ID CREATE
MERGE SET REMOVE DELETE DETACH AND OR NOT IS NULL ASC DESC OPTIONAL WITH UNWIND CALL
YIELD UNION ALL DISTINCT FOREACH LOAD CSV USE INSERT UPDATE DROP ALTER INDEX
CONSTRAINT EXPLAIN PROFILE SHORTEST PATH SEARCH NEAR IN TRUE FALSE EXISTS""".split())
EXCLUDED = frozenset("""OPTIONAL WITH UNWIND CALL YIELD UNION FOREACH LOAD CSV USE
INSERT UPDATE DROP ALTER INDEX CONSTRAINT EXPLAIN PROFILE SHORTEST PATH SEARCH NEAR
DISTINCT IN EXISTS COUNT ON""".split())
MUTATIONS = (
    "keyword-case", "unicode-space", "split-symbol", "spaced-param",
    "character-offset", "eof-offset", "unsupported-syntax", "syntax-unsupported",
    "parameter-syntax", "limit-parameter", "allow-reserved", "reserve-names",
    "reserve-params", "allow-multi-match", "allow-chain", "allow-unanchored",
    "allow-path-node", "allow-rel-hops", "allow-anchor-hops", "allow-write-hops",
    "allow-write-asof", "allow-redeclare", "allow-future", "allow-edge-label",
    "allow-detach-edge", "allow-duplicate-label", "allow-duplicate-map",
    "allow-duplicate-patch", "allow-duplicate-column", "allow-unresolved",
    "allow-entity-sort", "allow-unprojected-sort", "allow-unaliased-sort", "allow-hop-zero",
    "allow-hop-leading-zero", "raise-hop-cap", "lower-hop-cap",
    "raise-byte-cap", "lower-byte-cap", "raise-token-cap", "lower-token-cap",
    "raise-depth-cap", "lower-depth-cap", "raise-predicate-cap",
    "lower-predicate-cap", "raise-param-count", "lower-param-count",
    "raise-param-name", "lower-param-name", "raise-param-bytes",
    "lower-param-bytes", "allow-missing", "allow-unused", "allow-key-type",
    "allow-id-type", "allow-skip-type", "allow-limit-type", "allow-asof-type",
    "allow-duplicate-params", "allow-eof-tail", "allow-caller-hop", "type-first-name-offset",
    "reject-empty-key",
)


def mutated(name):
    return MUTATION == name


def cap(name, normal):
    return normal + int(mutated("raise-" + name)) - int(mutated("lower-" + name))


def ascii_upper(value):
    return value.translate(str.maketrans("abcdefghijklmnopqrstuvwxyz",
                                       "ABCDEFGHIJKLMNOPQRSTUVWXYZ"))


@dataclass(frozen=True)
class Diagnostic(Exception):
    kind: str
    offset: int
    reason: str | None = None

    def result(self):
        result = {"kind": self.kind, "offset": self.offset}
        if self.reason is not None:
            result["reason"] = self.reason
        return result


# Lexer

@dataclass(frozen=True)
class Token:
    kind: str
    value: str
    offset: int
    end: int


def lex(text):
    offsets = [0]
    for char in text:
        offsets.append(offsets[-1] + len(char.encode("utf-8")))
    length = offsets[-1]

    def fail(kind, index):
        position = index if mutated("character-offset") else offsets[index]
        if kind == "Unsupported" and mutated("unsupported-syntax"):
            kind = "Syntax"
        elif kind == "Syntax" and mutated("syntax-unsupported"):
            kind = "Unsupported"
        if kind == "Limit" and mutated("limit-parameter"):
            kind = "Parameter"
        raise Diagnostic(kind, position)

    byte_cap = cap("byte-cap", 16384)
    if length > byte_cap:
        # Keep the first over-cap position on a Unicode scalar boundary.
        index = next(i for i, position in enumerate(offsets) if position >= byte_cap)
        fail("Limit", index)
    tokens = []
    i = 0
    while i < len(text):
        char = text[i]
        if char in " \t\r\n" or (mutated("unicode-space") and char.isspace()):
            i += 1
            continue
        start = i
        if char == "`":
            i += 1
            begin = i
            while i < len(text) and text[i] != "`":
                if unicodedata.category(text[i]) == "Cc" or 0xD800 <= ord(text[i]) <= 0xDFFF:
                    fail("Syntax", i)
                i += 1
            if i == len(text):
                fail("Syntax", start)
            if begin == i:
                fail("Syntax", start)
            value = text[begin:i]
            i += 1
            kind = "quoted"
        elif char == "$":
            i += 1
            if mutated("spaced-param"):
                while i < len(text) and text[i] in " \t\r\n":
                    i += 1
            begin = i
            if i == len(text) or not re.match(r"[A-Za-z_]", text[i]):
                fail("Syntax", start)
            i += 1
            while i < len(text) and re.match(r"[A-Za-z0-9_]", text[i]):
                i += 1
            value, kind = text[begin:i], "param"
        elif re.match(r"[A-Za-z_]", char):
            i += 1
            while i < len(text) and re.match(r"[A-Za-z0-9_]", text[i]):
                i += 1
            value, kind = text[start:i], "bare"
        elif char in "0123456789":
            i += 1
            while i < len(text) and text[i] in "0123456789":
                i += 1
            value, kind = text[start:i], "uint"
        else:
            symbol = text[i:i + 2]
            if symbol in ("!=", "//", "/*", "--"):
                fail("Unsupported", i)
            if symbol in ("->", "<-", "<>", "<=", ">=", "..") and not mutated("split-symbol"):
                value = symbol
                i += 2
            elif char in "()[]{}:,.=<>|-*":
                value = char
                i += 1
            else:
                fail("Unsupported" if char in ";'\"+/%^" else "Syntax", i)
            kind = "symbol"
        position = start if mutated("character-offset") else offsets[start]
        end = i if mutated("character-offset") else offsets[i]
        tokens.append(Token(kind, value, position, end))
        if len(tokens) > cap("token-cap", 1024):
            fail("Limit", start)
    eof = length - int(mutated("eof-offset") and length > 0)
    if mutated("character-offset"):
        eof = len(text)
    tokens.append(Token("eof", "", eof, eof))
    return tokens


# Parser: plain statement data, suitable for a later evaluator.

@dataclass(frozen=True)
class Reference:
    name: str
    offset: int
    use: str


@dataclass(frozen=True)
class Field:
    variable: Token
    name: Token


@dataclass(frozen=True)
class Predicate:
    op: str
    operands: tuple
    offset: int


@dataclass
class Node:
    variable: Token
    labels: list[Token]
    anchor: tuple[str, Reference] | None
    properties: list[tuple[Token, Reference]]
    offset: int


@dataclass
class Relationship:
    variable: Token | None
    types: list[Token]
    anchor: tuple[str, Reference] | None
    hops: int | None
    properties: list[tuple[Token, Reference]]
    direction: str
    offset: int
    star_offset: int | None = None
    upper_offset: int | None = None


@dataclass
class Pattern:
    path: Token | None
    first: Node
    relationship: Relationship | None
    second: Node | None


@dataclass
class Mutation:
    kind: str
    items: list
    offset: int


@dataclass
class Projection:
    variable: Token
    property: Token | None
    alias: Token | None

    @property
    def column(self):
        return self.alias.value if self.alias else (
            self.variable.value + ("." + self.property.value if self.property else ""))


@dataclass
class Returning:
    items: list[Projection]
    sorts: list[tuple[Token, str]]
    skip: Reference | None
    limit: Reference | None


@dataclass
class Statement:
    kind: str
    pattern: Pattern | None
    as_of: Reference | None
    where: Predicate | None
    mutations: list[Mutation]
    returning: Returning | None
    references: list[Reference]
    bindings: list[tuple[str, str]] = field(default_factory=list)


class Parser:
    def __init__(self, text):
        self.tokens = lex(text)
        self.i = 0
        self.references = []
        self.atoms = 0
        self.depth = 0

    @property
    def token(self):
        return self.tokens[self.i]

    def word(self, token=None):
        token = token or self.token
        if token.kind != "bare":
            return ""
        return token.value if mutated("keyword-case") else ascii_upper(token.value)

    def at(self, value):
        return self.word() == value or (
            self.token.kind == "symbol" and self.token.value == value)

    def take(self):
        token = self.token
        if token.kind != "eof":
            self.i += 1
        return token

    def maybe(self, value):
        if self.at(value):
            return self.take()
        return None

    def fail(self, kind="Syntax", token=None):
        token = token or self.token
        if kind == "Syntax" and mutated("syntax-unsupported"):
            kind = "Unsupported"
        elif kind == "Unsupported" and mutated("unsupported-syntax"):
            kind = "Syntax"
        elif kind == "Limit" and mutated("limit-parameter"):
            kind = "Parameter"
        raise Diagnostic(kind, token.offset)

    def expected_failure(self):
        token = self.token
        next_token = self.tokens[min(self.i + 1, len(self.tokens) - 1)]
        if (self.word() in EXCLUDED | {"TRUE", "FALSE", "NULL"} or token.kind == "uint"
                or token.value in ("*", "[", "{")
                or (token.value == "-" and next_token.kind in ("uint", "param", "bare", "quoted"))
                or (token.kind == "bare" and next_token.value in ("(", "{"))):
            self.fail("Unsupported")
        self.fail()

    def need(self, value):
        if not self.at(value):
            self.expected_failure()
        return self.take()

    def identifier(self, name=False):
        token = self.token
        if token.kind not in ("bare", "quoted"):
            self.expected_failure()
        index = self.i
        while (self.tokens[index + 1].value == "."
               and self.tokens[index + 2].kind in ("bare", "quoted")):
            index += 2
        if self.tokens[index + 1].value == "(":
            self.fail("Unsupported", self.tokens[index])
        reserved = self.word(token) in RESERVED
        if reserved and ((not name and not mutated("allow-reserved"))
                         or (name and mutated("reserve-names"))):
            self.fail(token=token)
        return self.take()

    def param(self, use="value"):
        if self.token.kind != "param":
            self.expected_failure()
        token = self.take()
        if mutated("reserve-params") and ascii_upper(token.value) in RESERVED:
            self.fail(token=token)
        ref = Reference(token.value, token.offset, use)
        self.references.append(ref)
        return ref

    def atom(self, token):
        self.atoms += 1
        if self.atoms > cap("predicate-cap", 64):
            self.fail("Limit", token)

    def properties(self, predicates=False):
        result = []
        if self.maybe("{"):
            if not self.at("}"):
                while True:
                    name = self.identifier(name=True)
                    self.need(":")
                    ref = self.param()
                    result.append((name, ref))
                    if predicates:
                        self.atom(name)
                    if not self.maybe(","):
                        break
            self.need("}")
        return result

    def anchor(self):
        if self.at("KEY") or self.at("ID"):
            token = self.take()
            kind = self.word(token)
            return kind, self.param(kind)
        return None

    def node(self, new=False):
        begin = self.need("(")
        variable = self.identifier()
        labels = []
        while self.maybe(":"):
            labels.append(self.identifier(name=True))
        anchor = self.anchor()
        if new and (anchor is None or anchor[0] != "KEY"):
            self.fail(token=self.token)
        properties = self.properties(predicates=not new)
        self.need(")")
        return Node(variable, labels, anchor, properties, begin.offset)

    def relationship(self):
        begin = self.take()
        incoming = begin.value == "<-"
        if not self.at("["):
            if self.at("-") or self.at("->"):
                self.fail("Unsupported", begin)
            self.fail()
        self.need("[")
        variable = None
        if self.token.kind in ("bare", "quoted") and not self.at("KEY") and not self.at("ID"):
            variable = self.identifier()
        types = []
        if self.maybe(":"):
            types.append(self.identifier(name=True))
            while self.maybe("|"):
                self.maybe(":")
                types.append(self.identifier(name=True))
        anchor = self.anchor()
        hops, star = None, self.maybe("*")
        if star:
            if ((variable and not mutated("allow-rel-hops"))
                    or (anchor and not mutated("allow-anchor-hops"))):
                self.fail("Unsupported", star)
            minimum = self.token
            if minimum.kind != "uint" or minimum.value != "1":
                self.fail("Unsupported", star)
            self.take()
            if not self.at(".."):
                self.fail("Unsupported", star)
            self.take()
            bound = self.token
            if bound.kind != "uint":
                self.fail("Unsupported", star)
            self.take()
            if len(bound.value) > 1 and bound.value.startswith("0") and not mutated("allow-hop-leading-zero"):
                self.fail(token=bound)
            # Lexicographic comparison avoids Python's integer digit limit.
            significant = bound.value.lstrip("0") or "0"
            if significant == "0" and not mutated("allow-hop-zero"):
                self.fail(token=bound)
            if (len(significant) > 1 or significant > str(cap("hop-cap", 4))):
                self.fail("Limit", bound)
            hops = int(significant)
        properties = self.properties(predicates=True)
        self.need("]")
        if incoming:
            self.need("-")
            direction = "in"
        elif self.maybe("->"):
            direction = "out"
        else:
            self.need("-")
            direction = "both"
        return Relationship(variable, types, anchor, hops, properties, direction,
                            begin.offset, star.offset if star else None,
                            bound.offset if star else None)

    def match(self):
        self.need("MATCH")
        path = None
        if self.token.kind in ("bare", "quoted"):
            path = self.identifier()
            self.need("=")
        first = self.node()
        rel = second = None
        if self.at("-") or self.at("<-"):
            rel = self.relationship()
            second = self.node()
        if self.at(","):
            if not mutated("allow-multi-match"):
                self.fail("Unsupported")
            self.take()
            self.node()
        if self.at("-") or self.at("<-"):
            if not mutated("allow-chain"):
                self.fail("Unsupported")
            self.relationship()
            self.node()
        return Pattern(path, first, rel, second)

    def field(self):
        variable = self.identifier()
        self.need(".")
        return Field(variable, self.identifier(name=True))

    def predicate(self):
        if self.at("("):
            opening = self.take()
            self.depth += 1
            if self.depth > cap("depth-cap", 16):
                self.fail("Limit", opening)
            result = self.disjunction()
            self.need(")")
            self.depth -= 1
            return result
        begin = self.token
        next_token = self.tokens[min(self.i + 1, len(self.tokens) - 1)]
        if next_token.value in ("(", "{"):
            self.fail("Unsupported")
        target = self.field()
        if self.at("("):
            self.fail("Unsupported", target.name)
        self.atom(begin)
        if self.maybe("IS"):
            negative = bool(self.maybe("NOT"))
            self.need("NULL")
            return Predicate("IS NOT NULL" if negative else "IS NULL", (target,), begin.offset)
        if self.token.value not in ("=", "<>", "<", "<=", ">", ">="):
            self.expected_failure()
        operator = self.take().value
        return Predicate(operator, (target, self.param()), begin.offset)

    def negation(self):
        token = self.maybe("NOT")
        result = self.predicate()
        return Predicate("NOT", (result,), token.offset) if token else result

    def conjunction(self):
        result = self.negation()
        while self.at("AND"):
            token = self.take()
            result = Predicate("AND", (result, self.negation()), token.offset)
        return result

    def disjunction(self):
        result = self.conjunction()
        while self.at("OR"):
            token = self.take()
            result = Predicate("OR", (result, self.conjunction()), token.offset)
        return result

    def creation(self, merge=False):
        begin = self.need("(")
        variable = self.identifier()
        labels = []
        while self.maybe(":"):
            labels.append(self.identifier(name=True))
        if self.at(")") and not labels:
            closing = self.take()
            if self.at("-"):
                rel_begin = self.take()
                self.need("[")
                rel_variable = self.identifier()
                self.need(":")
                rel_type = self.identifier(name=True)
                anchor = None
                if merge or self.at("KEY"):
                    self.need("KEY")
                    anchor = "KEY", self.param("KEY")
                properties = self.properties()
                self.need("]")
                if self.at("-"):
                    self.fail()
                self.need("->")
                end_begin = self.need("(")
                end_variable = self.identifier()
                self.need(")")
                first = Node(variable, [], None, [], begin.offset)
                second = Node(end_variable, [], None, [], end_begin.offset)
                rel = Relationship(rel_variable, [rel_type], anchor, None,
                                   properties, "out", rel_begin.offset)
                return Pattern(None, first, rel, second)
            if self.at("<-"):
                self.fail()
            self.fail("Unsupported" if merge else "Syntax", closing)
        self.need("KEY")
        anchor = "KEY", self.param("KEY")
        properties = self.properties()
        self.need(")")
        return Pattern(None, Node(variable, labels, anchor, properties, begin.offset), None, None)

    def mutation(self):
        token = self.take()
        kind = self.word(token)
        items = []
        if kind in ("CREATE", "MERGE"):
            items.append(self.creation(merge=kind == "MERGE"))
            while kind == "CREATE" and self.maybe(","):
                items.append(self.creation())
        elif kind in ("SET", "REMOVE"):
            while True:
                variable = self.identifier()
                if self.maybe("."):
                    target = Field(variable, self.identifier(name=True))
                    if kind == "SET":
                        self.need("=")
                        items.append((target, self.param()))
                    else:
                        items.append((target, None))
                else:
                    self.need(":")
                    items.append(((variable, self.identifier(name=True)), None))
                if not self.maybe(","):
                    break
        else:
            if kind == "DETACH":
                self.need("DELETE")
                kind = "DETACH DELETE"
            items.append(self.identifier())
            while self.maybe(","):
                items.append(self.identifier())
        return Mutation(kind, items, token.offset)

    def returning(self):
        self.need("RETURN")
        items = []
        while True:
            token = self.token
            next_token = self.tokens[min(self.i + 1, len(self.tokens) - 1)]
            if self.word() in ("DISTINCT", "TRUE", "FALSE", "NULL") or token.value == "*" or next_token.value in ("(", "{"):
                self.fail("Unsupported")
            variable = self.identifier()
            prop = self.identifier(name=True) if self.maybe(".") else None
            if prop and self.at("("):
                self.fail("Unsupported", prop)
            alias = self.identifier() if self.maybe("AS") else None
            items.append(Projection(variable, prop, alias))
            if not self.maybe(","):
                break
        sorts = []
        if self.maybe("ORDER"):
            self.need("BY")
            while True:
                variable = self.identifier()
                direction = self.word() if self.at("ASC") or self.at("DESC") else "ASC"
                if self.at("ASC") or self.at("DESC"):
                    self.take()
                sorts.append((variable, direction))
                if not self.maybe(","):
                    break
        skip = self.param("SKIP") if self.maybe("SKIP") else None
        limit = self.param("LIMIT") if self.maybe("LIMIT") else None
        return Returning(items, sorts, skip, limit)

    def parse(self):
        pattern = self.match() if self.at("MATCH") else None
        as_of = None
        as_token = self.maybe("AS") if pattern else None
        if as_token:
            self.need("OF")
            as_of = self.param("AS OF")
        where = self.disjunction() if pattern and self.maybe("WHERE") else None
        mutations = []
        while self.word() in ("CREATE", "MERGE", "SET", "REMOVE", "DELETE", "DETACH"):
            mutations.append(self.mutation())
        if mutations and as_of and not mutated("allow-write-asof"):
            self.fail("Unsupported", as_token)
        kind = "write" if mutations else "read"
        if pattern is None and not mutations:
            self.expected_failure()
        returning = self.returning() if self.at("RETURN") else None
        if kind == "read" and returning is None:
            self.need("RETURN")
        if mutations and self.at("AS") and self.word(self.tokens[min(self.i + 1, len(self.tokens) - 1)]) == "OF":
            self.fail("Unsupported")
        if self.token.kind != "eof" and not mutated("allow-eof-tail"):
            self.expected_failure()
        return Statement(kind, pattern, as_of, where, mutations, returning, self.references)


# Static analysis and parameter validation

def syntax(token, switch=None):
    if switch and mutated(switch):
        return
    kind = "Unsupported" if mutated("syntax-unsupported") else "Syntax"
    raise Diagnostic(kind, token.offset)


def duplicates(tokens, switch):
    seen = set()
    for token in tokens:
        if token.value in seen:
            syntax(token, switch)
        seen.add(token.value)


def analyse(statement):
    bound = {}

    def declare(token, kind, repeat=None):
        if token.value in bound and token.value != repeat:
            syntax(token, "allow-redeclare")
        if token.value not in bound:
            bound[token.value] = kind
            statement.bindings.append((token.value, kind))
        elif token.value == repeat and bound[token.value] != "node":
            syntax(token, "allow-redeclare")

    def resolve(token, future=False):
        if token.value not in bound:
            if not (mutated("allow-unresolved") or (future and mutated("allow-future"))):
                syntax(token)
            return "node"
        return bound[token.value]

    def members(node):
        duplicates(node.labels, "allow-duplicate-label")
        duplicates([name for name, _ in node.properties], "allow-duplicate-map")

    def walk(expression):
        for operand in expression.operands:
            if isinstance(operand, Predicate):
                walk(operand)
            elif isinstance(operand, Field):
                if resolve(operand.variable, future=True) == "path":
                    syntax(operand.variable)

    pattern = statement.pattern
    if pattern:
        if pattern.path:
            declare(pattern.path, "path")
        declare(pattern.first.variable, "node")
        members(pattern.first)
        rel = pattern.relationship
        if pattern.path and rel is None:
            syntax(pattern.path, "allow-path-node")
        if rel:
            point = Token("symbol", "", rel.offset, rel.offset)
            star = Token("symbol", "*", rel.star_offset or rel.offset, rel.star_offset or rel.offset)
            if pattern.first.anchor is None:
                syntax(point, "allow-unanchored")
            if rel.hops is not None and statement.kind == "write" and not mutated("allow-write-hops"):
                raise Diagnostic("Syntax" if mutated("unsupported-syntax") else "Unsupported", star.offset)
            if rel.variable:
                declare(rel.variable, "rel")
            duplicates([name for name, _ in rel.properties], "allow-duplicate-map")
            declare(pattern.second.variable, "node", repeat=pattern.first.variable.value)
            members(pattern.second)
    if statement.where:
        walk(statement.where)
    for mutation in statement.mutations:
        if mutation.kind in ("CREATE", "MERGE"):
            for creation in mutation.items:
                members(creation.first)
                if creation.relationship:
                    rel = creation.relationship
                    resolve(creation.first.variable)
                    resolve(creation.second.variable)
                    if bound.get(creation.first.variable.value) != "node":
                        syntax(creation.first.variable)
                    if bound.get(creation.second.variable.value) != "node":
                        syntax(creation.second.variable)
                    declare(rel.variable, "rel")
                    duplicates([name for name, _ in rel.properties], "allow-duplicate-map")
                else:
                    declare(creation.first.variable, "node")
        elif mutation.kind in ("SET", "REMOVE"):
            seen = set()
            for target, _ in mutation.items:
                if isinstance(target, Field):
                    variable, member, category = target.variable, target.name, "property"
                else:
                    variable, member = target
                    category = "label"
                kind = resolve(variable)
                if kind == "path":
                    syntax(variable)
                if category == "label" and kind != "node":
                    syntax(variable, "allow-edge-label")
                identity = (variable.value, category, member.value)
                if identity in seen:
                    syntax(member, "allow-duplicate-patch")
                seen.add(identity)
        else:
            for token in mutation.items:
                kind = resolve(token)
                if kind == "path":
                    syntax(token)
                if mutation.kind == "DETACH DELETE" and kind != "node":
                    syntax(token, "allow-detach-edge")
    if statement.returning:
        columns = {}
        for item in statement.returning.items:
            kind = resolve(item.variable)
            if item.property and kind == "path":
                syntax(item.variable)
            if item.column in columns:
                syntax(item.alias or item.property or item.variable, "allow-duplicate-column")
            columns[item.column] = item
        for token, _ in statement.returning.sorts:
            if token.value not in columns:
                syntax(token, "allow-unprojected-sort")
                continue
            if columns[token.value].property is None:
                syntax(token, "allow-entity-sort")
            elif columns[token.value].alias is None:
                syntax(token, "allow-unaliased-sort")
    return statement


@dataclass(frozen=True)
class RawObject:
    entries: tuple


def unique_json(raw, duplicate_kind="fixture"):
    def pairs(items):
        if duplicate_kind == "parameter":
            return RawObject(tuple(items))
        result = {}
        for name, value in items:
            if name in result:
                raise ValueError("fixture: duplicate-json-key")
            result[name] = value
        return result

    def decode(value, root=False):
        if isinstance(value, RawObject):
            names = [name for name, _ in value.entries]
            if root and len(set(names)) != len(names) and not mutated("allow-duplicate-params"):
                raise Diagnostic("Parameter", 0)
            result = {}
            for name, child in value.entries:
                if name in result:
                    if not root:
                        raise Diagnostic("InvalidMetadata", 0)
                    if not mutated("allow-duplicate-params"):
                        raise Diagnostic("Parameter", 0)
                result[name] = decode(child)
            return result
        if isinstance(value, list):
            return [decode(child) for child in value]
        return value

    def constant(_):
        raise ValueError("invalid-json-constant")

    try:
        result = json.loads(raw, object_pairs_hook=pairs, parse_constant=constant)
        return decode(result, root=True) if duplicate_kind == "parameter" else result
    except (json.JSONDecodeError, ValueError) as error:
        if duplicate_kind == "parameter":
            raise Diagnostic("InvalidMetadata", 0) from error
        if str(error).startswith("fixture: "):
            raise
        raise ValueError("fixture: invalid-json") from error


def value_size(value):
    """Validate a typed payload and return its tag plus encoded value bytes."""
    if not isinstance(value, dict):
        raise Diagnostic("InvalidMetadata", 0)
    tag = value.get("tag")
    wanted = {"tag", "latitude", "longitude"} if tag == "GeoPoint" else {"tag", "value"}
    if set(value) != wanted:
        raise Diagnostic("InvalidMetadata", 0)
    payload = value.get("value")
    valid = False
    size = 0
    if tag == "String" and isinstance(payload, str):
        try:
            size = 4 + len(payload.encode("utf-8"))
            valid = size <= 4100
        except UnicodeEncodeError:
            valid = False
    elif tag == "Integer" and isinstance(payload, str) and re.fullmatch(r"-?(0|[1-9][0-9]*)", payload):
        if len(payload) <= 20:
            valid = -(2**63) <= int(payload) <= 2**63 - 1
        size = 8
    elif tag == "Float" and type(payload) in (int, float):
        try:
            valid = math.isfinite(float(payload))
        except OverflowError:
            valid = False
        size = 8
    elif tag == "Boolean":
        valid, size = type(payload) is bool, 1
    elif tag == "GeoPoint":
        lat, lon = value["latitude"], value["longitude"]
        valid = (type(lat) in (int, float) and type(lon) in (int, float)
                 and -90 <= lat <= 90 and -180 <= lon <= 180)
        size = 16
    if not valid:
        raise Diagnostic("InvalidMetadata", 0)
    return 1 + size


def finite_number(value):
    if type(value) not in (int, float):
        return False
    try:
        return math.isfinite(float(value))
    except OverflowError:
        return False


def validate_parameters(statement, params, eof):
    if not isinstance(params, dict):
        raise Diagnostic("InvalidMetadata", 0)

    def limit():
        raise Diagnostic("Parameter" if mutated("limit-parameter") else "Limit", eof)

    if len(params) > cap("param-count", 64):
        limit()
    total = 4
    for name, value in params.items():
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            raise Diagnostic("InvalidMetadata", 0)
        if len(name) > cap("param-name", 256):
            limit()
        total += 2 + len(name) + value_size(value)
    if total > cap("param-bytes", 65536):
        limit()
    used = set()
    first_offsets = {}
    for reference in statement.references:
        first_offsets.setdefault(reference.name, reference.offset)

    def parameter(offset):
        raise Diagnostic("Syntax" if mutated("parameter-syntax") else "Parameter", offset)

    for reference in statement.references:
        name, use = reference.name, reference.use
        used.add(name)
        if name not in params:
            if mutated("allow-missing"):
                continue
            parameter(reference.offset)
        value = params[name]
        tag, payload = value["tag"], value.get("value")
        valid = True
        switch = None
        if use == "KEY":
            valid = tag == "String" and not (mutated("reject-empty-key") and payload == "")
            switch = "allow-key-type"
        elif use in ("ID", "SKIP", "LIMIT"):
            valid = tag == "Integer" and int(payload) >= 0
            switch = "allow-" + use.lower() + "-type"
        elif use == "AS OF":
            valid = tag == "Integer"
            switch = "allow-asof-type"
        if not valid and not mutated(switch):
            parameter(first_offsets[name] if mutated("type-first-name-offset") else reference.offset)
    if set(params) - used and not mutated("allow-unused"):
        parameter(eof)


def validate_hops(statement, max_hops):
    if type(max_hops) is not int or max_hops < 0:
        raise Diagnostic("InvalidQuery", 0)
    if max_hops > 4:
        raise Diagnostic("GraphLimitExceedsCeiling", 0)
    if statement.pattern and statement.pattern.relationship:
        rel = statement.pattern.relationship
        hops = rel.hops if rel.hops is not None else 1
        if hops > max_hops and not mutated("allow-caller-hop"):
            position = rel.upper_offset if rel.upper_offset is not None else rel.offset
            raise Diagnostic("InvalidQuery", position, "subset hop bound exceeds max_hops")


def parse(text, params=None, params_raw=None, max_hops=2):
    parser = Parser(text)
    statement = analyse(parser.parse())
    if params_raw is not None:
        params = unique_json(params_raw, "parameter")
    elif params is None:
        params = {}
    validate_parameters(statement, params, parser.tokens[-1].offset)
    validate_hops(statement, max_hops)
    return statement


def outcome(text, params=None, params_raw=None, max_hops=2):
    try:
        statement = parse(text, params, params_raw, max_hops)
    except Diagnostic as error:
        return {"outcome": "reject", "error": error.result()}
    return {
        "outcome": "accept", "kind": statement.kind,
        "columns": [item.column for item in statement.returning.items] if statement.returning else [],
        "references": [[ref.name, ref.offset] for ref in statement.references],
        "bindings": [list(binding) for binding in statement.bindings],
    }


def quote(name):
    return "`" + name + "`"


def format_statement(statement):
    def param(ref):
        return "$" + ref.name

    def props(items):
        return (" {" + ", ".join(quote(name.value) + ": " + param(ref)
                                for name, ref in items) + "}") if items else ""

    def node(item, endpoint=False):
        result = "(" + quote(item.variable.value)
        if not endpoint:
            result += "".join(":" + quote(label.value) for label in item.labels)
            if item.anchor:
                result += " " + item.anchor[0] + " " + param(item.anchor[1])
            result += props(item.properties)
        return result + ")"

    def pattern(item, creation=False):
        result = quote(item.path.value) + " = " if item.path else ""
        result += node(item.first, endpoint=creation and item.relationship is not None)
        rel = item.relationship
        if rel:
            detail = "[" + (quote(rel.variable.value) if rel.variable else "")
            if rel.types:
                detail += ":" + "|".join(quote(token.value) for token in rel.types)
            if rel.anchor:
                detail += " " + rel.anchor[0] + " " + param(rel.anchor[1])
            if rel.hops is not None:
                detail += "*1.." + str(rel.hops)
            detail += props(rel.properties) + "]"
            result += ("<-" if rel.direction == "in" else "-") + detail
            result += "->" if rel.direction == "out" else "-"
            result += node(item.second, endpoint=creation)
        return result

    def target(item):
        return quote(item.variable.value) + "." + quote(item.name.value)

    def expression(item, parent=0):
        precedence = {"OR": 1, "AND": 2, "NOT": 3}.get(item.op, 4)
        if item.op in ("OR", "AND"):
            result = (" " + item.op + " ").join(expression(child, precedence) for child in item.operands)
        elif item.op == "NOT":
            result = "NOT " + expression(item.operands[0], precedence)
        elif item.op.startswith("IS"):
            result = target(item.operands[0]) + " " + item.op
        else:
            result = target(item.operands[0]) + " " + item.op + " " + param(item.operands[1])
        return "(" + result + ")" if precedence < parent else result

    parts = []
    if statement.pattern:
        parts.append("MATCH " + pattern(statement.pattern))
    if statement.as_of:
        parts.append("AS OF " + param(statement.as_of))
    if statement.where:
        parts.append("WHERE " + expression(statement.where))
    for mutation in statement.mutations:
        if mutation.kind in ("CREATE", "MERGE"):
            body = ", ".join(pattern(item, creation=True) for item in mutation.items)
        elif mutation.kind in ("SET", "REMOVE"):
            items = []
            for item, ref in mutation.items:
                if isinstance(item, Field):
                    value = target(item)
                    if ref:
                        value += " = " + param(ref)
                else:
                    value = quote(item[0].value) + ":" + quote(item[1].value)
                items.append(value)
            body = ", ".join(items)
        else:
            body = ", ".join(quote(item.value) for item in mutation.items)
        parts.append(mutation.kind + " " + body)
    if statement.returning:
        returning = statement.returning
        items = []
        for item in returning.items:
            value = quote(item.variable.value)
            if item.property:
                value += "." + quote(item.property.value)
            if item.alias:
                value += " AS " + quote(item.alias.value)
            items.append(value)
        parts.append("RETURN " + ", ".join(items))
        if returning.sorts:
            parts.append("ORDER BY " + ", ".join(quote(token.value) + " " + direction
                                                for token, direction in returning.sorts))
        if returning.skip:
            parts.append("SKIP " + param(returning.skip))
        if returning.limit:
            parts.append("LIMIT " + param(returning.limit))
    return " ".join(parts)


# Read evaluator: graph inputs are immutable committed views; clocks are explicit.

READ_MUTATIONS = (
    "read-reverse-direction", "read-drop-label", "read-drop-type",
    "read-repeat-edge", "read-double-loop", "read-dedup-endpoints",
    "read-coerce-int", "read-bool-int", "read-not-unknown",
    "read-reverse-null", "read-reverse-tie", "read-limit-first",
    "read-parameter-text", "read-ttl-inclusive", "read-from-exclusive",
    "read-sample-clock", "read-short-and", "read-short-or",
    "read-last-edge-predicate", "read-intermediate-endpoint",
    "read-skip-first", "read-casefold", "read-signed-zero",
    "read-geo-latitude", "read-is-not-null", "read-to-inclusive",
    "read-hide-legacy", "read-expired-endpoint", "read-default-time-zero",
    "read-visited-per-path",
    "read-output-per-row", "read-distinct-path-cap", "read-label-or",
    "read-map-or", "read-property-identity", "read-sort-rank",
    "read-desc-null", "read-ignore-edge-anchor", "read-default-order",
    "read-scan-desc", "read-reset-work", "read-no-cancel-sort",
    "read-no-check-64", "read-clamp-limits", "read-zero-output",
    "read-zero-return", "read-drop-path-nodes", "read-drop-path-edges",
    "read-ignore-scratch",
    "read-no-check-before", "read-no-scan-work", "read-live-target-only",
    "read-allow-time-overflow", "read-no-order-work", "read-no-canonical-work",
    "read-no-frontier-insert-work", "read-no-adjacency-work",
    "read-no-map-work", "read-no-frontier-pop-work", "read-allow-empty-key",
) + tuple("read-cap-" + name for name in (
    "visited", "edges", "degree", "bindings", "rows",
    "work", "output-nodes", "output-edges", "output-paths", "deadline"))

READ_DEFAULTS = {
    "max_hops": 2, "max_visited_nodes": 1000, "max_edges_examined": 10000,
    "max_edges_per_node": 1000, "deadline_ms": 250, "max_output_nodes": 100,
    "max_output_edges": 200, "max_output_paths": 20, "max_binding_rows": 1000,
    "max_frontier_entries": 1000, "max_returned_rows": 100,
    "max_work_units": 100000, "max_scratch_bytes": 33554432,
    "write_deadline_ms": 100,
}
READ_CEILINGS = dict(zip(READ_DEFAULTS, (
    4, 10000, 100000, 10000, 5000, 1000, 2000, 100, 10000, 10000,
    1000, 1000000, 134217728, 1000)))
READ_REASONS = frozenset("subset " + reason for reason in (
    "visited-node budget exhausted", "edge budget exhausted",
    "degree budget exhausted", "deadline exceeded", "cancelled",
    "frontier budget exhausted", "binding-row budget exhausted",
    "result-row budget exhausted", "output budget exhausted",
    "work budget exhausted", "scratch budget exhausted",
    "hop bound exceeds max_hops", "work limits must be positive",
    "comparison type mismatch"))
NULL = {"tag": "Null"}


@dataclass(frozen=True)
class ReadFailure(Exception):
    kind: str
    reason: str | None = None
    feature: int | None = None

    def result(self):
        result = {"kind": self.kind}
        if self.reason is not None:
            result["reason"] = self.reason
        if self.feature is not None:
            result["feature"] = self.feature
        return result


def read_fail(reason):
    raise ReadFailure("InvalidQuery", "subset " + reason)


def read_limits(overrides):
    limits = dict(READ_DEFAULTS)
    if overrides is not None:
        if not isinstance(overrides, dict) or set(overrides) - set(limits):
            raise ValueError("read input: limits")
        limits.update(overrides)
    if any(type(value) is not int or value < 0 for value in limits.values()):
        raise ValueError("read input: limits")
    if any(limits[name] > READ_CEILINGS[name] for name in limits):
        if mutated("read-clamp-limits"):
            limits = {name: min(value, READ_CEILINGS[name]) for name, value in limits.items()}
        else:
            raise ReadFailure("GraphLimitExceedsCeiling")
    positive = set(limits) - {
        "max_hops", "max_output_nodes", "max_output_edges",
        "max_output_paths", "max_returned_rows",
    }
    if any(limits[name] == 0 for name in positive):
        read_fail("work limits must be positive")
    if mutated("read-zero-output"):
        for name in ("max_output_nodes", "max_output_edges", "max_output_paths"):
            if limits[name] == 0:
                limits[name] = READ_DEFAULTS[name]
    if mutated("read-zero-return") and limits["max_returned_rows"] == 0:
        limits["max_returned_rows"] = 100
    return limits


class ReadMeter:
    def __init__(self, limits, clock):
        self.limits = limits
        self.clock = clock
        self.work = 0
        self.scratch = 0
        self.edges = 0
        self.visited = set()
        self.visits = 0
        if not mutated("read-no-check-before"):
            self.check("before")

    def over(self, name, count, switch):
        return count > self.limits[name] + int(mutated("read-cap-" + switch))

    def check(self, phase):
        if phase == "sort" and mutated("read-no-cancel-sort"):
            return
        cancelled = self.clock.get("cancelled", False)
        stop = self.clock.get("cancel_at_work")
        cancelled = cancelled or (stop is not None and self.work >= stop)
        cancelled = cancelled or phase in self.clock.get("cancel_phases", [])
        if cancelled:
            read_fail("cancelled")
        elapsed = self.clock.get("elapsed_before_ms", 0)
        for sample in self.clock.get("monotonic", []):
            if sample["at_work"] <= self.work:
                elapsed = max(elapsed, sample["elapsed_ms"])
        elapsed = max(elapsed, self.clock.get("elapsed_by_phase", {}).get(phase, 0))
        bound = self.limits["deadline_ms"] + int(mutated("read-cap-deadline"))
        if elapsed >= bound:
            read_fail("deadline exceeded")

    def charge(self):
        self.work += 1
        if self.over("max_work_units", self.work, "work"):
            read_fail("work budget exhausted")
        if self.work % 64 == 0 and not mutated("read-no-check-64"):
            self.check("interval")

    def allocate(self, size):
        if mutated("read-ignore-scratch"):
            return
        # Logical owned-byte model, not a claim about a Rust memory layout.
        self.scratch += size
        if self.over("max_scratch_bytes", self.scratch, "scratch"):
            read_fail("scratch budget exhausted")

    def visit(self, node_id):
        self.visited.add(node_id)
        self.visits += 1
        count = self.visits if mutated("read-visited-per-path") else len(self.visited)
        if self.over("max_visited_nodes", count, "visited"):
            read_fail("visited-node budget exhausted")

    def examine(self, node_id, degree):
        if not mutated("read-no-adjacency-work"):
            self.charge()
        self.edges += 1
        if self.over("max_edges_examined", self.edges, "edges"):
            read_fail("edge budget exhausted")
        if self.over("max_edges_per_node", degree, "degree"):
            read_fail("degree budget exhausted")

    def frontier_insert(self, entries):
        if not mutated("read-no-frontier-insert-work"):
            self.charge()
        if self.over("max_frontier_entries", entries, "frontier"):
            read_fail("frontier budget exhausted")

    def binding_insert(self, rows):
        if self.over("max_binding_rows", rows, "bindings"):
            read_fail("binding-row budget exhausted")

    def returned(self, rows):
        if self.over("max_returned_rows", rows, "rows"):
            read_fail("result-row budget exhausted")

    def output(self, nodes, edges, paths):
        if (self.over("max_output_nodes", nodes, "output-nodes")
                or self.over("max_output_edges", edges, "output-edges")
                or self.over("max_output_paths", paths, "output-paths")):
            read_fail("output budget exhausted")


def scalar_payload(value):
    tag = value["tag"]
    if tag == "Integer":
        return int(value["value"])
    if tag == "Float":
        return float(value["value"])
    if tag == "String":
        text = value["value"].casefold() if mutated("read-casefold") else value["value"]
        return text.encode("utf-8")
    if tag == "GeoPoint":
        return float(value["latitude"]), float(value["longitude"])
    if tag == "Null":
        return None
    return value["value"]


def typed_equal(left, right):
    a, b = left["tag"], right["tag"]
    if mutated("read-parameter-text") and b == "String" and "RETURN" in right["value"]:
        return True
    if mutated("read-coerce-int") and a in ("Integer", "Float") and b in ("Integer", "Float"):
        return float(left["value"]) == float(right["value"])
    if mutated("read-bool-int") and a in ("Integer", "Boolean") and b in ("Integer", "Boolean"):
        return scalar_payload(left) == scalar_payload(right)
    if a != b:
        return False
    if a == "Float" and mutated("read-signed-zero") and scalar_payload(left) == scalar_payload(right) == 0:
        return math.copysign(1, scalar_payload(left)) == math.copysign(1, scalar_payload(right))
    if a == "GeoPoint" and mutated("read-geo-latitude"):
        return left["latitude"] == right["latitude"]
    return scalar_payload(left) == scalar_payload(right)


def compare_value(left, operator, right):
    if left["tag"] == "Null" or right["tag"] == "Null":
        return None
    if operator in ("=", "<>"):
        equal = typed_equal(left, right)
        return equal if operator == "=" else not equal
    if left["tag"] != right["tag"] or left["tag"] not in ("Integer", "Float", "String"):
        read_fail("comparison type mismatch")
    a, b = scalar_payload(left), scalar_payload(right)
    if mutated("read-coerce-int") and left["tag"] == "Integer":
        a, b = float(a), float(b)
    return {"<": a < b, "<=": a <= b, ">": a > b, ">=": a >= b}[operator]


def expression(predicate, row, params, meter):
    if predicate.op in ("AND", "OR"):
        left = expression(predicate.operands[0], row, params, meter)
        if (predicate.op == "AND" and left is False and mutated("read-short-and")
                or predicate.op == "OR" and left is True and mutated("read-short-or")):
            return left
        right = expression(predicate.operands[1], row, params, meter)
        if predicate.op == "AND":
            return False if left is False or right is False else (
                True if left is True and right is True else None)
        return True if left is True or right is True else (
            False if left is False and right is False else None)
    if predicate.op == "NOT":
        value = expression(predicate.operands[0], row, params, meter)
        if value is None:
            return True if mutated("read-not-unknown") else None
        return not value
    meter.charge()
    target = predicate.operands[0]
    entity = row[target.variable.value]
    value = entity["properties"].get(target.name.value, NULL)
    if mutated("read-property-identity") and target.name.value in ("id", "key", "revision"):
        name = target.name.value
        value = {"tag": "String" if name == "key" else "Integer", "value": entity[name]}
    if predicate.op in ("IS NULL", "IS NOT NULL"):
        absent = value["tag"] == "Null"
        if mutated("read-is-not-null") and predicate.op == "IS NOT NULL":
            return absent
        return absent if predicate.op == "IS NULL" else not absent
    return compare_value(value, predicate.op, params[predicate.operands[1].name])


def scalar_sort_key(value):
    ranks = {"Boolean": 0, "Integer": 1, "Float": 2, "String": 3, "GeoPoint": 4, "Null": 5}
    if mutated("read-sort-rank"):
        ranks["Integer"], ranks["Float"] = ranks["Float"], ranks["Integer"]
    if mutated("read-reverse-null"):
        ranks["Null"] = -1
    return ranks[value["tag"]], scalar_payload(value)


def row_identity(row, bindings):
    identity = []
    for name, kind in sorted(bindings, key=lambda binding: binding[0].encode("utf-8")):
        entity = row[name]
        if kind == "path":
            identity.append((entity["length"], tuple(map(int, entity["edge_ids"])),
                             tuple(map(int, entity["node_ids"]))))
        else:
            identity.append(int(entity["id"]))
    return tuple(identity)


def entity_result(entity, kind):
    if kind == "path":
        return {"tag": "Path", "length": entity["length"],
                "edge_ids": list(entity["edge_ids"]), "node_ids": list(entity["node_ids"])}
    result = {"tag": "Node" if kind == "node" else "Edge",
              "id": entity["id"], "key": entity["key"],
              "properties": copy.deepcopy(entity["properties"]), "revision": entity["revision"]}
    if kind == "node":
        result["labels"] = sorted(entity["labels"], key=lambda label: label.encode("utf-8"))
    else:
        result.update(type=entity["type"], source=entity["source"], target=entity["target"])
    return result


def owned_result_bytes(entity, kind):
    if kind == "path":
        return 8 * (1 + len(entity["edge_ids"]) + len(entity["node_ids"]))
    size = 16 + (4 + len(entity["key"].encode("utf-8")) if entity["key"] is not None else 1)
    if kind == "node":
        size += 4 + sum(4 + len(label.encode("utf-8")) for label in entity["labels"])
    else:
        size += 16 + 4 + len(entity["type"].encode("utf-8"))
    size += 4
    for name, value in entity["properties"].items():
        size += 2 + len(name.encode("utf-8")) + value_size(value)
    return size


def evaluate_read(graph, statement, params=None, limits=None, clock=None):
    """Return a tagged result/error; writes are explicitly NOT_MODELED.

    `statement` accepts text or Part A's Statement. No epoch/monotonic sampling,
    graph mutation, external access, or fixture rewriting occurs here.
    """
    params = {} if params is None else params
    clock = {} if clock is None else clock
    try:
        bounds = read_limits(limits)
        if isinstance(statement, str):
            statement = parse(statement, params, max_hops=bounds["max_hops"])
        else:
            validate_parameters(statement, params, 0)
            validate_hops(statement, bounds["max_hops"])
        if statement.kind == "write":
            return {"outcome": "NOT_MODELED"}
        meter = ReadMeter(bounds, clock)
        if not mutated("read-allow-empty-key"):
            for reference in statement.references:
                if reference.use == "KEY" and params[reference.name]["value"] == "":
                    raise ReadFailure("InvalidQuery")
        ttl = int(clock.get("ttl_clock_ms", graph["ttl_clock_ms"]))
        temporal = graph["temporal_capable"]
        if statement.as_of and not temporal:
            raise ReadFailure("UnsupportedFeature", feature=64)
        instant = int(params[statement.as_of.name]["value"]) if statement.as_of else (ttl if temporal else None)
        upper_instant = 2**63 - 1 + int(mutated("read-allow-time-overflow"))
        if temporal and not -(2**63) <= instant <= upper_instant:
            # The specification requires checked conversion but leaves its error unspecified.
            raise ReadFailure("InvalidQuery")
        if mutated("read-default-time-zero") and temporal and not statement.as_of:
            instant = 0
        samples = clock.get("ttl_samples_ms", [])
        if mutated("read-sample-clock") and samples:
            ttl = int(samples[-1])
        nodes = {node["id"]: node for node in graph["nodes"]}
        edges = {edge["id"]: edge for edge in graph["edges"]}

        def live(entity):
            expiry = entity.get("expires_at_ms")
            if expiry is None:
                return True
            return ttl <= int(expiry) if mutated("read-ttl-inclusive") else ttl < int(expiry)

        def active(edge):
            if not live(edge):
                return None
            endpoints = ("target",) if mutated("read-live-target-only") else ("source", "target")
            if not mutated("read-expired-endpoint") and not all(live(nodes[edge[key]]) for key in endpoints):
                return None
            if "segments" not in edge:
                return None if instant is not None and mutated("read-hide-legacy") else edge
            if instant is None:
                raise ValueError("read input: temporal edge on legacy view")
            for segment in edge["segments"]:
                start, end = segment["from_ms"], segment["to_ms"]
                after = start is None or (instant > int(start) if mutated("read-from-exclusive") else instant >= int(start))
                before = end is None or (instant <= int(end) if mutated("read-to-inclusive") else instant < int(end))
                if after and before:
                    meter.allocate(8 * len(edge))
                    return dict(edge, properties=segment["properties"])
            return None

        def anchor(pattern, candidates, edge=False):
            if pattern.anchor is None:
                return candidates
            meter.charge()
            meter.allocate(8)
            if edge and mutated("read-ignore-edge-anchor"):
                return candidates
            role, ref = pattern.anchor
            wanted = params[ref.name]["value"]
            return [entity for entity in candidates if entity["key" if role == "KEY" else "id"] == wanted]

        def matches(pattern, entity, edge=False, check_properties=True):
            if edge:
                types = [token.value for token in pattern.types]
                if types and entity["type"] not in types and not mutated("read-drop-type"):
                    return False
            else:
                labels = [token.value for token in pattern.labels]
                if labels and not mutated("read-drop-label"):
                    tests = [label in entity["labels"] for label in labels]
                    if not (any(tests) if mutated("read-label-or") else all(tests)):
                        return False
            if pattern.anchor:
                role, ref = pattern.anchor
                if entity["key" if role == "KEY" else "id"] != params[ref.name]["value"]:
                    if not (edge and mutated("read-ignore-edge-anchor")):
                        return False
            values = []
            if check_properties:
                for token, ref in pattern.properties:
                    if not mutated("read-no-map-work"):
                        meter.charge()
                    values.append(compare_value(entity["properties"].get(token.value, NULL), "=", params[ref.name]) is True)
            return (any(values) if mutated("read-map-or") else all(values)) if values else True

        pattern = statement.pattern
        rows = []

        def prepare_binding():
            meter.binding_insert(len(rows) + 1)
            meter.allocate(8 * len(statement.bindings))

        candidates = sorted(nodes.values(), key=lambda node: int(node["id"]),
                            reverse=mutated("read-scan-desc"))
        starts = anchor(pattern.first, candidates)
        for start in starts:
            if pattern.first.anchor is None and not mutated("read-no-scan-work"):
                meter.charge()
            meter.visit(start["id"])
            if not live(start) or not matches(pattern.first, start):
                continue
            if pattern.relationship is None:
                prepare_binding()
                rows.append({pattern.first.variable.value: start})
                continue
            rel = pattern.relationship
            direction = rel.direction
            if mutated("read-reverse-direction") and direction != "both":
                direction = "in" if direction == "out" else "out"
            maximum = rel.hops if rel.hops is not None else 1
            edge_candidates = anchor(rel, sorted(edges.values(), key=lambda edge: int(edge["id"])), edge=True)
            frontier = []

            def push(node_ids, edge_ids, next_node, next_edge=None):
                meter.frontier_insert(len(frontier) + 1)
                meter.allocate(8 * (len(node_ids) + len(edge_ids) + 1 + int(next_edge is not None)))
                frontier.append((list(node_ids) + [next_node],
                                 list(edge_ids) + ([] if next_edge is None else [next_edge])))

            push((), (), start["id"])
            while frontier:
                node_ids, edge_ids = frontier.pop()
                if not mutated("read-no-frontier-pop-work"):
                    meter.charge()
                current = node_ids[-1]
                if edge_ids:
                    endpoint = nodes[current]
                    equal_endpoint = (pattern.second.variable.value != pattern.first.variable.value or current == start["id"])
                    if equal_endpoint and matches(pattern.second, endpoint):
                        prepare_binding()
                        path = {"length": len(edge_ids), "edge_ids": edge_ids, "node_ids": node_ids}
                        row = {pattern.first.variable.value: start,
                               pattern.second.variable.value: endpoint}
                        if rel.variable:
                            row[rel.variable.value] = active(edges[edge_ids[0]])
                        if pattern.path:
                            row[pattern.path.value] = path
                        rows.append(row)
                if len(edge_ids) >= maximum:
                    continue
                degree = 0
                for raw_edge in edge_candidates:
                    destinations = []
                    if direction in ("out", "both") and raw_edge["source"] == current:
                        destinations.append(raw_edge["target"])
                    if direction in ("in", "both") and raw_edge["target"] == current:
                        if raw_edge["source"] != current or direction == "in" or mutated("read-double-loop"):
                            destinations.append(raw_edge["source"])
                    for destination in destinations:
                        degree += 1
                        meter.examine(current, degree)
                        edge = active(raw_edge)
                        if edge is None:
                            continue
                        repeated = raw_edge["id"] in edge_ids
                        if repeated and not (mutated("read-repeat-edge") and edge_ids.count(raw_edge["id"]) == 1):
                            continue
                        check_properties = not mutated("read-last-edge-predicate") or len(edge_ids) + 1 == maximum
                        if not matches(rel, edge, edge=True, check_properties=check_properties):
                            continue
                        endpoint = nodes[destination]
                        if mutated("read-intermediate-endpoint") and not matches(pattern.second, endpoint):
                            continue
                        meter.visit(destination)
                        push(node_ids, edge_ids, destination, raw_edge["id"])
        if mutated("read-dedup-endpoints") and pattern.second:
            seen_endpoints = set()
            kept = []
            for row in rows:
                endpoint_id = row[pattern.second.variable.value]["id"]
                if endpoint_id not in seen_endpoints:
                    kept.append(row)
                    seen_endpoints.add(endpoint_id)
            rows = kept
        if pattern.relationship:
            def canonical_compare(left, right):
                if not mutated("read-no-canonical-work"):
                    meter.charge()
                meter.check("sort")
                a, b = row_identity(left, statement.bindings), row_identity(right, statement.bindings)
                difference = (a > b) - (a < b)
                return -difference if mutated("read-default-order") else difference

            meter.check("sort")
            meter.allocate(8 * len(rows))
            rows.sort(key=cmp_to_key(canonical_compare))
        elif mutated("read-default-order"):
            rows.reverse()
        if statement.where:
            rows = [row for row in rows if expression(statement.where, row, params, meter) is True]
        returning = statement.returning
        if mutated("read-reset-work"):
            meter.work = 0
        projected = []
        binding_types = dict(statement.bindings)
        for row in rows:
            meter.charge()
            meter.allocate(8 * (1 + len(returning.items)))
            cells = []
            for item in returning.items:
                entity = row[item.variable.value]
                if item.property:
                    value = entity["properties"].get(item.property.value, NULL)
                    meter.allocate(1 if value["tag"] == "Null" else value_size(value))
                    cells.append(copy.deepcopy(value))
                else:
                    meter.allocate(owned_result_bytes(entity, binding_types[item.variable.value]))
                    cells.append(entity_result(entity, binding_types[item.variable.value]))
            projected.append((row_identity(row, statement.bindings), cells))
        skip = int(params[returning.skip.name]["value"]) if returning.skip else 0
        limit = int(params[returning.limit.name]["value"]) if returning.limit else None
        if mutated("read-skip-first"):
            projected = projected[skip:]
            skip = 0
        if mutated("read-limit-first") and limit is not None:
            projected = projected[:limit]
        if returning.sorts:
            meter.check("sort")
            meter.allocate(8 * len(projected))
            columns = [item.column for item in returning.items]

            def compare(left, right):
                if not mutated("read-no-order-work"):
                    meter.charge()
                meter.check("sort")
                for token, direction in returning.sorts:
                    index = columns.index(token.value)
                    a, b = left[1][index], right[1][index]
                    key_a, key_b = scalar_sort_key(a), scalar_sort_key(b)
                    difference = (key_a > key_b) - (key_a < key_b)
                    if direction == "DESC":
                        if not (mutated("read-desc-null") and (a["tag"] == "Null" or b["tag"] == "Null")):
                            difference = -difference
                    if difference:
                        return difference
                tie = (left[0] > right[0]) - (left[0] < right[0])
                return -tie if mutated("read-reverse-tie") else tie

            projected.sort(key=cmp_to_key(compare))
        projected = projected[skip:]
        if limit is not None:
            projected = projected[:limit]
        meter.returned(len(projected))
        output_nodes, output_edges, paths = [], [], []
        for _, cells in projected:
            for cell in cells:
                if cell["tag"] == "Node":
                    output_nodes.append(cell["id"])
                elif cell["tag"] == "Edge":
                    output_edges.append(cell["id"])
                elif cell["tag"] == "Path":
                    paths.append((cell["length"], tuple(cell["edge_ids"]), tuple(cell["node_ids"])))
                    if not mutated("read-drop-path-nodes"):
                        output_nodes.extend(cell["node_ids"])
                    if not mutated("read-drop-path-edges"):
                        output_edges.extend(cell["edge_ids"])
        node_count = len(output_nodes) if mutated("read-output-per-row") else len(set(output_nodes))
        edge_count = len(output_edges) if mutated("read-output-per-row") else len(set(output_edges))
        path_count = len(set(paths)) if mutated("read-distinct-path-cap") else len(paths)
        meter.output(node_count, edge_count, path_count)
        meter.check("finish")
        return {"outcome": "result", "columns": [item.column for item in returning.items],
                "rows": [cells for _, cells in projected],
                "as_of_ms": None if instant is None else str(instant),
                "ttl_clock_ms": str(ttl), "view_sequence": graph["view_sequence"]}
    except Diagnostic as error:
        return {"outcome": "reject", "error": error.result()}
    except ReadFailure as error:
        return {"outcome": "reject", "error": error.result()}


# Fixture validation

SCHEMA = "vlite.graph-subset-parse"
FIXTURE = Path(__file__).resolve().parent / "fixtures" / "graph_subset"
GROUPS = frozenset(("read", "write", "lexical", "excluded", "syntax", "static",
                    "parameter", "open-cap-offset", "open-encoding",
                    "open-envelope", "open-static-offset", "open-param-domain",
                    "open-caller-hop"))


def require(condition, code):
    if not condition:
        raise ValueError("fixture: " + code)


def exact_keys(value, required, optional=(), code="unknown-fields"):
    require(isinstance(value, dict), "expected-object")
    require(set(required) <= set(value) <= set(required) | set(optional), code)


def content_hash(document):
    body = {key: value for key, value in document.items() if key != "content_hash"}
    return hashlib.sha256(json.dumps(body, ensure_ascii=False, sort_keys=True,
                                     separators=(",", ":"), allow_nan=False).encode("utf-8")).hexdigest()


def expand(form):
    if isinstance(form, str):
        return form
    exact_keys(form, ("prefix", "unit", "count", "suffix"), code="text-form")
    require(all(isinstance(form[key], str) for key in ("prefix", "unit", "suffix")), "text-form")
    require(type(form["count"]) is int and 0 <= form["count"] <= 20000, "text-form")
    return form["prefix"] + form["unit"] * form["count"] + form["suffix"]


def expand_params(params):
    if not isinstance(params, dict):
        return params
    result = copy.deepcopy(params)
    for value in result.values():
        if isinstance(value, dict) and value.get("tag") == "String" and isinstance(value.get("value"), dict):
            value["value"] = expand(value["value"])
    return result


def validate_fixture_params(params):
    for name, value in params.items():
        require(isinstance(name, str) and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name),
                "parameter-name")
        require(isinstance(value, dict), "parameter-value")
        tag = value.get("tag")
        require(isinstance(tag, str) and tag in ("String", "Integer", "Float", "Boolean", "GeoPoint"),
                "parameter-tag")
        if tag == "GeoPoint":
            exact_keys(value, ("tag", "latitude", "longitude"))
            require(all(finite_number(value[key])
                        for key in ("latitude", "longitude")), "parameter-value")
        else:
            exact_keys(value, ("tag", "value"))
            payload = value["value"]
            if tag == "String":
                payload = expand(payload)
                try:
                    payload.encode("utf-8")
                except UnicodeEncodeError as error:
                    raise ValueError("fixture: invalid-utf8") from error
            elif tag == "Integer":
                require(isinstance(payload, str) and re.fullmatch(r"-?(0|[1-9][0-9]*)", payload),
                        "parameter-value")
            elif tag == "Float":
                require(finite_number(payload), "parameter-value")
            else:
                require(type(payload) is bool, "parameter-value")


def validate_fixture(document, verify_hash=True):
    exact_keys(document, ("schema", "version", "content_hash", "cases"))
    require(document["schema"] == SCHEMA and type(document["version"]) is int
            and document["version"] == 1, "schema-version")
    require(isinstance(document["cases"], list) and bool(document["cases"]), "cases-array")
    seen = set()
    for case in document["cases"]:
        exact_keys(case, ("id", "group", "origin", "cite", "text", "expect"),
                   ("params", "params_raw", "marked", "offset_note", "max_hops"))
        require(isinstance(case["id"], str) and re.fullmatch(r"[a-z0-9][a-z0-9-]*", case["id"]), "case-id")
        require(case["id"] not in seen, "duplicate-id")
        seen.add(case["id"])
        require(isinstance(case["group"], str) and case["group"] in GROUPS, "unknown-group")
        require(case["origin"] == "local", "origin")
        require(isinstance(case["cite"], str) and bool(case["cite"]), "citation")
        require(("params" in case) != ("params_raw" in case), "parameter-source")
        require(isinstance(case.get("params", {}), dict), "params-object")
        if "params" in case:
            validate_fixture_params(case["params"])
        require("params_raw" not in case or isinstance(case["params_raw"], str), "params-raw")
        require("max_hops" not in case or type(case["max_hops"]) is int, "max-hops")
        text = expand(case["text"])
        require("\u27e6" not in text, "marker-in-text")
        try:
            text.encode("utf-8")
        except UnicodeEncodeError as error:
            raise ValueError("fixture: invalid-utf8") from error
        expect = case["expect"]
        require(isinstance(expect, dict), "expect-object")
        if expect.get("outcome") == "accept":
            exact_keys(expect, ("outcome", "kind", "columns", "references", "bindings"))
            require(expect["kind"] in ("read", "write"), "statement-kind")
            require(isinstance(expect["columns"], list) and all(isinstance(x, str) for x in expect["columns"]), "columns")
            require(len(set(expect["columns"])) == len(expect["columns"]), "duplicate-expected-column")
            for key in ("references", "bindings"):
                require(isinstance(expect[key], list), key)
                for entry in expect[key]:
                    require(isinstance(entry, list) and len(entry) == 2 and isinstance(entry[0], str), key)
                    if key == "references":
                        require(type(entry[1]) is int and 0 <= entry[1] <= len(text.encode("utf-8")), key)
                        require(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", entry[0])
                                and text.encode("utf-8")[entry[1]:entry[1] + 1] == b"$", key)
                    else:
                        require(entry[1] in ("node", "rel", "path"), key)
            require(len({entry[0] for entry in expect["bindings"]}) == len(expect["bindings"]), "bindings")
            require(all(left[1] < right[1] for left, right in zip(
                expect["references"], expect["references"][1:])), "references")
            require("marked" not in case and "offset_note" not in case, "accept-marker")
        elif expect.get("outcome") == "reject":
            exact_keys(expect, ("outcome", "error"))
            exact_keys(expect["error"], ("kind", "offset"), ("reason",))
            require(expect["error"]["kind"] in ("Syntax", "Unsupported", "Parameter", "Limit",
                                               "InvalidMetadata", "InvalidQuery",
                                               "GraphLimitExceedsCeiling"), "error-kind")
            if "reason" in expect["error"]:
                require(expect["error"]["kind"] == "InvalidQuery"
                        and expect["error"]["reason"] == "subset hop bound exceeds max_hops", "error-reason")
            offset = expect["error"]["offset"]
            require(type(offset) is int and 0 <= offset <= len(text.encode("utf-8")), "error-offset")
            try:
                text.encode("utf-8")[:offset].decode("utf-8")
            except UnicodeDecodeError as error:
                raise ValueError("fixture: error-offset") from error
            require(("marked" in case) != ("offset_note" in case), "rejection-marker")
            if "marked" in case:
                marked = case["marked"]
                require(isinstance(marked, str) and marked.count("\u27e6") == 1, "marker-count")
                require(marked.replace("\u27e6", "") == text, "marker-text")
                require(len(marked.split("\u27e6")[0].encode("utf-8")) == offset, "marker-offset")
            else:
                require(isinstance(case["offset_note"], str) and bool(case["offset_note"]), "offset-note")
        else:
            require(False, "outcome")
    require(isinstance(document["content_hash"], str)
            and re.fullmatch(r"[0-9a-f]{64}", document["content_hash"]), "hash-shape")
    if verify_hash:
        require(content_hash(document) == document["content_hash"], "bad-hash")
    return document


def run_case(case):
    return outcome(expand(case["text"]), expand_params(case.get("params")), case.get("params_raw"),
                   case.get("max_hops", 2))


READ_GROUPS = frozenset((
    "scan", "anchors", "labels", "maps", "directions", "trails", "scope",
    "logic", "equality", "comparison", "ordering", "pagination", "entities",
    "time", "limits", "clock", "not-modeled", "open-accounting",
    "open-frontier", "open-scratch", "open-clock-conversion",
))


def decimal(value, bits=64, signed=False, code="decimal"):
    require(isinstance(value, str) and re.fullmatch(r"-?(0|[1-9][0-9]*)", value)
            and value != "-0" and len(value) <= 20, code)
    lower = -(2**(bits - 1)) if signed else 0
    upper = 2**(bits - int(signed)) - 1
    require(lower <= int(value) <= upper, code)


def read_scalar(value, nullable=False):
    require(isinstance(value, dict), "cell-object")
    if value.get("tag") == "Null" and nullable:
        exact_keys(value, ("tag",))
        return
    validate_fixture_params({"value": value})
    try:
        value_size(expand_params({"value": value})["value"])
    except Diagnostic as error:
        raise ValueError("fixture: scalar-domain") from error


def read_properties(properties):
    require(isinstance(properties, dict) and len(properties) <= 64, "properties")
    for name, value in properties.items():
        require(isinstance(name, str) and 1 <= len(name.encode("utf-8")) <= 256, "property-name")
        read_scalar(value)


def read_entity(entity, edge=False, result=False):
    required = ("id", "key", "properties", "revision")
    required += ("type", "source", "target") if edge else ("labels",)
    if result:
        required += ("tag",)
    exact_keys(entity, required, () if result else (
        ("expires_at_ms", "segments") if edge else ("expires_at_ms",)))
    decimal(entity["id"], code="entity-id")
    decimal(entity["revision"], code="entity-revision")
    key = entity["key"]
    require(edge and key is None or isinstance(key, str) and 1 <= len(key.encode("utf-8")) <= 256, "entity-key")
    read_properties(entity["properties"])
    if edge:
        require(isinstance(entity["type"], str) and re.fullmatch(r"[A-Za-z0-9_]{1,63}", entity["type"]), "edge-type")
        for name in ("source", "target"):
            decimal(entity[name], code="endpoint-id")
    else:
        labels = entity["labels"]
        require(isinstance(labels, list) and len(labels) <= 8
                and all(isinstance(label, str) and re.fullmatch(r"[A-Za-z0-9_]{1,63}", label) for label in labels)
                and len(set(labels)) == len(labels), "labels")
    if "expires_at_ms" in entity:
        decimal(entity["expires_at_ms"], code="expiry")
    if "segments" in entity:
        require(isinstance(entity["segments"], list) and len(entity["segments"]) <= 256, "segments")
        previous_end = None
        for index, segment in enumerate(entity["segments"]):
            exact_keys(segment, ("from_ms", "to_ms", "properties"))
            start, end = segment["from_ms"], segment["to_ms"]
            for bound in (start, end):
                if bound is not None:
                    decimal(bound, signed=True, code="validity")
            require(start is None or end is None or int(start) < int(end), "validity")
            if index:
                require(previous_end is not None and start is not None
                        and int(previous_end) <= int(start), "segment-overlap")
            previous_end = end
            read_properties(segment["properties"])


def read_cell(cell):
    require(isinstance(cell, dict), "cell-object")
    tag = cell.get("tag")
    if tag in ("Node", "Edge"):
        read_entity(cell, edge=tag == "Edge", result=True)
    elif tag == "Path":
        exact_keys(cell, ("tag", "length", "edge_ids", "node_ids"))
        require(type(cell["length"]) is int and 1 <= cell["length"] <= 4, "path-length")
        for name, count in (("edge_ids", cell["length"]), ("node_ids", cell["length"] + 1)):
            require(isinstance(cell[name], list) and len(cell[name]) == count, "path-shape")
            for value in cell[name]:
                decimal(value, code="path-id")
        require(len(set(cell["edge_ids"])) == len(cell["edge_ids"]), "path-repeated-edge")
    else:
        read_scalar(cell, nullable=True)


def validate_read_graph(graph):
    exact_keys(graph, ("nodes", "edges", "temporal_capable", "ttl_clock_ms", "view_sequence"))
    require(type(graph["temporal_capable"]) is bool, "temporal-capability")
    decimal(graph["ttl_clock_ms"], code="ttl-clock")
    decimal(graph["view_sequence"], code="view-sequence")
    for collection, edge in (("nodes", False), ("edges", True)):
        require(isinstance(graph[collection], list), "entity-array")
        ids, keys = set(), set()
        for entity in graph[collection]:
            read_entity(entity, edge=edge)
            require(entity["id"] not in ids, "duplicate-entity-id")
            ids.add(entity["id"])
            if entity["key"] is not None:
                require(entity["key"] not in keys, "duplicate-entity-key")
                keys.add(entity["key"])
    nodes = {node["id"] for node in graph["nodes"]}
    for edge in graph["edges"]:
        require(edge["source"] in nodes and edge["target"] in nodes, "dangling-endpoint")
        require("segments" not in edge or graph["temporal_capable"], "temporal-edge-gate")


def validate_read_clock(clock):
    exact_keys(clock, (), ("ttl_clock_ms", "ttl_samples_ms", "cancelled", "cancel_at_work",
                          "cancel_phases", "elapsed_before_ms", "monotonic", "elapsed_by_phase"))
    for name in ("ttl_clock_ms",):
        if name in clock:
            decimal(clock[name], code="ttl-clock")
    if "ttl_samples_ms" in clock:
        require(isinstance(clock["ttl_samples_ms"], list), "ttl-samples")
        for value in clock["ttl_samples_ms"]:
            decimal(value, code="ttl-clock")
        require(all(int(a) <= int(b) for a, b in zip(clock["ttl_samples_ms"],
                                                    clock["ttl_samples_ms"][1:])), "ttl-samples-order")
    if "cancelled" in clock:
        require(type(clock["cancelled"]) is bool, "cancelled")
    for name in ("cancel_at_work", "elapsed_before_ms"):
        if name in clock:
            require(type(clock[name]) is int and 0 <= clock[name] <= 2**64 - 1, "clock-integer")
    phases = {"before", "interval", "sort", "finish"}
    if "cancel_phases" in clock:
        require(isinstance(clock["cancel_phases"], list)
                and all(isinstance(phase, str) and phase in phases for phase in clock["cancel_phases"]), "clock-phase")
    if "elapsed_by_phase" in clock:
        require(isinstance(clock["elapsed_by_phase"], dict)
                and set(clock["elapsed_by_phase"]) <= phases, "clock-phase")
        require(all(type(value) is int and 0 <= value <= 2**64 - 1
                    for value in clock["elapsed_by_phase"].values()), "clock-integer")
    if "monotonic" in clock:
        require(isinstance(clock["monotonic"], list), "monotonic")
        previous_work, previous_elapsed = -1, -1
        for sample in clock["monotonic"]:
            exact_keys(sample, ("at_work", "elapsed_ms"))
            require(all(type(value) is int and 0 <= value <= 2**64 - 1 for value in sample.values()), "clock-integer")
            require(sample["at_work"] > previous_work and sample["elapsed_ms"] >= previous_elapsed, "monotonic-order")
            previous_work, previous_elapsed = sample["at_work"], sample["elapsed_ms"]


def validate_read_fixture(document, verify_hash=True):
    exact_keys(document, ("schema", "version", "content_hash", "graphs", "cases"))
    require(document["schema"] == "vlite.graph-subset-read"
            and type(document["version"]) is int and document["version"] == 1, "read-schema-version")
    require(isinstance(document["graphs"], dict), "graphs-object")
    for name, graph in document["graphs"].items():
        require(isinstance(name, str) and re.fullmatch(r"[a-z0-9][a-z0-9-]*", name), "graph-id")
        validate_read_graph(graph)
    require(isinstance(document["cases"], list) and bool(document["cases"]), "cases-array")
    seen = set()
    for case in document["cases"]:
        exact_keys(case, ("id", "group", "origin", "cite", "graph", "params", "clock", "limits", "text", "expect"))
        require(isinstance(case["id"], str) and re.fullmatch(r"[a-z0-9][a-z0-9-]*", case["id"]), "case-id")
        require(case["id"] not in seen, "duplicate-id")
        seen.add(case["id"])
        require(isinstance(case["group"], str) and case["group"] in READ_GROUPS, "unknown-group")
        require(case["origin"] == "local", "origin")
        require(isinstance(case["cite"], str) and bool(case["cite"]), "citation")
        if isinstance(case["graph"], str):
            require(case["graph"] in document["graphs"], "dangling-graph-id")
        else:
            validate_read_graph(case["graph"])
        require(isinstance(case["params"], dict), "params-object")
        validate_fixture_params(case["params"])
        validate_read_clock(case["clock"])
        exact_keys(case["limits"], (), tuple(READ_DEFAULTS))
        require(all(type(value) is int and 0 <= value <= 2**64 - 1 for value in case["limits"].values()), "limit-value")
        text = expand(case["text"])
        try:
            text.encode("utf-8")
        except UnicodeEncodeError as error:
            raise ValueError("fixture: invalid-utf8") from error
        expect = case["expect"]
        require(isinstance(expect, dict), "expect-object")
        if expect.get("outcome") == "result":
            exact_keys(expect, ("outcome", "columns", "rows", "as_of_ms", "ttl_clock_ms", "view_sequence"))
            require(isinstance(expect["columns"], list) and all(isinstance(name, str) for name in expect["columns"])
                    and len(set(expect["columns"])) == len(expect["columns"]), "columns")
            require(isinstance(expect["rows"], list), "rows-array")
            for row in expect["rows"]:
                require(isinstance(row, list) and len(row) == len(expect["columns"]), "row-width")
                for cell in row:
                    read_cell(cell)
            if expect["as_of_ms"] is not None:
                decimal(expect["as_of_ms"], signed=True, code="as-of")
            decimal(expect["ttl_clock_ms"], code="ttl-clock")
            decimal(expect["view_sequence"], code="view-sequence")
        elif expect.get("outcome") == "reject":
            exact_keys(expect, ("outcome", "error"))
            error = expect["error"]
            exact_keys(error, ("kind",), ("offset", "reason", "feature"))
            require(error["kind"] in ("Syntax", "Unsupported", "Parameter", "Limit", "InvalidMetadata",
                                     "InvalidQuery", "GraphLimitExceedsCeiling", "UnsupportedFeature"), "error-kind")
            if "offset" in error:
                require(type(error["offset"]) is int and 0 <= error["offset"] <= len(text.encode("utf-8")), "error-offset")
                try:
                    text.encode("utf-8")[:error["offset"]].decode("utf-8")
                except UnicodeDecodeError as exception:
                    raise ValueError("fixture: error-offset") from exception
            if error["kind"] in ("Syntax", "Unsupported", "Parameter", "Limit"):
                exact_keys(error, ("kind", "offset"))
            elif error["kind"] == "GraphLimitExceedsCeiling":
                exact_keys(error, ("kind",))
            elif error["kind"] != "UnsupportedFeature":
                require("feature" not in error, "error-feature")
            if "reason" in error:
                require(error["kind"] == "InvalidQuery"
                        and error["reason"] in READ_REASONS, "error-reason")
            if error["kind"] == "UnsupportedFeature":
                require(error == {"kind": "UnsupportedFeature", "feature": 64}, "error-feature")
        elif expect.get("outcome") == "NOT_MODELED":
            exact_keys(expect, ("outcome",))
        else:
            require(False, "outcome")
    require(isinstance(document["content_hash"], str)
            and re.fullmatch(r"[0-9a-f]{64}", document["content_hash"]), "hash-shape")
    if verify_hash:
        require(document["content_hash"] == content_hash(document), "bad-hash")
    return document


def run_read_case(case, graphs):
    graph = graphs[case["graph"]] if isinstance(case["graph"], str) else case["graph"]
    return evaluate_read(graph, expand(case["text"]), expand_params(case["params"]), case["limits"], case["clock"])


def read_malformed_trials(document):
    trials = []
    for location in ("root", "case", "graph", "node", "clock", "limits", "expect", "cell"):
        bad = copy.deepcopy(document)
        first = bad["cases"][0]
        graph = next(graph for graph in bad["graphs"].values() if graph["nodes"])
        target = {"root": bad, "case": first, "graph": graph, "node": graph["nodes"][0],
                  "clock": first["clock"], "limits": first["limits"], "expect": first["expect"],
                  "cell": first["expect"]["rows"][0][0]}[location]
        target["extra"] = 1
        trials.append((bad, "unknown-fields"))
    for field_name, value, code in (
        ("group", "unknown", "unknown-group"), ("graph", "unknown", "dangling-graph-id"),
        ("id", "BAD", "case-id"), ("params", [], "params-object"),
        ("clock", {"cancelled": 1}, "cancelled"), ("limits", {"max_hops": True}, "limit-value"),
    ):
        bad = copy.deepcopy(document)
        bad["cases"][0][field_name] = value
        trials.append((bad, code))
    bad = copy.deepcopy(document)
    bad["cases"].append(copy.deepcopy(bad["cases"][0]))
    trials.append((bad, "duplicate-id"))
    bad = copy.deepcopy(document)
    bad["content_hash"] = "0" * 64
    trials.append((bad, "bad-hash"))
    for field_name, value, code in (
        ("version", True, "read-schema-version"),
        ("schema", "unknown", "read-schema-version"),
        ("content_hash", "short", "hash-shape"),
        ("graphs", [], "graphs-object"),
        ("cases", [], "cases-array"),
    ):
        bad = copy.deepcopy(document)
        bad[field_name] = value
        trials.append((bad, code))
    for path, value, code in (
        (("graphs", "scan", "nodes", 0, "id"), "18446744073709551616", "entity-id"),
        (("graphs", "scan", "nodes", 0, "labels"), ["Person", "Person"], "labels"),
        (("graphs", "scan", "nodes", 0, "properties", "v", "value"), "9223372036854775808", "scalar-domain"),
        (("graphs", "line", "edges", 0, "target"), "99", "dangling-endpoint"),
        (("graphs", "line", "edges", 0, "extra"), True, "unknown-fields"),
        (("graphs", "temporal", "edges", 0, "segments", 0, "extra"), True, "unknown-fields"),
        (("graphs", "temporal", "edges", 0, "segments", 1, "from_ms"), "-1", "segment-overlap"),
        (("cases", 0, "expect", "rows", 0), [], "row-width"),
        (("cases", 0, "clock"), {"monotonic": [{"at_work": 2, "elapsed_ms": 5},
                                            {"at_work": 1, "elapsed_ms": 6}]}, "monotonic-order"),
    ):
        bad = copy.deepcopy(document)
        target = bad
        for component in path[:-1]:
            target = target[component]
        target[path[-1]] = value
        trials.append((bad, code))
    for bad, code in trials:
        try:
            validate_read_fixture(bad)
        except ValueError as error:
            same(str(error), "fixture: " + code, "malformed read fixture")
        else:
            raise ValueError("malformed read fixture accepted: " + code)
    return len(trials)


def validate_read_mutants(document, cases):
    require("read_mutants" in document and isinstance(document["read_mutants"], list), "read-mutants-array")
    lookup = {case["id"] for case in cases}
    seen = set()
    for entry in document["read_mutants"]:
        exact_keys(entry, ("name", "change", "cases"))
        require(isinstance(entry["name"], str) and entry["name"] in READ_MUTATIONS
                and entry["name"] not in seen, "read-mutant-name")
        seen.add(entry["name"])
        require(isinstance(entry["change"], str) and bool(entry["change"]), "read-mutant-change")
        require(isinstance(entry["cases"], list) and bool(entry["cases"])
                and all(isinstance(case_id, str) for case_id in entry["cases"]), "read-mutant-cases")
        require(len(set(entry["cases"])) == len(entry["cases"]), "read-mutant-cases")
        require(all(case_id in lookup for case_id in entry["cases"]), "read-mutant-case-id")
    require(seen == set(READ_MUTATIONS), "read-mutant-coverage")
    require(content_hash(document) == document["content_hash"], "mutant-bad-hash")


def read_mutant_trials(document, cases, graphs):
    global MUTATION
    validate_read_mutants(document, cases)
    lookup = {case["id"]: case for case in cases}
    named_kills = 0
    try:
        for entry in document["read_mutants"]:
            MUTATION = entry["name"]
            for case_id in entry["cases"]:
                case = lookup[case_id]
                actual = run_read_case(case, graphs)
                if actual == case["expect"]:
                    raise ValueError(f"surviving read mutant {MUTATION}: {case_id}")
                named_kills += 1
    finally:
        MUTATION = None
    return len(document["read_mutants"]), named_kills


def malformed_read_mutant_trials(document, cases):
    trials = []
    for field_name, value, code in (
        ("name", "unknown", "read-mutant-name"), ("cases", ["unknown"], "read-mutant-case-id"),
        ("cases", [], "read-mutant-cases"), ("change", "", "read-mutant-change"),
        ("extra", 1, "unknown-fields"),
    ):
        bad = copy.deepcopy(document)
        bad["read_mutants"][0][field_name] = value
        trials.append((bad, code))
    bad = copy.deepcopy(document)
    bad["read_mutants"].append(copy.deepcopy(bad["read_mutants"][0]))
    trials.append((bad, "read-mutant-name"))
    bad = copy.deepcopy(document)
    bad["read_mutants"].pop()
    trials.append((bad, "read-mutant-coverage"))
    bad = copy.deepcopy(document)
    bad["content_hash"] = "0" * 64
    trials.append((bad, "mutant-bad-hash"))
    for bad, code in trials:
        try:
            validate_read_mutants(bad, cases)
        except ValueError as error:
            same(str(error), "fixture: " + code, "malformed read mutant map")
        else:
            raise ValueError("malformed read mutant map accepted: " + code)
    return len(trials)


def read_boundary_trials():
    trials = 0
    checks = (
        ("max_visited_nodes", "visited-node budget exhausted", lambda meter, n: meter.visit(str(n))),
        ("max_edges_examined", "edge budget exhausted", lambda meter, n: meter.examine("1", 1)),
        ("max_edges_per_node", "degree budget exhausted", lambda meter, n: meter.examine("1", n)),
        ("max_frontier_entries", "frontier budget exhausted", lambda meter, n: meter.frontier_insert(n)),
        ("max_binding_rows", "binding-row budget exhausted", lambda meter, n: meter.binding_insert(n)),
        ("max_returned_rows", "result-row budget exhausted", lambda meter, n: meter.returned(n)),
        ("max_work_units", "work budget exhausted", lambda meter, n: meter.charge()),
        ("max_scratch_bytes", "scratch budget exhausted", lambda meter, n: meter.allocate(1)),
        ("max_output_nodes", "output budget exhausted", lambda meter, n: meter.output(n, 0, 0)),
        ("max_output_edges", "output budget exhausted", lambda meter, n: meter.output(0, n, 0)),
        ("max_output_paths", "output budget exhausted", lambda meter, n: meter.output(0, 0, n)),
    )
    for name, reason, action in checks:
        meter = ReadMeter(read_limits({name: 2}), {})
        action(meter, 1)
        action(meter, 2)
        try:
            action(meter, 3)
        except ReadFailure as error:
            same(error.result(), {"kind": "InvalidQuery", "reason": "subset " + reason}, name)
        else:
            raise ValueError("missing accounting boundary: " + name)
        trials += 3
    for name, ceiling in READ_CEILINGS.items():
        for requested in (ceiling - 1, ceiling):
            same(read_limits({name: requested})[name], requested, name + " caller boundary")
        try:
            read_limits({name: ceiling + 1})
        except ReadFailure as error:
            same(error.result(), {"kind": "GraphLimitExceedsCeiling"}, name + " ceiling")
        else:
            raise ValueError("missing caller ceiling: " + name)
        trials += 3
    # These exercise meter arithmetic with declared bytes/entries, not inferred
    # graph storage sizes or a fabricated frontier traversal schedule.
    return trials


def check_read_paths(result, graph, text, params):
    if result["outcome"] != "result":
        return
    statement = parse(text, params, max_hops=4)
    if not statement.pattern.relationship:
        return
    direction = statement.pattern.relationship.direction
    maximum = statement.pattern.relationship.hops or 1
    nodes = {node["id"]: node for node in graph["nodes"]}
    edges = {edge["id"]: edge for edge in graph["edges"]}
    ttl = int(result["ttl_clock_ms"])
    instant = None if result["as_of_ms"] is None else int(result["as_of_ms"])
    for row in result["rows"]:
        for cell in row:
            if cell["tag"] != "Path":
                continue
            require(1 <= cell["length"] <= maximum, "returned-path-bound")
            require(len(set(cell["edge_ids"])) == cell["length"], "returned-path-simple")
            for node_id in cell["node_ids"]:
                require(node_id in nodes, "returned-path-node")
                expiry = nodes[node_id].get("expires_at_ms")
                require(expiry is None or ttl < int(expiry), "returned-path-ttl")
            for source, edge_id, target in zip(cell["node_ids"], cell["edge_ids"], cell["node_ids"][1:]):
                require(edge_id in edges, "returned-path-edge")
                edge = edges[edge_id]
                forward = (source, target) == (edge["source"], edge["target"])
                backward = (target, source) == (edge["source"], edge["target"])
                require(direction == "out" and forward or direction == "in" and backward
                        or direction == "both" and (forward or backward), "returned-path-direction")
                expiry = edge.get("expires_at_ms")
                require(expiry is None or ttl < int(expiry), "returned-path-ttl")
                if "segments" in edge:
                    matches = [segment for segment in edge["segments"]
                               if (segment["from_ms"] is None or int(segment["from_ms"]) <= instant)
                               and (segment["to_ms"] is None or instant < int(segment["to_ms"]))]
                    require(len(matches) == 1, "returned-path-validity")


def read_self_test(directory):
    document = validate_read_fixture(unique_json((directory / "read-cases.json").read_text(encoding="utf-8")))
    cases, graphs = document["cases"], document["graphs"]
    require(len(cases) >= 100, "minimum-read-cases")
    pristine = copy.deepcopy(document)
    counts = {}
    for case in cases:
        same(run_read_case(case, graphs), case["expect"], case["id"])
        graph = graphs[case["graph"]] if isinstance(case["graph"], str) else case["graph"]
        check_read_paths(case["expect"], graph, expand(case["text"]), expand_params(case["params"]))
        counts[case["group"]] = counts.get(case["group"], 0) + 1
    malformed = read_malformed_trials(document)
    mutant_map = unique_json((directory / "mutant-map.json").read_text(encoding="utf-8"))
    malformed += malformed_read_mutant_trials(mutant_map, cases)
    boundaries = read_boundary_trials()
    for seed in (17, 941, 2026, 73, 65537):
        shuffled = copy.deepcopy(document)
        randomizer = random.Random(seed)
        randomizer.shuffle(shuffled["cases"])
        for graph in shuffled["graphs"].values():
            randomizer.shuffle(graph["nodes"])
            randomizer.shuffle(graph["edges"])
        for case in shuffled["cases"]:
            if isinstance(case["graph"], dict):
                randomizer.shuffle(case["graph"]["nodes"])
                randomizer.shuffle(case["graph"]["edges"])
            same(run_read_case(case, shuffled["graphs"]), case["expect"], case["id"] + " graph shuffle")
    killed, named_kills = read_mutant_trials(mutant_map, cases, graphs)
    require(killed >= 40, "minimum-read-mutants")
    same(document, pristine, "read input immutability")
    for group, count in sorted(counts.items()):
        print(f"read/{group}: {count}")
    print(f"read-cases={len(cases)} read-malformed={malformed} read-boundaries={boundaries} "
          f"graph-shuffle-seeds=5 read-mutants-killed={killed} read-named-kills={named_kills}")
    return document


# Self-test: explicit raises remain active under python -O.

def same(actual, expected, context):
    if actual != expected:
        raise ValueError(f"{context}: expected {expected!r}, got {actual!r}")


def semantic_signature(statement):
    # Formatting intentionally changes offsets but not the statement tree.
    return format_statement(statement), statement.bindings, [
        (ref.name, ref.use) for ref in statement.references]


def malformed_trials(document):
    trials = []
    bad = copy.deepcopy(document)
    bad["surprise"] = True
    trials.append((bad, "unknown-fields"))
    bad = copy.deepcopy(document)
    bad["cases"][0]["surprise"] = True
    trials.append((bad, "unknown-fields"))
    bad = copy.deepcopy(document)
    bad["cases"].append(copy.deepcopy(bad["cases"][0]))
    trials.append((bad, "duplicate-id"))
    bad = copy.deepcopy(document)
    bad["cases"][0]["group"] = "unknown"
    trials.append((bad, "unknown-group"))
    bad = copy.deepcopy(document)
    bad["content_hash"] = "0" * 64
    trials.append((bad, "bad-hash"))
    rejected = next(i for i, case in enumerate(document["cases"]) if "marked" in case)
    for field_name, replacement, code in (
            ("marked", "no marker", "marker-count"),
            ("expect", {"outcome": "nope"}, "outcome"),
            ("params", [], "params-object")):
        bad = copy.deepcopy(document)
        bad["cases"][rejected][field_name] = replacement
        trials.append((bad, code))
    accepted = next(i for i, case in enumerate(document["cases"]) if case["expect"]["outcome"] == "accept")
    for field_name, replacement, code in (
            ("schema", "unknown", "schema-version"),
            ("version", True, "schema-version"),
            ("cases", [], "cases-array"),
            ("content_hash", "short", "hash-shape")):
        bad = copy.deepcopy(document)
        bad[field_name] = replacement
        trials.append((bad, code))
    for field_name, replacement, code in (
            ("origin", "external", "origin"),
            ("cite", "", "citation"),
            ("id", "Has spaces", "case-id"),
            ("text", {"prefix": "", "unit": " ", "count": -1, "suffix": ""}, "text-form"),
            ("text", "\ud800", "invalid-utf8"),
            ("params", {"v": {"tag": "String", "value": "x", "extra": 1}}, "unknown-fields"),
            ("params", {"v": {"tag": "Unknown", "value": 1}}, "parameter-tag"),
            ("params", {"v": {"tag": "Boolean", "value": 1}}, "parameter-value"),
            ("expect", {"outcome": "accept", "kind": "read", "columns": [], "references": [],
                        "bindings": [], "extra": 1}, "unknown-fields")):
        bad = copy.deepcopy(document)
        bad["cases"][accepted][field_name] = replacement
        trials.append((bad, code))
    bad = copy.deepcopy(document)
    bad["cases"][accepted]["params_raw"] = "{}"
    trials.append((bad, "parameter-source"))
    bad = copy.deepcopy(document)
    bad["cases"][accepted]["expect"]["bindings"] = [["n", "scalar"]]
    trials.append((bad, "bindings"))
    bad = copy.deepcopy(document)
    bad["cases"][accepted]["expect"]["references"] = [["p", 0]]
    trials.append((bad, "references"))
    bad = copy.deepcopy(document)
    bad["cases"][rejected]["marked"] += "x"
    trials.append((bad, "marker-text"))
    bad = copy.deepcopy(document)
    bad["cases"][rejected]["expect"]["error"]["offset"] += 1
    trials.append((bad, "marker-offset"))
    for bad, code in trials:
        try:
            validate_fixture(bad)
        except ValueError as error:
            same(str(error), "fixture: " + code, "malformed fixture")
        else:
            raise ValueError("malformed fixture accepted: " + code)
    raw_trials = (
        ('{"schema":1,"schema":2}', "duplicate-json-key"),
        ('{"schema":', "invalid-json"),
        ('{"value":NaN}', "invalid-json"),
    )
    for raw, code in raw_trials:
        try:
            unique_json(raw)
        except ValueError as error:
            same(str(error), "fixture: " + code, "malformed raw fixture")
        else:
            raise ValueError("malformed raw fixture accepted: " + code)
    return len(trials) + len(raw_trials)


def fuzz_trials(cases):
    accepted = [case for case in cases if case["expect"]["outcome"] == "accept"]
    compact = [case for case in accepted if isinstance(case["text"], str)]
    randomizer = random.Random(20261103)
    fragments = (" ", "\t", "\u00a0", "\u2003", "\u00e9", "\U0001f600",
                 "`", "$", "->", "<=", "..", ";", "/*", "NOT", "\x00", "[", "")
    for case in accepted:
        text, params = expand(case["text"]), expand_params(case.get("params"))
        raw = case.get("params_raw")
        first = parse(text, params, raw, case.get("max_hops", 2))
        formatted = format_statement(first)
        second = parse(formatted, params, raw, case.get("max_hops", 2))
        same(semantic_signature(second), semantic_signature(first), case["id"] + " round-trip")
    for index in range(600):
        case = randomizer.choice(compact)
        original = case["text"]
        position = randomizer.randrange(len(original) + 1)
        end = min(position + randomizer.randrange(3), len(original))
        text = original[:position] + randomizer.choice(fragments) + original[end:]
        result = run_case(dict(case, text=text))
        if result["outcome"] == "reject":
            offset = result["error"]["offset"]
            boundaries = {len(text[:i].encode("utf-8")) for i in range(len(text) + 1)}
            if offset not in boundaries:
                raise ValueError(f"fuzz {index}: offset not on UTF-8 boundary")
        else:
            first = parse(text, expand_params(case.get("params")), case.get("params_raw"), case.get("max_hops", 2))
            second = parse(format_statement(first), expand_params(case.get("params")), case.get("params_raw"),
                           case.get("max_hops", 2))
            same(semantic_signature(second), semantic_signature(first), f"fuzz {index} round-trip")
    raw_template = '{"v":{"tag":"String","value":"data"}}'
    raw_fragments = ('"', "\\ud800", "\\u0000", "é", "😀", "null", "NaN",
                     '{"v":1,"v":2}', "[]", "true", "1e999", ",", "\x00")
    text = "MATCH (n {x:$v}) RETURN n"
    for index in range(200):
        start = randomizer.randrange(len(raw_template) + 1)
        end = min(start + randomizer.randrange(4), len(raw_template))
        raw = raw_template[:start] + randomizer.choice(raw_fragments) + raw_template[end:]
        result = outcome(text, params_raw=raw)
        if result["outcome"] == "reject":
            position = result["error"]["offset"]
            if not 0 <= position <= len(text):
                raise ValueError(f"parameter fuzz {index}: offset outside statement")
    return 800


def boundary_trials(cases):
    lookup = {case["id"]: case for case in cases}
    for suffix, size in (("minus-one", 16383), ("", 16384), ("plus-one", 16385)):
        case = lookup["bytes-cap" + ("-" + suffix if suffix else "")]
        same(len(expand(case["text"]).encode("utf-8")), size, "byte boundary")
    for case_id, count in (("tokens-cap-minus-one", 1023), ("tokens-cap", 1024)):
        same(len(lex(expand(lookup[case_id]["text"]))) - 1, count, "token boundary")
    for suffix, count in (("minus-one", 63), ("", 64), ("plus-one", 65)):
        name = "parameter-count-cap" + ("-" + suffix if suffix else "")
        same(len(lookup[name]["params"]), count, "parameter count boundary")
    for suffix, size in (("minus-one", 255), ("", 256), ("plus-one", 257)):
        name = "parameter-name-cap" + ("-" + suffix if suffix else "")
        case = lookup[name]
        parameter_name = next(iter(case["params"]))
        same(len(parameter_name.encode("utf-8")), size, "parameter name boundary")
        same(parameter_name, re.search(r"\$([A-Za-z_][A-Za-z0-9_]*)", expand(case["text"])).group(1),
             "parameter name/ref agreement")
    for suffix, size in (("minus-one", 65535), ("", 65536), ("plus-one", 65537)):
        name = "parameter-bytes-cap" + ("-" + suffix if suffix else "")
        params = expand_params(lookup[name]["params"])
        encoded = 4 + sum(2 + len(key.encode("utf-8")) + value_size(value) for key, value in params.items())
        same(encoded, size, "metadata parameter encoding boundary")
    return 14


def validate_mutant_map(document, cases):
    exact_keys(document, ("schema", "version", "content_hash", "mutants"), ("read_mutants",))
    require(document["schema"] == "vlite.graph-subset-mutants"
            and type(document["version"]) is int and document["version"] == 1, "mutant-schema")
    require(isinstance(document["mutants"], list), "mutants-array")
    lookup = {case["id"]: case for case in cases}
    seen = set()
    for mutant in document["mutants"]:
        exact_keys(mutant, ("name", "change", "cases"))
        name = mutant["name"]
        require(isinstance(name, str) and name in MUTATIONS and name not in seen, "mutant-name")
        seen.add(name)
        require(isinstance(mutant["change"], str) and bool(mutant["change"]), "mutant-change")
        require(isinstance(mutant["cases"], list) and bool(mutant["cases"])
                and all(isinstance(case_id, str) for case_id in mutant["cases"]), "mutant-cases")
        require(len(set(mutant["cases"])) == len(mutant["cases"]), "mutant-cases")
        require(all(case_id in lookup for case_id in mutant["cases"]), "mutant-case-id")
    require(seen == set(MUTATIONS), "mutant-coverage")
    require(document["content_hash"] == content_hash(document), "mutant-bad-hash")
    return document


def malformed_mutant_trials(document, cases):
    trials = []
    bad = copy.deepcopy(document)
    bad["extra"] = 1
    trials.append((bad, "unknown-fields"))
    bad = copy.deepcopy(document)
    bad["mutants"][0]["extra"] = 1
    trials.append((bad, "unknown-fields"))
    bad = copy.deepcopy(document)
    bad["version"] = True
    trials.append((bad, "mutant-schema"))
    bad = copy.deepcopy(document)
    bad["mutants"].append(copy.deepcopy(bad["mutants"][0]))
    trials.append((bad, "mutant-name"))
    bad = copy.deepcopy(document)
    bad["mutants"][0]["name"] = "unknown"
    trials.append((bad, "mutant-name"))
    bad = copy.deepcopy(document)
    bad["mutants"][0]["cases"] = ["unknown"]
    trials.append((bad, "mutant-case-id"))
    bad = copy.deepcopy(document)
    bad["content_hash"] = "0" * 64
    trials.append((bad, "mutant-bad-hash"))
    for bad, code in trials:
        try:
            validate_mutant_map(bad, cases)
        except ValueError as error:
            same(str(error), "fixture: " + code, "malformed mutant map")
        else:
            raise ValueError("malformed mutant map accepted: " + code)
    return len(trials)


def mutant_trials(directory, cases):
    global MUTATION
    document = validate_mutant_map(unique_json((directory / "mutant-map.json").read_text(encoding="utf-8")), cases)
    lookup = {case["id"]: case for case in cases}
    try:
        for mutant in document["mutants"]:
            name = mutant["name"]
            MUTATION = name
            for case_id in mutant["cases"]:
                case = lookup[case_id]
                actual = run_case(case)
                if actual == case["expect"]:
                    raise ValueError(f"surviving mutant {name}: {case_id}")
    finally:
        MUTATION = None
    return len(document["mutants"])


def self_test(directory):
    document = validate_fixture(unique_json((directory / "parse-cases.json").read_text(encoding="utf-8")))
    cases = document["cases"]
    require(len(cases) >= 120, "minimum-cases")
    require(sum(case["expect"]["outcome"] == "accept" for case in cases) >= 40, "minimum-accepts")
    require(sum(case["expect"]["outcome"] == "reject" for case in cases) >= 60, "minimum-rejects")
    require(sum(any(ord(char) > 127 for char in expand(case["text"])) for case in cases) >= 15,
            "minimum-unicode")
    counts = {}
    for case in cases:
        same(run_case(case), case["expect"], case["id"])
        group = counts.setdefault(case["group"], [0, 0])
        group[0 if case["expect"]["outcome"] == "accept" else 1] += 1
    malformed = malformed_trials(document)
    mutant_map = unique_json((directory / "mutant-map.json").read_text(encoding="utf-8"))
    malformed += malformed_mutant_trials(mutant_map, cases)
    boundaries = boundary_trials(cases)
    for seed in (17, 941, 2026, 73, 65537):
        shuffled = copy.deepcopy(cases)
        randomizer = random.Random(seed)
        randomizer.shuffle(shuffled)
        for case in shuffled:
            if "params" in case:
                entries = list(case["params"].items())
                randomizer.shuffle(entries)
                case["params"] = dict(entries)
            same(run_case(case), case["expect"], case["id"] + " shuffle")
    fuzzed = fuzz_trials(cases)
    killed = mutant_trials(directory, cases)
    require(killed >= 30, "minimum-mutants")
    for group, (accepted, rejected) in sorted(counts.items()):
        print(f"{group}: accept={accepted} reject={rejected}")
    print(f"cases={len(cases)} malformed={malformed} boundaries={boundaries} "
          f"shuffle-seeds=5 fuzz={fuzzed} mutants-killed={killed}")
    read_self_test(directory)
    print("SELF-TEST OK")


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("--self-test", action="store_true", required=True)
    cli.add_argument("--fixture", type=Path, default=FIXTURE, help="fixture directory")
    args = cli.parse_args()
    try:
        self_test(args.fixture)
    except (ValueError, OSError, Diagnostic) as error:
        print(f"SELF-TEST FAILED: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
