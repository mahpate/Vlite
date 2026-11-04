#!/usr/bin/env python3
"""Independent lease-ledger v1 reference, derived from the two design documents.

Only Python 3.11's standard library is used. Expectations are frozen JSON bytes
and handwritten state assertions, never production codec output. See the report
for deliberately unmodeled, underspecified behavior.
"""

from __future__ import annotations

import argparse
import copy
import json
import struct
import sys
import uuid
import zlib
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Callable


FLAG = 0x0080
MAX_U64 = (1 << 64) - 1
MAX_TTL = 86_400_000
MAX_ROWS = 4096
MAX_BODY = 2048
DOMAIN = "00112233-4455-4677-8899-aabbccddeeff"
OTHER_DOMAIN = "10213243-5465-4768-899a-abbccddeeff0"
KINDS = {
    "DomainInit": 1, "Reserve": 2, "Acquire": 3, "Renew": 4,
    "Revoke": 5, "StartupBarrier": 6, "RecoveryRequired": 7,
    "SnapshotGrant": 8,
}
GRANT_KINDS = {3, 4, 5, 8}
FIXTURES = Path(__file__).parent / "fixtures" / "kv" / "lease-ledger-v1"


class OracleError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


def fail(code: str = "Corrupt") -> None:
    raise OracleError(code)


def require(condition: bool, message: str) -> None:
    """Checks must remain effective under python -O."""
    if not condition:
        raise AssertionError(message)


def expect_error(code: str, action: Callable[[], object]) -> None:
    try:
        action()
    except OracleError as error:
        require(error.code == code, f"expected {code}, got {error.code}")
    else:
        raise AssertionError(f"expected {code}, operation succeeded")


def expect_rejected(action: Callable[[], object]) -> None:
    """Some required refusals have no specified concrete error taxonomy."""
    try:
        action()
    except OracleError:
        return
    raise AssertionError("expected rejection, operation succeeded")


@dataclass(frozen=True)
class Rules:
    """One named fault switches one actual rule for the mutation campaign."""

    fault: str = ""

    def broken(self, name: str) -> bool:
        return self.fault == name


def valid_domain(value: str, rules: Rules = Rules()) -> bytes:
    if not isinstance(value, str):
        fail("InvalidInput")
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError):
        fail("InvalidInput")
    if not rules.broken("domain_format") and (
        str(parsed) != value or parsed.version != 4
        or parsed.variant != uuid.RFC_4122
    ):
        fail("InvalidInput")
    return parsed.bytes


def checked_text(value: str, maximum: int, rules: Rules = Rules()) -> bytes:
    if not isinstance(value, str):
        fail("InvalidInput")
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError:
        fail("InvalidInput")
    size = len(value) if rules.broken("character_count") else len(encoded)
    if not 1 <= size <= maximum:
        fail("InvalidInput")
    return encoded


@dataclass(frozen=True)
class Frame:
    kind: str
    domain: str = DOMAIN
    high_water: int = 0
    token: int = 0
    deadline: int = 0
    collection: int = 0
    namespace: str = ""
    key: str = ""
    holder: str = ""
    active: bool = False
    sequence: int = 1


def validate_frame(frame: Frame, rules: Rules = Rules()) -> tuple[bytes, ...]:
    kind = KINDS.get(frame.kind)
    if kind is None:
        fail()
    try:
        domain = valid_domain(frame.domain, rules)
        fields = tuple(value.encode("utf-8") for value in (
            frame.namespace, frame.key, frame.holder,
        ))
    except (OracleError, UnicodeEncodeError, AttributeError):
        fail()
    if not all(type(value) is int and 0 <= value <= MAX_U64 for value in (
        frame.high_water, frame.token, frame.deadline, frame.sequence,
    )):
        fail()
    if not 0 <= frame.collection <= 65535:
        fail()
    if kind in GRANT_KINDS:
        if not rules.broken("field_bounds") and not all(
            1 <= len(field) <= maximum
            for field, maximum in zip(fields, (255, 1024, 255))
        ):
            fail()
        if not rules.broken("zero_token") and frame.token == 0:
            fail()
        if frame.token > frame.high_water:
            fail()
        # A nonzero TTL added to an unsigned Unix clock cannot produce zero.
        if not rules.broken("zero_deadline") and frame.deadline == 0:
            fail()
        if kind in {3, 4} and not frame.active:
            fail()
        if kind == 5 and frame.active:
            fail()
    else:
        if frame.collection or any(fields) or frame.deadline or frame.active:
            fail()
        if kind == 2:
            if frame.token == 0 or frame.token != frame.high_water:
                fail()
        elif frame.token:
            fail()
    return (domain,) + fields


def encode_frame(frame: Frame, rules: Rules = Rules()) -> bytes:
    domain, namespace, key, holder = validate_frame(frame, rules)
    flags = (1 if frame.kind == "RecoveryRequired" else 0) | (
        2 if frame.active else 0
    )
    body = struct.pack(
        "<4sHBB16sQQQHHHHQ", b"VLL1", 1, KINDS[frame.kind], flags,
        domain, frame.high_water, frame.token, frame.deadline,
        len(namespace), len(key), len(holder), 0, 0,
    ) + namespace + key + holder
    if len(body) > MAX_BODY:
        fail()
    prefix = struct.pack(
        "<4sBHBQQII", b"VREC", 18, frame.collection, 0, frame.sequence,
        0, 0, len(body),
    )
    prefix += struct.pack("<I", zlib.crc32(prefix))
    packet = prefix + body
    return packet + struct.pack(
        "<4sIQ", b"VCMT", zlib.crc32(packet), frame.sequence,
    )


def decode_frame(
    packet: bytes, *, file_flags: int = FLAG, file_version: int = 5,
    supported_flags: int = 0x00FF, loaded_sequence: int = 0,
    rules: Rules = Rules(),
) -> Frame:
    unsupported = file_flags & ~supported_flags
    if unsupported and not rules.broken("older_reader"):
        fail(f"UnsupportedFeature(0x{unsupported:04x})")
    if file_version != 5 or (
        not file_flags & FLAG and not rules.broken("missing_flag")
    ):
        fail()
    if len(packet) < 36:
        fail("Truncated")
    magic, op, collection, reserved, sequence, identity, count, length, crc = (
        struct.unpack("<4sBHBQQIII", packet[:36])
    )
    if magic != b"VREC" or op != 18 or reserved or identity or count:
        fail()
    if not rules.broken("prefix_crc") and zlib.crc32(packet[:32]) != crc:
        fail()
    if sequence != loaded_sequence + 1 and not rules.broken("sequence"):
        fail()
    if not rules.broken("body_bound") and not 64 <= length <= MAX_BODY:
        fail()
    expected_length = 36 + length + 16
    if len(packet) < expected_length:
        fail("Truncated")
    if len(packet) != expected_length:
        fail()
    footer_magic, checksum, footer_sequence = struct.unpack(
        "<4sIQ", packet[-16:],
    )
    if footer_magic != b"VCMT" or footer_sequence != sequence:
        fail()
    if not rules.broken("footer_crc") and zlib.crc32(packet[:-16]) != checksum:
        fail()
    body = packet[36:-16]
    if len(body) < 64:
        fail()
    (magic, version, kind, flags, domain, high_water, token, deadline,
     namespace_len, key_len, holder_len, reserved16, reserved64) = (
        struct.unpack("<4sHBB16sQQQHHHHQ", body[:64])
    )
    if magic != b"VLL1":
        fail()
    if version != 1:
        fail("UnsupportedFeature(0x0080)" if version else "Corrupt")
    if kind not in KINDS.values() or flags & ~3 or reserved16 or reserved64:
        fail()
    expected_flags = 1 if kind == 7 else (flags & 2 if kind in GRANT_KINDS else 0)
    if flags != expected_flags:
        fail()
    if 64 + namespace_len + key_len + holder_len != length:
        fail()
    if not rules.broken("field_bounds"):
        sizes = (namespace_len, key_len, holder_len)
        if kind in GRANT_KINDS:
            if not all(
                1 <= size <= maximum
                for size, maximum in zip(sizes, (255, 1024, 255))
            ):
                fail()
        elif any(sizes):
            fail()
    position = 64
    fields = []
    for size in (namespace_len, key_len, holder_len):
        try:
            fields.append(body[position:position + size].decode("utf-8"))
        except UnicodeDecodeError:
            fail()
        position += size
    frame = Frame(
        next(name for name, value in KINDS.items() if value == kind),
        str(uuid.UUID(bytes=domain)), high_water, token, deadline,
        collection, *fields, bool(flags & 2), sequence,
    )
    validate_frame(frame, rules)
    return frame


