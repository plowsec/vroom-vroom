#!/usr/bin/env python3
"""Add X.509 certificates to db or KEK in an EDK2 NVRAM image, in place.

  scripts/enroll-keys.py VARS.fd [--db CERT.der]... [--kek CERT.der]...

The live variable is marked deleted and a new one, with the certificates
appended (owner: Microsoft), goes after the last variable, as EDK2 itself
does. Certificates already there are skipped. Authenticated store only.
"""
import argparse
import struct
import sys
import uuid

AUTH_STORE = uuid.UUID("aaf32c78-947b-439a-a180-2e144ec37792")
GLOBAL = uuid.UUID("8be4df61-93ca-11d2-aa0d-00e098032b8c")
IMAGE_SECURITY = uuid.UUID("d719b2cb-3d3a-4596-a3bc-dad00e67656f")
CERT_X509 = uuid.UUID("a5c059a1-94e4-4aa7-87b5-ab155c2bf072")
MICROSOFT = uuid.UUID("77fa9abd-0359-4d32-bd60-28f4e78f784b")
HEADER = 60  # AUTHENTICATED_VARIABLE_HEADER
ADDED, DELETED = 0x3F, 0x3C


def variables(d, start, end):
    """(offset, state, name, guid, name size, data size) of each record."""
    o = start
    while o + HEADER <= end and struct.unpack_from("<H", d, o)[0] == 0x55AA:
        ns, ds = struct.unpack_from("<II", d, o + 36)
        name = d[o + HEADER : o + HEADER + ns].decode("utf-16-le").rstrip("\0")
        yield o, d[o + 2], name, uuid.UUID(bytes_le=bytes(d[o + 44 : o + 60])), ns, ds
        o = (o + HEADER + ns + ds + 3) & ~3


def enroll(d, start, end, name, guid, certs):
    live = [v for v in variables(d, start, end) if v[1] == ADDED and v[2] == name and v[3] == guid]
    if len(live) != 1:
        sys.exit(f"{name}: {len(live)} live copies, expected 1")
    o, _, _, _, ns, ds = live[0]
    data = bytes(d[o + HEADER + ns : o + HEADER + ns + ds])
    new = [c for c in certs if c not in data]
    if not new:
        return
    for c in new:
        size = 16 + len(c)
        data += CERT_X509.bytes_le + struct.pack("<III", 28 + size, 0, size) + MICROSOFT.bytes_le + c
    tail = max(((v[0] + HEADER + v[4] + v[5] + 3) & ~3) for v in variables(d, start, end))
    rec = bytearray(d[o : o + HEADER + ns])
    struct.pack_into("<I", rec, 40, len(data))
    rec += data
    if tail + len(rec) > end or any(b != 0xFF for b in d[tail : tail + len(rec)]):
        sys.exit(f"{name}: no free space after the last variable")
    d[tail : tail + len(rec)] = rec
    d[o + 2] = DELETED
    print(f"{name}: +{len(new)} certificate(s)")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("vars")
    p.add_argument("--db", action="append", default=[])
    p.add_argument("--kek", action="append", default=[])
    a = p.parse_args()
    d = bytearray(open(a.vars, "rb").read())
    vs = struct.unpack_from("<H", d, 0x30)[0]  # firmware volume header length
    if uuid.UUID(bytes_le=bytes(d[vs : vs + 16])) != AUTH_STORE:
        sys.exit(f"{a.vars}: not an authenticated variable store")
    start, end = vs + 28, vs + struct.unpack_from("<I", d, vs + 16)[0]
    read = lambda f: open(f, "rb").read()
    enroll(d, start, end, "db", IMAGE_SECURITY, [read(f) for f in a.db])
    enroll(d, start, end, "KEK", GLOBAL, [read(f) for f in a.kek])
    open(a.vars, "wb").write(d)


if __name__ == "__main__":
    main()
