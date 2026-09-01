#!/usr/bin/env python3
"""Generate interop contract fixtures using the ORIGINAL Python CyDrive code.

Outputs into crates/cydrive-core/tests/compat/fixtures/:
  - crypto_vector.json : ciphertext produced by cydrive.crypto.CyCrypto (AES-256-GCM v1 format)
  - python_meta.sql    : SQL dump of a database created by cydrive.database.MetaDatabase

Run from anywhere: paths are resolved relative to this file and the Python repo.
"""

import base64
import json
import os
import sqlite3
import sys
import tempfile

CYDRIVE_PY_REPO = r"E:\GitHub\CyDrive"
HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.normpath(
    os.path.join(HERE, "..", "crates", "cydrive-core", "tests", "compat", "fixtures")
)

sys.path.insert(0, CYDRIVE_PY_REPO)
os.makedirs(FIXTURES, exist_ok=True)
tmp = tempfile.mkdtemp(prefix="cydrive_fixtures_")

# ---------------------------------------------------------------- crypto ----
from cydrive.crypto import CyCrypto  # noqa: E402

PASSWORD = "cydrive-interop-vector-pw"
PLAINTEXT = (b"CyDrive Rust rewrite interop vector.\n" * 16) + bytes(range(256))

src = os.path.join(tmp, "plain.bin")
enc = os.path.join(tmp, "enc.bin")
dec = os.path.join(tmp, "dec.bin")
with open(src, "wb") as f:
    f.write(PLAINTEXT)

crypto = CyCrypto(password=PASSWORD)
assert crypto.encrypt_file(src, enc), "python encrypt failed"
assert CyCrypto(password=PASSWORD).decrypt_file(enc, dec), "python decrypt failed"
with open(dec, "rb") as f:
    assert f.read() == PLAINTEXT, "python roundtrip mismatch"

with open(enc, "rb") as f:
    ciphertext = f.read()
assert len(ciphertext) == len(PLAINTEXT) + 16 + 12 + 16  # salt+nonce+tag

vector = {
    "password": PASSWORD,
    "plaintext_base64": base64.b64encode(PLAINTEXT).decode("ascii"),
    "ciphertext_base64": base64.b64encode(ciphertext).decode("ascii"),
}
with open(os.path.join(FIXTURES, "crypto_vector.json"), "w", encoding="utf-8") as f:
    json.dump(vector, f, indent=2)

# ------------------------------------------------------------- database ----
from cydrive.database import MetaDatabase  # noqa: E402

db_path = os.path.join(tmp, "meta.db")
db = MetaDatabase(db_path=db_path)

MB = 1024 * 1024
db.upsert_file("/Documents", "Documents", "/", is_dir=True)
db.upsert_file(
    "/Documents/report.pdf", "report.pdf", "/Documents",
    size=1024, mtime=1700000000.5, sha256="ab" * 32,
    is_uploaded=True, telegram_msg_id=111, mime_type="application/pdf",
)
db.upsert_file("/notes.txt", "notes.txt", "/", size=42, mtime=1700000100.0, is_uploaded=False)
db.upsert_file("/big", "big", "/", is_dir=True)
fid_movie = db.upsert_file(
    "/big/movie.mkv", "movie.mkv", "/big",
    size=3 * 1900 * MB, mtime=1700000200.0,
    is_uploaded=True, telegram_msg_id=200, is_encrypted=False, chunk_count=3,
)
for idx, (msg_id, size) in enumerate([(201, 1900 * MB), (202, 1900 * MB), (203, 1024)]):
    db.upsert_chunk(fid_movie, idx, msg_id, size)

conn = sqlite3.connect(db_path)
dump = "\n".join(conn.iterdump()) + "\n"
conn.close()
with open(os.path.join(FIXTURES, "python_meta.sql"), "w", encoding="utf-8") as f:
    f.write(dump)

print("fixtures written to", FIXTURES)
for name in sorted(os.listdir(FIXTURES)):
    path = os.path.join(FIXTURES, name)
    print(f"  {name}: {os.path.getsize(path)} bytes")