Address = tuple[int, bytes, bytes]


@dataclass(frozen=True)
class Grant:
    holder: str
    token: int
    deadline: int
    active: bool = True


class Ledger:
    """Abstract durable events are independent of byte encode/decode."""

    def __init__(self, rules: Rules = Rules()):
        self.rules = rules
        self.domain: str | None = None
        self.high_water = 0
        self.rows: dict[Address, Grant] = {}
        self.recovery = False
        self.pending: int | None = None
        self.snapshot_last: Address | None = None
        self.snapshot_phase = False

    def apply(self, frame: Frame) -> None:
        validate_frame(frame, self.rules)
        if frame.kind == "DomainInit":
            if self.domain is not None:
                fail()
            self.domain = frame.domain
            self.high_water = frame.high_water
            self.snapshot_phase = True
            return
        if self.domain is None or frame.domain != self.domain:
            fail()
        if frame.high_water < self.high_water:
            fail()
        kind = frame.kind
        if self.recovery and kind != "RecoveryRequired":
            fail()
        if self.pending is not None and kind not in {
            "Acquire", "StartupBarrier", "RecoveryRequired",
        }:
            fail()
        if kind == "Reserve":
            if frame.token <= self.high_water:
                fail()
            self.high_water = frame.high_water
            self.pending = frame.token
        elif kind in {"StartupBarrier", "RecoveryRequired"}:
            if frame.high_water != self.high_water:
                fail()
            self.rows = {
                address: replace(grant, active=False)
                for address, grant in self.rows.items()
            }
            self.pending = None
            if kind == "RecoveryRequired":
                self.recovery = True
        elif kind in {"Acquire", "Renew", "Revoke", "SnapshotGrant"}:
            address = (
                frame.collection, frame.namespace.encode(), frame.key.encode(),
            )
            previous = self.rows.get(address)
            if frame.high_water != self.high_water:
                fail()
            if address not in self.rows and len(self.rows) >= MAX_ROWS:
                fail()
            if kind == "Acquire":
                if frame.token != self.high_water or (
                    frame.token != self.pending
                    and not self.rules.broken("reservation_required")
                ):
                    fail()
                self.pending = None
            elif kind in {"Renew", "Revoke"}:
                if previous is None or not previous.active or (
                    previous.holder != frame.holder or previous.token != frame.token
                ):
                    fail()
            else:
                if not self.snapshot_phase:
                    fail()
                if not self.rules.broken("snapshot_order") and (
                    self.snapshot_last is not None
                    and address <= self.snapshot_last
                ):
                    fail()
                self.snapshot_last = address
            self.rows[address] = Grant(
                frame.holder, frame.token, frame.deadline, frame.active,
            )
        else:
            fail()
        if kind != "SnapshotGrant":
            self.snapshot_phase = False

    def ordinary_commit(self) -> None:
        if self.pending is not None:
            fail()
        self.snapshot_phase = False


class Replay:
    """Auxiliary frames preserve ordinary sequence, count and history floor."""

    def __init__(
        self, rules: Rules = Rules(), *, sequence: int = 0,
        timestamps: bool = False,
    ):
        self.rules = rules
        self.ledger = Ledger(rules)
        self.sequence = sequence
        self.history_floor = sequence
        self.commits_seen = 0
        self.end = 0
        self.timestamps = timestamps
        self.pending_anchor = False

    def auxiliary(self, packet: bytes) -> None:
        frame = decode_frame(packet, loaded_sequence=self.sequence, rules=self.rules)
        if self.pending_anchor:
            fail()
        self.ledger.apply(frame)
        self.end += len(packet)

    def anchor(self) -> None:
        if self.pending_anchor:
            fail()
        self.pending_anchor = True

    def ordinary(self, sequence: int) -> None:
        if sequence != self.sequence + 1 or (
            self.timestamps and not self.pending_anchor
        ):
            fail()
        self.ledger.ordinary_commit()
        self.sequence = sequence
        self.commits_seen += 1
        self.pending_anchor = False


