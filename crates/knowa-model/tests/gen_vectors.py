"""Independent oracle for knowa-model golden vectors.

Uses only Python's json/hashlib/base64/datetime plus `cryptography` and `nacl` for
Ed25519 — nothing from the Rust implementation. Python's
json.dumps(sort_keys=True, separators=(',', ':'), ensure_ascii=False) equals RFC 8785
for Knowa's subset (ASCII keys, integers only); escaping was probed separately.
"""

import base64
import datetime as dt
import hashlib
import json
import sys

import nacl.signing
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

SIG_DOMAIN = b"knowa/claim/v1\n"


def jcs(obj) -> str:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def b32(b: bytes) -> str:
    return base64.b32encode(b).decode().lower().rstrip("=")


def mh(b: bytes) -> bytes:
    return b"\x12\x20" + hashlib.sha256(b).digest()


def b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).decode().rstrip("=")


def sign(seed: bytes, msg: bytes) -> bytes:
    a = Ed25519PrivateKey.from_private_bytes(seed).sign(msg)
    b = nacl.signing.SigningKey(seed).sign(msg).signature
    assert a == b, "cryptography and nacl disagree"
    return a


def pubkey(seed: bytes) -> bytes:
    from cryptography.hazmat.primitives import serialization

    a = Ed25519PrivateKey.from_private_bytes(seed).public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )
    b = bytes(nacl.signing.SigningKey(seed).verify_key)
    assert a == b
    return a


# ---- (a) JCS ----
JCS_ACCEPT = [
    ("key order is by UTF-16 code unit (= byte order for ASCII)", '{"b":1,"a":2,"A":3,"_":4,"1":5,"aa":6,"":7,"~":8}'),
    ("nested objects and arrays sort recursively; arrays keep order", '{"z":{"y":[3,{"b":true,"a":null}],"x":"s"},"a":[],"m":{}}'),
    ("whitespace is removed", ' { "a" : [ 1 , 2 ] ,\n\t"b" : { } } '),
    ("control characters: short forms and lowercase \\u00xx", '"\\u0000\\u0001\\b\\t\\n\\u000b\\f\\r\\u000e\\u001f"'),
    ("quote and backslash escaped; slash, DEL, U+2028/2029 verbatim", '"\\" \\\\ \\/ / \\u007f \\u2028 \\u2029"'),
    ("non-ASCII verbatim as UTF-8, incl. astral plane", '{"text":"日本語 café \\ud83d\\ude00 😀"}'),
    ("integers at the safe limits", '[0,-1,1,9007199254740991,-9007199254740991]'),
    ("literals", '[true,false,null]'),
]
JCS_REJECT = [
    ("float", '{"a":1.5}'),
    ("integral float", "1.0"),
    ("exponent", "1e3"),
    ("negative zero float", "-0.0"),
    ("-0 (parsers disagree whether it is an integer; Knowa input never needs it)", "-0"),
    ("above 2^53 - 1", "9007199254740992"),
    ("below -(2^53 - 1)", "-9007199254740992"),
    ("u64 max", "18446744073709551615"),
    ("non-ASCII key", '{"é":1}'),
    ("nested non-ASCII key", '{"a":[{"ключ":1}]}'),
]

jcs_cases = []
for name, text in JCS_ACCEPT:
    jcs_cases.append({"name": name, "input": text, "canonical": jcs(json.loads(text))})

# ---- (b) blob ids ----
blobs = []
for data in [b"", b"hello", bytes(range(256))]:
    blobs.append({"bytes_hex": data.hex(), "id": "b_" + b32(mh(data))})

# ---- (c) actor ----
seed = bytes(range(32))
pk = pubkey(seed)
actor_id = "a_" + b32(b"\xed\x01" + pk)
actor = {"seed_hex": seed.hex(), "pubkey_hex": pk.hex(), "id": actor_id}

# ---- (d) claims ----
space = "s_01jdfq6tz3m8x9v2k4n5p7r8s0"
entity = "e_01jdfq7a2b3c4d5e6f7g8h9j0k"
hello_blob = "b_" + b32(mh(b"hello"))