class Store:
    """Single-owner, clock-injected API model with explicit crash outcomes.

    ``disk`` contains complete abstract frames, including a failed sync's
    possibly durable outcome. A partial write is represented by ``torn``;
    normal reopen cannot silently repair it.
    """

    def __init__(
        self, rules: Rules = Rules(), mode: str = "writable",
        durability: str = "periodic", enabled: bool = True,
    ):
        self.rules = rules
        self.ledger = Ledger(rules)
        self.ledger.apply(Frame("DomainInit"))
        self.disk = [Frame("DomainInit")]
        self.mode = mode
        self.durability = durability
        self.enabled = enabled
        self.poisoned = False
        self.ready = True
        self.torn = False
        self.last_clock = 0
        self.wall = 100
        self.sequence = 0
        self.syncs = 0
        self.attempts = 0
        self.returned: list[int] = []
        self.burned: set[int] = set()
        self.collections = {0: 1, 1: 1, 2: 1}
        self.events: list[str] = []
        self.fault: str | None = None
        self.outcome = "complete"

    def available(self) -> None:
        if self.poisoned:
            fail("Poisoned")
        if not self.enabled or self.mode != "writable" or self.durability == "none":
            fail("LeaseUnavailable")
        if not self.ready and not self.rules.broken("late_barrier"):
            fail("LeaseUnavailable")
        if self.ledger.recovery:
            fail("FencingRecoveryRequired")

    def now(self) -> int:
        if type(self.wall) is not int or not 0 <= self.wall <= MAX_U64:
            fail("InvalidInput")
        self.last_clock = (
            self.wall if self.rules.broken("clock_rollback")
            else max(self.wall, self.last_clock)
        )
        return self.last_clock

    def deadline(self, now: int, ttl: int) -> int:
        lower = 0 if self.rules.broken("ttl_zero") else 1
        upper = MAX_U64 if self.rules.broken("ttl_max") else MAX_TTL
        if type(ttl) is not int or not lower <= ttl <= upper:
            fail("InvalidInput")
        deadline = now + ttl
        if deadline > MAX_U64:
            if self.rules.broken("deadline_wrap"):
                return deadline & MAX_U64
            fail("InvalidInput")
        return deadline

    def address(self, collection: int, namespace: str, key: str) -> Address:
        if collection not in self.collections or self.collections[collection] != 1:
            fail("InvalidInput")
        ns = checked_text(namespace, 255, self.rules)
        encoded_key = checked_text(key, 1024, self.rules)
        if self.rules.broken("hash_identity"):
            # Deliberately weak hash: the fixture supplies colliding full keys.
            encoded_key = str(sum(encoded_key) % 256).encode("ascii")
        return collection, ns, encoded_key

    def frame(self, kind: str, address: Address | None = None, **fields: object) -> Frame:
        base = Frame(
            kind, domain=self.ledger.domain or DOMAIN,
            high_water=self.ledger.high_water, sequence=self.sequence + 1,
        )
        if address is not None:
            base = replace(
                base, collection=address[0],
                namespace=address[1].decode(), key=address[2].decode(),
            )
        return replace(base, **fields)

    def indeterminate(self) -> None:
        self.poisoned = True
        if self.rules.broken("automatic_retry"):
            self.attempts += 1
            self.events.append("automatic-retry")
        fail("CommitIndeterminate")

    def append(self, frame: Frame) -> None:
        kind = frame.kind
        if self.fault == "before_reserve" and kind == "Reserve":
            fail("InjectedBeforeAppend")
        self.events.append(f"append:{kind}")
        if self.fault == f"{kind}:sync":
            if self.outcome == "complete":
                self.disk.append(frame)
            elif self.outcome == "partial":
                self.torn = True
            if self.rules.broken("publish_before_sync") and kind == "Acquire":
                self.ledger.apply(frame)
                self.returned.append(frame.token)
                return
            self.indeterminate()
        self.disk.append(frame)
        if not self.rules.broken("periodic_only"):
            self.syncs += 1
            self.events.append(f"sync:{kind}")
        self.ledger.apply(frame)
        if kind == "Reserve":
            self.burned.add(frame.token)
            if self.fault == "after_reserve":
                self.indeterminate()
        if kind == "Acquire" and self.fault == "after_grant":
            self.indeterminate()

    def acquire(
        self, key: str = "k", *, namespace: str = "n", collection: int = 1,
        holder: str = "alice", ttl: int = 10,
    ) -> Grant:
        self.attempts += 1
        self.available()
        address = self.address(collection, namespace, key)
        checked_text(holder, 255, self.rules)
        now = self.now()
        deadline = self.deadline(now, ttl)
        old = self.ledger.rows.get(address)
        if old is not None and old.active and now < old.deadline:
            fail("LeaseHeld")
        if address not in self.ledger.rows and (
            len(self.ledger.rows) >= MAX_ROWS and not self.rules.broken("cap")
        ):
            fail("QuotaExceeded")
        if self.ledger.high_water == MAX_U64:
            fail("InvalidInput")
        token = self.ledger.high_water + 1
        if not self.rules.broken("reserve_after_grant"):
            self.append(self.frame("Reserve", high_water=token, token=token))
        else:
            # The faulty allocator advances only volatile state, not the log.
            self.ledger.high_water = token
            self.ledger.pending = token
        self.append(self.frame(
            "Acquire", address, token=token, deadline=deadline,
            holder=holder, active=True,
        ))
        self.returned.append(token)
        return self.ledger.rows[address]

    def matching(
        self, key: str, namespace: str, collection: int, holder: str,
        domain: str, token: int,
    ) -> tuple[Address, Grant, int]:
        self.available()
        address = self.address(collection, namespace, key)
        checked_text(holder, 255, self.rules)
        valid_domain(domain, self.rules)
        if type(token) is not int or not 1 <= token <= MAX_U64:
            fail("InvalidInput")
        now = self.now()
        old = self.ledger.rows.get(address)
        if old is None:
            fail("StaleFencingToken")
        if not old.active and not self.rules.broken("inactive_match"):
            fail("StaleFencingToken")
        if now >= old.deadline and not self.rules.broken("expired_match"):
            fail("StaleFencingToken")
        if holder != old.holder and not self.rules.broken("holder_match"):
            fail("StaleFencingToken")
        if domain != self.ledger.domain and not self.rules.broken("domain_match"):
            fail("StaleFencingToken")
        if token != old.token and not self.rules.broken("token_match"):
            fail("StaleFencingToken")
        return address, old, now

    def renew(
        self, token: int, *, key: str = "k", namespace: str = "n",
        collection: int = 1, holder: str = "alice", domain: str = DOMAIN,
        ttl: int = 10,
    ) -> Grant:
        self.attempts += 1
        address, old, now = self.matching(
            key, namespace, collection, holder, domain, token,
        )
        deadline = self.deadline(now, ttl)
        if self.rules.broken("renew_token"):
            # Bypass event validation to model the independently wrong API result.
            return replace(old, token=old.token + 1, deadline=deadline)
        if self.rules.broken("inactive_match") or self.rules.broken("expired_match"):
            return replace(old, deadline=deadline, active=True)
        self.append(self.frame(
            "Renew", address, token=old.token, holder=old.holder,
            deadline=deadline, active=True,
        ))
        return self.ledger.rows[address]

    def revoke(
        self, token: int, *, key: str = "k", namespace: str = "n",
        collection: int = 1, holder: str = "alice", domain: str = DOMAIN,
    ) -> bool:
        self.attempts += 1
        address, old, _ = self.matching(
            key, namespace, collection, holder, domain, token,
        )
        if self.rules.broken("inactive_match") or self.rules.broken("expired_match"):
            return True
        self.append(self.frame(
            "Revoke", address, token=old.token, holder=old.holder,
            deadline=old.deadline, active=False,
        ))
        if self.rules.broken("revoke_high_water"):
            self.ledger.high_water = 0
        return True

    def reopen(self, *, fault: str | None = None, outcome: str = "complete") -> Store:
        if self.torn:
            fail("Corrupt")
        reopened = Store(
            self.rules, self.mode, self.durability, self.enabled,
        )
        reopened.disk = copy.deepcopy(self.disk)
        reopened.ledger = Ledger(self.rules)
        for frame in reopened.disk:
            reopened.ledger.apply(frame)
        reopened.sequence = self.sequence
        reopened.wall = self.wall
        reopened.burned = set(self.burned)
        reopened.ready = False
        reopened.fault = fault
        reopened.outcome = outcome
        if self.rules.broken("restart_high_water"):
            reopened.ledger.high_water = 0
            reopened.disk = [Frame("DomainInit")]
            reopened.ledger.pending = None
        if (
            self.mode == "writable" and self.durability != "none"
            and self.enabled and not reopened.ledger.recovery
        ):
            if not self.rules.broken("late_barrier"):
                try:
                    reopened.append(reopened.frame("StartupBarrier"))
                except OracleError:
                    # The failed open exposes no handle, but its physical
                    # writes still exist on the backing file for the next open.
                    self.disk = reopened.disk
                    self.torn = reopened.torn
                    raise
            reopened.ready = True
        elif reopened.ledger.recovery:
            reopened.ready = True
        self.disk = reopened.disk
        return reopened

    def repair(self) -> Store:
        repaired = copy.deepcopy(self)
        repaired.torn = False
        repaired.poisoned = False
        repaired.ledger = Ledger(self.rules)
        for frame in repaired.disk:
            repaired.ledger.apply(frame)
        repaired.append(repaired.frame("RecoveryRequired"))
        return repaired

    def carry(self, operation: str) -> Store:
        carried = copy.deepcopy(self)
        if self.rules.broken("carry_domain"):
            carried.ledger.domain = OTHER_DOMAIN
        if self.rules.broken("carry_high_water"):
            carried.ledger.high_water = 0
        if self.rules.broken("clear_flag"):
            carried.enabled = False
        return carried

    def managed_copy(self, operation: str) -> Store:
        destination = copy.deepcopy(self)
        if not self.rules.broken("restore_ready"):
            destination.append(destination.frame("RecoveryRequired"))
        return destination

    def ordinary(self, operation: str) -> None:
        if self.rules.broken("ordinary_writer"):
            self.append(self.frame("StartupBarrier"))
        self.ledger.ordinary_commit()
        self.sequence += 1


class Downstream:
    def __init__(self, domain: str):
        self.domain = domain
        self.highest = 0
        self.identity: str | None = None

    def accept(self, domain: str, token: int, identity: str) -> bool:
        if domain != self.domain or token < self.highest:
            return False
        if token == self.highest:
            return identity == self.identity
        self.highest = token
        self.identity = identity
        return True


TESTS: dict[str, Callable[[Rules], None]] = {}


def trace(name: str) -> Callable[[Callable[[Rules], None]], Callable[[Rules], None]]:
    def register(function: Callable[[Rules], None]) -> Callable[[Rules], None]:
        if name in TESTS:
            raise RuntimeError(f"duplicate trace: {name}")
        TESTS[name] = function
        return function
    return register


@trace("acquire_reserves_and_syncs_before_publication")
def trace_acquire(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire()
    require(grant.active and grant.deadline == 110, "grant fields")
    require(grant.token > 0 and store.ledger.high_water >= grant.token, "token floor")
    require(store.events == [
        "append:Reserve", "sync:Reserve", "append:Acquire", "sync:Acquire",
    ], "reservation and grant must each sync, in order")
    require(store.sequence == 0, "auxiliary frames consumed a user sequence")


@trace("live_conflict_discloses_no_holder_and_allocates_nothing")
def trace_conflict(rules: Rules) -> None:
    store = Store(rules)
    store.acquire()
    before = (store.ledger.high_water, len(store.disk), store.syncs)
    for holder in ("alice", "bob"):
        expect_error("LeaseHeld", lambda holder=holder: store.acquire(holder=holder))
    require(before == (store.ledger.high_water, len(store.disk), store.syncs), "conflict wrote")


@trace("renew_keeps_token_and_can_shorten_deadline")
def trace_renew(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire(ttl=100)
    store.wall = 101
    renewed = store.renew(grant.token, ttl=1)
    require(renewed.token == grant.token and renewed.deadline == 102, "renew is now+ttl")
    require(store.ledger.high_water == grant.token, "renew allocated")
    require(store.syncs == 3, "renew not synced")


@trace("another_address_allocation_does_not_invalidate_current_token")
def trace_multiple_grants(rules: Rules) -> None:
    store = Store(rules)
    first = store.acquire()
    second = store.acquire("other")
    require(second.token > first.token, "file-wide token order")
    require(store.renew(first.token).token == first.token, "compared token to global floor")
    require(store.revoke(first.token), "could not revoke older live token")
    require(store.ledger.rows[(1, b"n", b"other")].active, "revoked unrelated address")
    require(store.ledger.high_water == second.token, "renew/revoke altered allocator")


@trace("renew_validates_ttl_and_overflow_before_any_write")
def trace_renew_bounds(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire(ttl=MAX_TTL)
    for ttl in (0, MAX_TTL + 1):
        before = (len(store.disk), store.syncs, store.ledger.high_water)
        expect_error("InvalidInput", lambda ttl=ttl: store.renew(grant.token, ttl=ttl))
        require(before == (len(store.disk), store.syncs, store.ledger.high_water), "invalid renew wrote")
    store.wall = MAX_U64 - 2
    store.ledger.rows[(1, b"n", b"k")] = replace(grant, deadline=MAX_U64)
    expect_error("InvalidInput", lambda: store.renew(grant.token, ttl=3))


@trace("revoke_preserves_floor_and_successor_is_strictly_greater")
def trace_revoke(rules: Rules) -> None:
    store = Store(rules)
    first = store.acquire()
    require(store.revoke(first.token) is True, "revoke result")
    require(store.ledger.high_water >= first.token, "revoke discarded floor")
    next_grant = store.acquire()
    require(next_grant.token > first.token, "revoke allowed token reuse")
    require(store.syncs == 5, "revoke did not sync")


def stale_case(rules: Rules, field: str, operation: str) -> None:
    store = Store(rules)
    grant = store.acquire()
    kwargs = {
        "holder": {"holder": "bob"},
        "domain": {"domain": OTHER_DOMAIN},
        "token": {},
        "missing": {"key": "missing"},
    }[field]
    token = grant.token + 1 if field == "token" else grant.token
    expect_error("StaleFencingToken", lambda: getattr(store, operation)(token, **kwargs))


for _field in ("holder", "domain", "token", "missing"):
    for _operation in ("renew", "revoke"):
        def _stale(rules: Rules, field: str = _field, operation: str = _operation) -> None:
            stale_case(rules, field, operation)
        trace(f"{_operation}_rejects_foreign_{_field}")(_stale)


for _operation in ("renew", "revoke"):
    def _wrong_address(rules: Rules, operation: str = _operation) -> None:
        store = Store(rules)
        grant = store.acquire()
        for kwargs in ({"namespace": "other"}, {"collection": 2}):
            expect_error(
                "StaleFencingToken",
                lambda kwargs=kwargs: getattr(store, operation)(grant.token, **kwargs),
            )
    trace(f"{_operation}_rejects_other_namespace_or_physical_collection")(_wrong_address)


for _operation in ("renew", "revoke"):
    for _delta in (0, 1):
        def _expired(rules: Rules, operation: str = _operation, delta: int = _delta) -> None:
            store = Store(rules)
            grant = store.acquire()
            store.wall = grant.deadline + delta
            expect_error("StaleFencingToken", lambda: getattr(store, operation)(grant.token))
        trace(f"{_operation}_rejects_deadline_plus_{_delta}")(_expired)

    def _revoked(rules: Rules, operation: str = _operation) -> None:
        store = Store(rules)
        grant = store.acquire()
        store.revoke(grant.token)
        expect_error("StaleFencingToken", lambda: getattr(store, operation)(grant.token))
    trace(f"{_operation}_rejects_revoked")(_revoked)


@trace("deadline_minus_one_is_live_then_equal_allows_successor")
def trace_expiry(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire()
    store.wall = grant.deadline - 1
    expect_error("LeaseHeld", store.acquire)
    store.wall += 1
    successor = store.acquire()
    require(successor.token > grant.token, "expiry reused token")
    expect_error("StaleFencingToken", lambda: store.revoke(grant.token))


@trace("clock_rollback_cannot_resurrect_observed_expiry")
def trace_clock(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire()
    store.wall = grant.deadline
    expect_error("StaleFencingToken", lambda: store.renew(grant.token))
    store.wall = 99
    expect_error("StaleFencingToken", lambda: store.renew(grant.token))


@trace("invalid_clock_read_does_not_update_clamp")
def trace_invalid_clock(rules: Rules) -> None:
    store = Store(rules)
    store.acquire()
    before = store.last_clock
    store.wall = -1
    expect_rejected(lambda: store.acquire("other"))
    require(store.last_clock == before, "invalid clock changed clamp")


@trace("ttl_zero_is_invalid_without_writes")
def trace_ttl_zero(rules: Rules) -> None:
    store = Store(rules)
    expect_error("InvalidInput", lambda: store.acquire(ttl=0))
    require(len(store.disk) == 1, "invalid ttl wrote")


@trace("ttl_maximum_and_over_maximum")
def trace_ttl_max(rules: Rules) -> None:
    store = Store(rules)
    require(store.acquire(ttl=MAX_TTL).deadline == 100 + MAX_TTL, "max ttl")
    expect_error("InvalidInput", lambda: store.acquire("other", ttl=MAX_TTL + 1))


@trace("checked_deadline_overflow_and_exact_u64_edge")
def trace_overflow(rules: Rules) -> None:
    store = Store(rules)
    store.wall = MAX_U64 - 1
    expect_error("InvalidInput", lambda: store.acquire(ttl=2))
    require(store.acquire(ttl=1).deadline == MAX_U64, "exact deadline edge")


@trace("token_exhaustion_refuses_without_wrap")
def trace_exhaustion(rules: Rules) -> None:
    store = Store(rules)
    store.ledger.high_water = MAX_U64
    expect_error("InvalidInput", store.acquire)
    require(len(store.disk) == 1, "exhaustion wrote")


@trace("utf8_byte_bounds_and_no_normalization")
def trace_text(rules: Rules) -> None:
    store = Store(rules)
    store.acquire("a" * 1024, namespace="n" * 255, holder="h" * 255)
    for field, value in (
        ("key", ""), ("key", "a" * 1025), ("namespace", ""),
        ("namespace", "\u00e9" * 128), ("holder", ""), ("holder", "\u00e9" * 128),
        ("key", "\ud800"),
    ):
        kwargs = {field: value}
        expect_error("InvalidInput", lambda kwargs=kwargs: store.acquire(**kwargs))
    composed = store.acquire("\u00e9")
    decomposed = store.acquire("e\u0301")
    require(decomposed.token > composed.token, "normalized distinct keys")


@trace("physical_collection_namespace_and_collision_addresses_are_distinct")
def trace_identity(rules: Rules) -> None:
    store = Store(rules)
    # Equal intentionally supplied hash; full bytes, not a Rust hash, are identity.
    require(sum(b"ab") == sum(b"ba"), "fixture must collide")
    grants = [
        store.acquire("ab"), store.acquire("ba"),
        store.acquire("ab", namespace="other"),
        store.acquire("ab", collection=2),
    ]
    require(len(store.ledger.rows) == 4, "physical addresses collided")
    require(all(a.token < b.token for a, b in zip(grants, grants[1:])), "global allocator")


@trace("restart_barrier_invalidates_grants_preserves_domain_and_floor")
def trace_restart(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire()
    reopened = store.reopen()
    require(reopened.events == [
        "append:StartupBarrier", "sync:StartupBarrier",
    ], "open did not sync barrier before exposing owner")
    require(reopened.ledger.domain == DOMAIN, "restart reset domain")
    expect_error("StaleFencingToken", lambda: reopened.renew(grant.token))
    next_grant = reopened.acquire()
    require(next_grant.token > grant.token, "restart reused token")
    require(len(reopened.ledger.rows) == 1, "restart discarded evidence")


@trace("distinct_address_cap_includes_inactive_and_existing_renew_revoke_work")
def trace_cap(rules: Rules) -> None:
    store = Store(rules)
    store.ledger.rows = {
        (1, b"n", f"k{index}".encode()): Grant("alice", index + 1, 110, False)
        for index in range(MAX_ROWS)
    }
    store.ledger.high_water = MAX_ROWS
    before = (len(store.disk), store.syncs, store.ledger.high_water)
    expect_error("QuotaExceeded", lambda: store.acquire("new"))
    require(before == (len(store.disk), store.syncs, store.ledger.high_water), "cap refusal allocated or wrote")
    grant = store.acquire("k0")
    require(store.renew(grant.token, key="k0").token == grant.token, "renew at cap")
    require(store.revoke(grant.token, key="k0"), "revoke at cap")
    require(len(store.ledger.rows) == MAX_ROWS, "cap evicted evidence")


for _mode in ("readonly", "replica", "shared", "memory"):
    def _unavailable(rules: Rules, mode: str = _mode) -> None:
        store = Store(rules, mode=mode)
        expect_error("LeaseUnavailable", store.acquire)
        expect_error("LeaseUnavailable", lambda: store.renew(1))
        expect_error("LeaseUnavailable", lambda: store.revoke(1))
        reopened = store.reopen()
        require(len(reopened.disk) == 1, "non-owner wrote startup barrier")
    trace(f"{_mode}_has_no_lease_owner_or_startup_write")(_unavailable)


@trace("durability_none_refuses_lease")
def trace_none(rules: Rules) -> None:
    store = Store(rules, durability="none")
    expect_error("LeaseUnavailable", store.acquire)


@trace("absent_capability_is_not_lazy_enablement")
def trace_disabled(rules: Rules) -> None:
    store = Store(rules, enabled=False)
    expect_error("LeaseUnavailable", store.acquire)
    require(len(store.disk) == 1, "lazy enablement")


@trace("collection_must_exist_and_be_one_dimensional")
def trace_collection(rules: Rules) -> None:
    store = Store(rules)
    store.collections[2] = 3
    expect_rejected(lambda: store.acquire(collection=99))
    expect_rejected(lambda: store.acquire(collection=2))
    require(99 not in store.collections, "lease implicitly created collection")


@trace("drop_recreate_preserves_old_physical_evidence_and_allocator")
def trace_drop(rules: Rules) -> None:
    store = Store(rules)
    grant = store.acquire()
    store.ordinary("drop")
    del store.collections[1]
    store.collections[3] = 1
    successor = store.acquire(collection=3)
    require(successor.token > grant.token and store.ledger.domain == DOMAIN, "drop reset allocator")
    require((1, b"n", b"k") in store.ledger.rows, "drop removed ledger evidence")


for _carry in ("compaction", "upgrade", "reencrypt", "directory_conversion", "checkpoint"):
    def _carry_trace(rules: Rules, operation: str = _carry) -> None:
        store = Store(rules)
        grant = store.acquire()
        store.revoke(grant.token)
        store.ordinary("put")
        carried = store.carry(operation)
        require(carried.enabled, "rewrite stripped capability")
        require(carried.ledger.domain == store.ledger.domain, "rewrite reset domain")
        require(carried.ledger.high_water == store.ledger.high_water, "rewrite reset floor")
        require(carried.ledger.rows == store.ledger.rows, "rewrite lost inactive grant")
        require(carried.sequence == store.sequence, "checkpoint advanced sequence/history")
    trace(f"{_carry}_carries_domain_floor_rows_flag_and_sequence")(_carry_trace)


for _managed in (
    "backup", "restore", "pitr", "directory_restore", "rollback",
    "copy", "promotion",
):
    def _managed_trace(rules: Rules, operation: str = _managed) -> None:
        store = Store(rules)
        grant = store.acquire()
        destination = store.managed_copy(operation)
        require(not store.ledger.recovery, "stamping changed source")
        require(destination.ledger.domain == DOMAIN, "restore changed domain")
        require(destination.ledger.high_water >= grant.token, "restore lost floor")
        require(destination.syncs == store.syncs + 1, "recovery stamp not synced")
        require(not any(row.active for row in destination.ledger.rows.values()), "recovery left active row")
        for action in (
            destination.acquire, lambda: destination.renew(grant.token),
            lambda: destination.revoke(grant.token),
        ):
            expect_error("FencingRecoveryRequired", action)
        reopened = destination.reopen()
        expect_error("FencingRecoveryRequired", reopened.acquire)
        sequence = reopened.sequence
        reopened.ordinary("kv_put")
        require(reopened.sequence == sequence + 1, "recovery blocked ordinary database")
    trace(f"{_managed}_stamps_terminal_recovery_before_publication")(_managed_trace)


@trace("ordinary_record_kv_import_drop_never_write_ledger")
def trace_nonowners(rules: Rules) -> None:
    store = Store(rules)
    store.acquire()
    before = copy.deepcopy((store.disk, store.ledger.rows, store.ledger.high_water))
    for operation in ("record_put", "record_delete", "kv_put", "kv_delete", "import", "drop"):
        store.ordinary(operation)
        require(before == (store.disk, store.ledger.rows, store.ledger.high_water), f"{operation} touched ledger")


@trace("downstream_rejects_old_domain_predecessor_and_equal_foreign_grant")
def trace_downstream(rules: Rules) -> None:
    store = Store(rules)
    resource = Downstream(DOMAIN)
    first = store.acquire()
    require(resource.accept(DOMAIN, first.token, "alice/k"), "first fence")
    require(resource.accept(DOMAIN, first.token, "alice/k"), "same grant equal fence")
    require(not resource.accept(DOMAIN, first.token, "bob/k"), "foreign equal fence")
    store.wall = first.deadline
    second = store.acquire(holder="bob")
    # Expiry alone cannot undo a downstream write or reject an unobserved successor.
    require(resource.accept(DOMAIN, first.token, "alice/k"), "invented immediate revocation")
    require(resource.accept(DOMAIN, second.token, "bob/k"), "successor fence")
    require(not resource.accept(DOMAIN, first.token, "alice/k"), "predecessor fence")
    require(not resource.accept(OTHER_DOMAIN, MAX_U64, "bob/k"), "cross-domain comparison")


@trace("before_reservation_append_has_no_allocation")
def trace_before_reserve(rules: Rules) -> None:
    store = Store(rules)
    store.fault = "before_reserve"
    expect_error("InjectedBeforeAppend", store.acquire)
    require(store.ledger.high_water == 0 and len(store.disk) == 1, "allocated before append")
    store.fault = None
    require(store.acquire().token > 0, "safe retry did not acquire")


@trace("synced_reservation_burns_token_before_grant")
def trace_burn(rules: Rules) -> None:
    store = Store(rules)
    store.fault = "after_reserve"
    expect_error("CommitIndeterminate", store.acquire)
    require(store.returned == [], "grant published before grant sync")
    require(store.ledger.high_water > 0 and store.ledger.pending is not None, "reservation not durable")
    burned = store.ledger.high_water
    reopened = store.reopen()
    require(reopened.acquire().token > burned, "synced reservation reused")


for _kind in ("Reserve", "Acquire", "Renew", "Revoke"):
    for _outcome in ("absent", "complete", "partial"):
        def _crash(
            rules: Rules, kind: str = _kind, outcome: str = _outcome,
        ) -> None:
            store = Store(rules)
            if kind in {"Renew", "Revoke"}:
                first = store.acquire()
            else:
                first = None
            store.fault = f"{kind}:sync"
            store.outcome = outcome
            action = (
                store.acquire if first is None
                else lambda: getattr(store, kind.lower())(first.token)
            )
            before_returned = list(store.returned)
            expect_error("CommitIndeterminate", action)
            require(store.returned == before_returned, "success before sync")
            require(store.poisoned, "indeterminate handle not poisoned")
            expect_error("Poisoned", lambda: store.acquire("other"))
            if outcome == "partial":
                expect_error("Corrupt", store.reopen)
                repaired = store.repair()
                expect_error("FencingRecoveryRequired", repaired.acquire)
            else:
                floor = max(
                    (frame.high_water for frame in store.disk), default=0,
                )
                reopened = store.reopen()
                require(not any(g.active for g in reopened.ledger.rows.values()), "reopen preserved authority")
                require(reopened.acquire().token > floor, "crash reused replayed floor")
        trace(f"crash_{_kind.lower()}_sync_{_outcome}")(_crash)


@trace("ambiguous_after_grant_sync_never_automatically_retries")
def trace_ambiguous(rules: Rules) -> None:
    store = Store(rules)
    store.fault = "after_grant"
    expect_error("CommitIndeterminate", store.acquire)
    require(store.attempts == 1 and "automatic-retry" not in store.events, "automatic retry")
    require(store.returned == [], "ambiguous result became success")
    burned = store.ledger.high_water
    require(store.ledger.rows[(1, b"n", b"k")].active, "synced grant not modeled")
    expect_error("Poisoned", store.acquire)
    reopened = store.reopen()
    require(reopened.acquire().token > burned, "ambiguous token reused")


@trace("lost_response_can_leave_live_grant_until_expiry_without_retry")
def trace_lost_response(rules: Rules) -> None:
    store = Store(rules)

    def request_with_lost_response() -> None:
        store.acquire()
        fail("ResponseLost")

    expect_error("ResponseLost", request_with_lost_response)
    require(store.attempts == 1, "transport retried acquisition")
    floor = store.ledger.high_water
    require(not store.poisoned, "a disconnect alone poisoned storage")
    expect_error("LeaseHeld", store.acquire)
    store.wall = 110
    successor = store.acquire()
    require(successor.token > floor, "inferred/reused ambiguous token")


for _operation in ("renew", "revoke"):
    def _ambiguous_mutation(rules: Rules, operation: str = _operation) -> None:
        store = Store(rules)
        grant = store.acquire()
        store.fault = f"{operation.title()}:sync"
        store.outcome = "complete"
        before_attempts = store.attempts
        expect_error(
            "CommitIndeterminate", lambda: getattr(store, operation)(grant.token),
        )
        require(store.attempts == before_attempts + 1, f"automatically retried {operation}")
        require("automatic-retry" not in store.events, "implicit retry")
        expect_error("Poisoned", lambda: getattr(store, operation)(grant.token))
    trace(f"ambiguous_{_operation}_is_not_retried")(_ambiguous_mutation)


for _outcome in ("absent", "complete", "partial"):
    def _barrier(rules: Rules, outcome: str = _outcome) -> None:
        store = Store(rules)
        grant = store.acquire()
        expect_error(
            "CommitIndeterminate",
            lambda: store.reopen(fault="StartupBarrier:sync", outcome=outcome),
        )
        if outcome == "partial":
            expect_error("Corrupt", store.reopen)
            expect_error("FencingRecoveryRequired", store.repair().acquire)
        else:
            reopened = store.reopen()
            expect_error("StaleFencingToken", lambda: reopened.renew(grant.token))
            require(reopened.acquire().token > grant.token, "barrier fault reused token")
    trace(f"startup_barrier_{_outcome}_fault_returns_no_handle")(_barrier)


@trace("unexposed_handle_cannot_serve_before_barrier")
def trace_unexposed(rules: Rules) -> None:
    store = Store(rules)
    store.ready = False
    expect_error("LeaseUnavailable", store.acquire)


@trace("checkpoint_publication_old_or_new_complete_image_preserves_floor")
def trace_checkpoint_crash(rules: Rules) -> None:
    store = Store(rules)
    store.acquire()
    old = store.carry("old_checkpoint")
    new = store.carry("new_checkpoint")
    for candidate in (old, new):
        require(candidate.ledger.domain == DOMAIN, "checkpoint domain")
        require(candidate.ledger.high_water == store.ledger.high_water, "checkpoint floor")
        require(candidate.ledger.rows == store.ledger.rows, "checkpoint rows")


@trace("restore_publication_sync_failure_exposes_no_destination")
def trace_restore_crash(rules: Rules) -> None:
    for operation in ("restore", "promotion", "backup"):
        store = Store(rules)
        store.acquire()
        store.fault = "RecoveryRequired:sync"
        expect_error("CommitIndeterminate", lambda: store.managed_copy(operation))


@trace("reservation_requires_immediately_matching_acquire")
def trace_replay_reservation(rules: Rules) -> None:
    ledger = Ledger(rules)
    ledger.apply(Frame("DomainInit"))
    without_reserve = Ledger(rules)
    without_reserve.apply(Frame("DomainInit", high_water=9))
    expect_error("Corrupt", lambda: without_reserve.apply(Frame(
        "Acquire", high_water=9, token=9, deadline=110,
        collection=0, namespace="n", key="k", holder="alice", active=True,
    )))
    expect_error("Corrupt", lambda: ledger.apply(Frame(
        "Acquire", high_water=0, token=1, deadline=110,
        collection=1, namespace="n", key="k", holder="alice", active=True,
    )))
    ledger.apply(Frame("Reserve", high_water=9, token=9))
    expect_error("Corrupt", ledger.ordinary_commit)
    expect_error("Corrupt", lambda: ledger.apply(Frame("Reserve", high_water=10, token=10)))
    wrong = Frame(
        "Acquire", high_water=9, token=8, deadline=110,
        collection=1, namespace="n", key="k", holder="alice", active=True,
    )
    expect_error("Corrupt", lambda: ledger.apply(wrong))
    ledger.apply(replace(wrong, token=9))
    ledger.ordinary_commit()
    require(ledger.rows[(1, b"n", b"k")].token == 9, "matching grant missing")


@trace("mixed_lifecycle_never_reuses_returned_or_burned_tokens")
def trace_mixed(rules: Rules) -> None:
    store = Store(rules)
    issued = []
    first = store.acquire()
    issued.append(first.token)
    renewed = store.renew(first.token)
    require(renewed.token == first.token, "renew allocated token")
    store.revoke(first.token)
    second = store.acquire()
    issued.append(second.token)
    store.wall = second.deadline
    third = store.acquire()
    issued.append(third.token)
    store = store.reopen()
    fourth = store.acquire()
    issued.append(fourth.token)
    store.fault = "after_reserve"
    expect_error("CommitIndeterminate", lambda: store.acquire("other"))
    burned = store.ledger.high_water
    require(burned > issued[-1], "reservation did not advance beyond returned tokens")
    store = store.reopen()
    fifth = store.acquire()
    issued.append(fifth.token)
    require(fifth.token > burned, "burned token reused")
    require(all(a < b for a, b in zip(issued, issued[1:])), "tokens not strictly increasing")
    require(len(set(issued)) == len(issued), "returned token reused")


@trace("snapshot_rows_sorted_unique_and_high_water_unchanged")
def trace_snapshots(rules: Rules) -> None:
    ledger = Ledger(rules)
    ledger.apply(Frame("DomainInit", high_water=9))
    row = Frame(
        "SnapshotGrant", high_water=9, token=2, deadline=110,
        collection=1, namespace="n", key="a", holder="alice", active=False,
    )
    ledger.apply(row)
    expect_error("Corrupt", lambda: ledger.apply(row))
    ledger = Ledger(rules)
    ledger.apply(Frame("DomainInit", high_water=9))
    ledger.apply(replace(row, key="z"))
    expect_error("Corrupt", lambda: ledger.apply(row))
    ledger = Ledger(rules)
    ledger.apply(Frame("DomainInit", high_water=9))
    ledger.apply(row)
    ledger.apply(replace(row, key="z", token=3, active=True))
    require(ledger.high_water == 9 and len(ledger.rows) == 2, "snapshot altered allocator")


@trace("replay_domain_floor_terminal_and_noninitial_domaininit_refuse")
def trace_replay_state(rules: Rules) -> None:
    ledger = Ledger(rules)
    expect_error("Corrupt", lambda: ledger.apply(Frame("StartupBarrier")))
    ledger.apply(Frame("DomainInit", high_water=9))
    expect_error("Corrupt", lambda: ledger.apply(Frame("DomainInit")))
    expect_error("Corrupt", lambda: ledger.apply(Frame("StartupBarrier", high_water=8)))
    expect_error("Corrupt", lambda: ledger.apply(Frame(
        "StartupBarrier", domain=OTHER_DOMAIN, high_water=9,
    )))
    ledger.apply(Frame("RecoveryRequired", high_water=9))
    expect_error("Corrupt", lambda: ledger.apply(Frame("StartupBarrier", high_water=9)))


@trace("domain_input_is_canonical_lowercase_uuid4")
def trace_domain_input(rules: Rules) -> None:
    store = Store(rules)
    first = store.acquire()
    for domain in (DOMAIN.upper(), DOMAIN.replace("-", ""), "not-a-uuid", str(uuid.UUID(int=0))):
        expect_error("InvalidInput", lambda domain=domain: store.renew(first.token, domain=domain))
    for token in (0, -1, MAX_U64 + 1):
        expect_error("InvalidInput", lambda token=token: store.revoke(token))


def patched_packet(packet: bytes, patch: dict[str, object]) -> bytes:
    """Apply a declarative malformed fixture, not a codec-derived expectation."""
    data = bytearray(packet)
    offset = int(patch["offset"])
    replacement = bytes.fromhex(str(patch["hex"]))
    data[offset:offset + len(replacement)] = replacement
    if patch.get("refresh_prefix", True):
        data[32:36] = struct.pack("<I", zlib.crc32(data[:32]))
    if patch.get("refresh_footer", True):
        data[-12:-8] = struct.pack("<I", zlib.crc32(data[:-16]))
    return bytes(data)


def load_goldens() -> list[dict[str, object]]:
    return json.loads((FIXTURES / "goldens.json").read_text(encoding="utf-8"))


def load_malformed() -> list[dict[str, object]]:
    return json.loads((FIXTURES / "malformed.json").read_text(encoding="utf-8"))


@trace("frozen_snapshot_and_reservation_replay_vectors")
def trace_replay_vectors(rules: Rules) -> None:
    fixtures = {row["name"]: row for row in load_goldens()}
    vectors = json.loads((FIXTURES / "replay.json").read_text(encoding="utf-8"))
    for vector in vectors:
        replay = Replay(rules)

        def replay_all() -> None:
            for name in vector["frames"]:
                replay.auxiliary(bytes.fromhex(fixtures[name]["expected_hex"]))
            if vector.get("ordinary"):
                replay.ordinary(1)

        if "expected_error" in vector:
            expect_error(vector["expected_error"], replay_all)
        else:
            replay_all()
            require(replay.ledger.high_water == vector["high_water"], vector["name"])
            require(len(replay.ledger.rows) == vector["rows"], vector["name"])
            require(replay.ledger.recovery == vector.get("recovery", False), vector["name"])
            require(replay.sequence == 0 and replay.commits_seen == 0, "auxiliary consumed commit")


@trace("timestamped_auxiliary_exempt_from_anchor_and_preserves_floor")
def trace_auxiliary_sequence(rules: Rules) -> None:
    fixtures = {row["name"]: row for row in load_goldens()}
    replay = Replay(rules, timestamps=True)
    for name in ("domain_init", "reserve", "acquire", "renew", "revoke"):
        replay.auxiliary(bytes.fromhex(fixtures[name]["expected_hex"]))
        require(replay.sequence == 0 and replay.commits_seen == 0, "auxiliary consumed sequence")
        require(replay.history_floor == 0, "auxiliary changed history floor")
    expect_error("Corrupt", lambda: replay.ordinary(1))
    replay.anchor()
    expect_error("Corrupt", lambda: replay.auxiliary(
        bytes.fromhex(fixtures["startup_barrier"]["expected_hex"]),
    ))
    replay.ordinary(1)
    require(replay.sequence == 1 and replay.commits_seen == 1, "ordinary commit did not consume sequence")


@trace("post_checkpoint_snapshot_uses_floor_plus_one_without_advancing_floor")
def trace_nonzero_floor(rules: Rules) -> None:
    replay = Replay(rules, sequence=41)
    init = Frame("DomainInit", high_water=9, sequence=42)
    row = Frame(
        "SnapshotGrant", high_water=9, token=3, deadline=110, sequence=42,
        namespace="n", key="k", holder="alice", active=False,
    )
    for frame in (init, row):
        replay.auxiliary(encode_frame(frame))
    require(replay.sequence == 41 and replay.history_floor == 41, "snapshot advanced floor")
    require(replay.commits_seen == 0, "snapshot became ordinary commit")
    require(replay.ledger.rows[(0, b"n", b"k")].token == 3, "snapshot grant")
    replay.ordinary(42)
    require(replay.sequence == 42 and replay.history_floor == 41, "next ordinary sequence")


@trace("frame_integrity_is_verified_before_unknown_schema_dispatch")
def trace_authentication_order(rules: Rules) -> None:
    fixture = next(row for row in load_goldens() if row["name"] == "acquire")
    packet = bytes.fromhex(fixture["expected_hex"])
    packet = patched_packet(packet, {"offset": 40, "hex": "0200"})
    expect_error(
        "UnsupportedFeature(0x0080)",
        lambda: decode_frame(packet, rules=rules),
    )
    packet = patched_packet(packet, {
        "offset": 111, "hex": "00000000", "refresh_footer": False,
    })
    expect_error("Corrupt", lambda: decode_frame(packet, rules=rules))


@trace("golden_bytes_all_kinds_match_and_decode")
def trace_goldens(rules: Rules) -> None:
    for fixture in load_goldens():
        frame = Frame(kind=str(fixture["kind"]), **fixture["input"])
        expected = bytes.fromhex(str(fixture["expected_hex"]))
        require(encode_frame(frame, rules) == expected, f"{fixture['name']}: encoded bytes")
        decoded = decode_frame(
            expected, loaded_sequence=frame.sequence - 1, rules=rules,
        )
        require(decoded == frame, f"{fixture['name']}: decoded fields")
    require(zlib.crc32(b"123456789") == 0xCBF43926, "IEEE CRC check vector")


@trace("malformed_prefix_body_integrity_and_capability_vectors")
def trace_malformed(rules: Rules) -> None:
    bases = {fixture["name"]: fixture for fixture in load_goldens()}
    for fixture in load_malformed():
        data = bytes.fromhex(str(bases[fixture["base"]]["expected_hex"]))
        for patch in fixture.get("patches", []):
            data = patched_packet(data, patch)
        kwargs = fixture.get("decode", {})
        expect_error(
            str(fixture["expected_error"]),
            lambda data=data, kwargs=kwargs: decode_frame(data, rules=rules, **kwargs),
        )


@trace("every_prefix_body_footer_byte_truncation_is_not_a_commit")
def trace_truncation(rules: Rules) -> None:
    for fixture in load_goldens():
        packet = bytes.fromhex(str(fixture["expected_hex"]))
        for cut in range(len(packet)):
            expect_error(
                "Truncated",
                lambda cut=cut, packet=packet: decode_frame(
                    packet[:cut], loaded_sequence=int(fixture["input"].get("sequence", 1)) - 1,
                    rules=rules,
                ),
            )
        # No frame can be applied until the complete footer has verified.
        ledger = Ledger(rules)
        expect_error("Truncated", lambda: ledger.apply(decode_frame(packet[:-1], rules=rules)))
        require(ledger.domain is None, "partial frame applied")


@trace("body_bound_edges_2047_2048_2049_and_max_valid_1598")
def trace_body_edges(rules: Rules) -> None:
    largest = Frame(
        "SnapshotGrant", high_water=1, token=1, deadline=MAX_U64,
        namespace="n" * 255, key="k" * 1024, holder="h" * 255,
    )
    packet = encode_frame(largest)
    require(len(packet) - 52 == 1598, "maximum legal body must be 1598")
    require(decode_frame(packet, rules=rules) == largest, "maximum legal body")
    for length in (2047, 2048, 2049):
        # Structurally oversized fields make 2047/2048 invalid even though the
        # prefix bound permits them. No valid v1 body can fill that allowance.
        body = bytearray(packet[36:-16])
        body.extend(b"x" * (length - len(body)))
        body[50:52] = struct.pack("<H", 1024 + length - 1598)
        prefix = bytearray(packet[:36])
        prefix[28:32] = struct.pack("<I", length)
        prefix[32:36] = struct.pack("<I", zlib.crc32(prefix[:32]))
        data = bytes(prefix) + bytes(body)
        data += struct.pack("<4sIQ", b"VCMT", zlib.crc32(data), 1)
        expect_error("Corrupt", lambda data=data: decode_frame(data, rules=rules))
    # Prefix rejection is before body allocation/read; distinguish it from
    # merely rejecting the illegal individual field lengths above.
    prefix = bytearray(packet[:36])
    prefix[28:32] = struct.pack("<I", 2049)
    prefix[32:36] = struct.pack("<I", zlib.crc32(prefix[:32]))
    expect_error("Corrupt", lambda: decode_frame(bytes(prefix), rules=rules))


@trace("frozen_individual_byte_bounds_and_body_edges")
def trace_frozen_bounds(rules: Rules) -> None:
    vectors = json.loads((FIXTURES / "bounds.json").read_text(encoding="utf-8"))
    for vector in vectors:
        fields = vector["input"]
        namespace = b"n" * fields["namespace_bytes"]
        key = b"k" * fields["key_bytes"]
        holder = b"h" * fields["holder_bytes"]
        # This raw fixture builder intentionally does not call encode_frame:
        # the lengths can violate the encoder's preconditions.
        body = (
            bytes.fromhex("564c4c310100080200112233445546778899aabbccddeeff")
            + bytes.fromhex("090000000000000003000000000000006e00000000000000")
            + struct.pack("<HHHHQ", len(namespace), len(key), len(holder), 0, 0)
            + namespace + key + holder
        )
        require(len(body) == fields["body_bytes"], vector["name"])
        prefix = (
            bytes.fromhex("56524543120000000100000000000000000000000000000000000000")
            + struct.pack("<I", len(body))
        )
        packet = prefix + struct.pack("<I", zlib.crc32(prefix)) + body
        packet += struct.pack("<4sIQ", b"VCMT", zlib.crc32(packet), 1)
        if "expected_error" in vector:
            expect_error(
                vector["expected_error"],
                lambda packet=packet: decode_frame(packet, rules=rules),
            )
        else:
            frame = decode_frame(packet, rules=rules)
            require(len(frame.namespace.encode()) == len(namespace), "namespace bound")
            require(len(frame.key.encode()) == len(key), "key bound")
            require(len(frame.holder.encode()) == len(holder), "holder bound")
            require(frame.token == 3 and frame.high_water == 9, "bound fixture fields")


@trace("older_reader_refuses_capability_before_replay_or_repair")
def trace_older_reader(rules: Rules) -> None:
    expect_error(
        "UnsupportedFeature(0x0080)",
        lambda: decode_frame(b"", supported_flags=0x007F, rules=rules),
    )


@trace("ledger_capability_rejected_on_non_v5_without_fixing_error_taxonomy")
def trace_non_v5(rules: Rules) -> None:
    packet = bytes.fromhex(load_goldens()[0]["expected_hex"])
    for version in (1, 2, 3, 4, 6):
        expect_rejected(lambda version=version: decode_frame(
            packet, file_version=version, rules=rules,
        ))


@trace("change_feed_remains_fail_closed_on_op18")
def trace_change_feed(rules: Rules) -> None:
    def feed() -> None:
        if rules.broken("feed_skip"):
            return
        fail()
    expect_error("Corrupt", feed)


MUTANTS = {
    "clock_rollback": "clock_rollback_cannot_resurrect_observed_expiry",
    "ttl_zero": "ttl_zero_is_invalid_without_writes",
    "ttl_max": "ttl_maximum_and_over_maximum",
    "deadline_wrap": "checked_deadline_overflow_and_exact_u64_edge",
    "reserve_after_grant": "synced_reservation_burns_token_before_grant",
    "restart_high_water": "restart_barrier_invalidates_grants_preserves_domain_and_floor",
    "revoke_high_water": "revoke_preserves_floor_and_successor_is_strictly_greater",
    "publish_before_sync": "crash_acquire_sync_complete",
    "automatic_retry": "ambiguous_after_grant_sync_never_automatically_retries",
    "renew_token": "renew_keeps_token_and_can_shorten_deadline",
    "holder_match": "renew_rejects_foreign_holder",
    "domain_match": "renew_rejects_foreign_domain",
    "token_match": "renew_rejects_foreign_token",
    "expired_match": "renew_rejects_deadline_plus_0",
    "inactive_match": "renew_rejects_revoked",
    "hash_identity": "physical_collection_namespace_and_collision_addresses_are_distinct",
    "snapshot_order": "snapshot_rows_sorted_unique_and_high_water_unchanged",
    "clear_flag": "compaction_carries_domain_floor_rows_flag_and_sequence",
    "carry_domain": "upgrade_carries_domain_floor_rows_flag_and_sequence",
    "carry_high_water": "checkpoint_carries_domain_floor_rows_flag_and_sequence",
    "restore_ready": "restore_stamps_terminal_recovery_before_publication",
    "late_barrier": "unexposed_handle_cannot_serve_before_barrier",
    "periodic_only": "acquire_reserves_and_syncs_before_publication",
    "ordinary_writer": "ordinary_record_kv_import_drop_never_write_ledger",
    "character_count": "utf8_byte_bounds_and_no_normalization",
    "domain_format": "domain_input_is_canonical_lowercase_uuid4",
    "missing_flag": "malformed_prefix_body_integrity_and_capability_vectors",
    "prefix_crc": "malformed_prefix_body_integrity_and_capability_vectors",
    "footer_crc": "malformed_prefix_body_integrity_and_capability_vectors",
    "sequence": "malformed_prefix_body_integrity_and_capability_vectors",
    "body_bound": "body_bound_edges_2047_2048_2049_and_max_valid_1598",
    "field_bounds": "body_bound_edges_2047_2048_2049_and_max_valid_1598",
    "zero_token": "malformed_prefix_body_integrity_and_capability_vectors",
    "zero_deadline": "malformed_prefix_body_integrity_and_capability_vectors",
    "older_reader": "older_reader_refuses_capability_before_replay_or_repair",
    "feed_skip": "change_feed_remains_fail_closed_on_op18",
    "cap": "distinct_address_cap_includes_inactive_and_existing_renew_revoke_work",
    "reservation_required": "reservation_requires_immediately_matching_acquire",
}


def self_test() -> dict[str, object]:
    require(len(TESTS) >= 40, "at least 40 named traces are required")
    require(len(MUTANTS) >= 25, "at least 25 independent mutants are required")
    goldens = load_goldens()
    require({row["kind"] for row in goldens} == set(KINDS), "missing kind golden")
    for rows in (
        goldens, load_malformed(),
        json.loads((FIXTURES / "replay.json").read_text(encoding="utf-8")),
        json.loads((FIXTURES / "bounds.json").read_text(encoding="utf-8")),
    ):
        require(len({row["name"] for row in rows}) == len(rows), "duplicate fixture name")
    for name, test in TESTS.items():
        try:
            test(Rules())
        except (AssertionError, OracleError) as error:
            raise AssertionError(f"trace {name}: {error}") from error
    killed = []
    survivors = []
    for mutant, name in MUTANTS.items():
        try:
            TESTS[name](Rules(mutant))
        except (AssertionError, OracleError) as error:
            killed.append({"mutant": mutant, "killed_by": name, "failure": str(error)})
        else:
            survivors.append(mutant)
    require(not survivors, f"surviving mutants: {survivors}")
    return {
        "goldens": len(goldens), "malformed": len(load_malformed()),
        "replay_vectors": len(json.loads((FIXTURES / "replay.json").read_text(encoding="utf-8"))),
        "bound_vectors": len(json.loads((FIXTURES / "bounds.json").read_text(encoding="utf-8"))),
        "truncations": sum(len(bytes.fromhex(str(row["expected_hex"]))) for row in goldens),
        "named_traces": len(TESTS), "mutants_killed": len(killed),
        "survivors": survivors, "mutation_results": killed,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--list-traces", action="store_true")
    parser.add_argument("--json", action="store_true", help="print detailed self-test results")
    args = parser.parse_args()
    if args.list_traces:
        print("\n".join(TESTS))
        return 0
    if not args.self_test:
        parser.error("choose --self-test or --list-traces")
    result = self_test()
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(
            f"lease_ledger_oracle: {result['goldens']} goldens, "
            f"{result['malformed']} malformed, {result['truncations']} truncations, "
            f"{result['replay_vectors']} replay, {result['bound_vectors']} bounds, "
            f"{result['named_traces']} traces; "
            f"{result['mutants_killed']} mutants killed, zero survivors",
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