def seal(body: dict) -> dict:
    canonical = jcs(body).encode("utf-8")
    cid = "c_" + b32(mh(canonical))
    sig = b64url(sign(seed, SIG_DOMAIN + canonical))
    full = dict(body, id=cid, sig=sig)
    key_doc = {k: body[k] for k in ("s", "p", "o", "q", "valid") if k in body}
    return {
        "body": body,
        "canonical": canonical.decode("utf-8"),
        "id": cid,
        "sig": sig,
        "line": jcs(full) + "\n",
        "equiv_key": b32(mh(jcs(key_doc).encode("utf-8"))),
    }


genesis = seal(
    {
        "v": 1,
        "space": space,
        "actor": actor_id,
        "seq": 1,
        "at": "2026-11-21T10:00:00.123Z",
        "s": space,
        "p": "knowa:owner",
        "o": {"ref": actor_id},
    }
)
full = seal(
    {
        "v": 1,
        "space": space,
        "actor": actor_id,
        "seq": 2,
        "prev": genesis["id"],
        "at": "2026-11-21T10:00:01.000Z",
        "s": entity,
        "p": "ex:note",
        "o": {"text": 'Ünïcödé "q" \\ / \n\t\x01   😀', "lang": "en-GB"},
        "q": {
            "ex:tag": [{"term": "ex:Draft"}, {"int": -42}],
            "ex:at": [{"geo": {"lat": "35.68", "lon": "139.76", "r": "25"}}],
            "ex:size": [{"qty": "12.50", "unit": "kg"}, {"dec": "-0.5"}, {"bool": False}, {"when": "2024-05-01T10:00"}],
        },
        "valid": "2024-05~/2024-06",
        "conf": "0.8",
        "src": [hello_blob + "#t=3,9.5", genesis["id"], "x:wd:Q42"],
        "replaces": [genesis["id"]],
        "via": {"tool": "knowa/0.1.0", "model": "m-1", "worker": "exif@1"},
        "note": "why this claim was made",
    }
)

# ---- timestamps (canonical form ↔ Unix ms), via Python's datetime ----
EPOCH = dt.datetime(1970, 1, 1)


def ms(y, mo, d, h=0, mi=0, s=0, milli=0) -> int:
    return (dt.datetime(y, mo, d, h, mi, s, milli * 1000) - EPOCH) // dt.timedelta(milliseconds=1)


timestamps = [
    {"text": "1970-01-01T00:00:00.000Z", "ms": 0},
    {"text": "1969-12-31T23:59:59.999Z", "ms": ms(1969, 12, 31, 23, 59, 59, 999)},
    {"text": "2026-11-21T10:00:00.123Z", "ms": ms(2026, 11, 21, 10, 0, 0, 123)},
    {"text": "2000-02-29T12:34:56.789Z", "ms": ms(2000, 2, 29, 12, 34, 56, 789)},
    {"text": "1900-03-01T00:00:00.000Z", "ms": ms(1900, 3, 1)},
    {"text": "0001-01-01T00:00:00.000Z", "ms": ms(1, 1, 1)},
    # Python has no year 0; proleptic year 0000 (= 1 BC) is a leap year of 366 days.
    {"text": "0000-01-01T00:00:00.000Z", "ms": ms(1, 1, 1) - 366 * 86_400_000},
    {"text": "9999-12-31T23:59:59.999Z", "ms": ms(9999, 12, 31, 23, 59, 59, 999)},
]

vectors = {
    "_comment": "Knowa conformance vectors. Generated by an independent Python oracle "
    "(json/hashlib/base64/datetime, Ed25519 via cryptography and PyNaCl, cross-checked). "
    "A change here is a format change.",
    "jcs": jcs_cases,
    "jcs_reject": [{"name": n, "input": t} for n, t in JCS_REJECT],
    "blob": blobs,
    "actor": actor,
    "claims": [genesis, full],
    "timestamps": timestamps,
}
json.dump(vectors, sys.stdout, indent=2, ensure_ascii=False)
sys.stdout.write("\n")
