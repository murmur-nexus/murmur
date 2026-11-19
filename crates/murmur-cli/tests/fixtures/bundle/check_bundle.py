#!/usr/bin/env python3
"""Check a murmur.bundle/1 directory with its exporting host's public key and nothing else.

Written from docs/content/reference/run-bundle.md and the pages it links, without Murmur:
  "Layout"                  where bundle.json, trace.jsonl, its seal and the blobs sit
  "bundle.json"             the keys read here: format, files[].path, .bytes, .sha256
  "Check without Murmur"    step 1 (manifest hashes), step 2 (blob names), step 3 (the trace
                            chain and seal, per observability-schemas.md "Sealed records")

Usage: python3 -I check_bundle.py <bundle dir> <public key file>
Prints one line per failure and exits 1 on any, 0 when every check holds.
"""
import base64
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

# DER prefix of an Ed25519 SubjectPublicKeyInfo; the raw 32-byte key follows it.
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
HEX = set("0123456789abcdef")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def line_hash(line):
    return "sha256:" + sha256(line)


def check_manifest(bundle, manifest, fail):
    if manifest.get("format") != "murmur.bundle/1":
        fail(f"bundle.json: format is {manifest.get('format')!r}, not murmur.bundle/1")
    for entry in manifest.get("files", []):
        path = bundle / entry["path"]
        if path.is_symlink() or not path.is_file():
            fail(f"{entry['path']}: listed in bundle.json, not in the bundle")
            continue
        data = path.read_bytes()
        if len(data) != entry["bytes"] or sha256(data) != entry["sha256"]:
            fail(f"{entry['path']}: sha256 {sha256(data)}, bundle.json lists {entry['sha256']}")


def check_blobs(bundle, fail):
    for blobs in [bundle / "blobs", *sorted(bundle.glob("conversations/*/*/blobs"))]:
        if not blobs.is_dir():
            continue
        for blob in sorted(blobs.iterdir()):
            name = blob.name
            if len(name) != 64 or not set(name) <= HEX or not blob.is_file():
                continue
            actual = sha256(blob.read_bytes())
            if actual != name:
                fail(f"{blob.relative_to(bundle)}: holds sha256 {actual}, not its name")


def verify_signature(public_key, message, signature_hex):
    pem = (
        "-----BEGIN PUBLIC KEY-----\n"
        + base64.b64encode(ED25519_SPKI_PREFIX + public_key).decode()
        + "\n-----END PUBLIC KEY-----\n"
    )
    with tempfile.TemporaryDirectory() as tmp:
        files = {"key.pem": pem.encode(), "message": message, "sig": bytes.fromhex(signature_hex)}
        for name, data in files.items():
            (Path(tmp) / name).write_bytes(data)
        result = subprocess.run(
            ["openssl", "pkeyutl", "-verify", "-pubin", "-inkey", f"{tmp}/key.pem",
             "-rawin", "-in", f"{tmp}/message", "-sigfile", f"{tmp}/sig"],
            capture_output=True,
        )
    return result.returncode == 0


def check_trace(bundle, public_key, fail):
    lines = (bundle / "trace.jsonl").read_bytes().split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    previous = "genesis"
    for number, line in enumerate(lines, 1):
        if not line.endswith(f'"prev_hash":"{previous}"}}'.encode()):
            fail(f"trace.jsonl line {number}: does not end with the prev_hash {previous}")
            return
        previous = line_hash(line)
    seal = json.loads((bundle / "trace.jsonl.seal").read_text())
    key_id = sha256(public_key)[:16]
    if seal["key_id"] != key_id:
        fail(f"trace.jsonl.seal: sealed by key {seal['key_id']}, not {key_id}")
    if seal["stream"] != "trace.jsonl" or seal["lines"] != len(lines):
        fail(f"trace.jsonl.seal: covers {seal['lines']} lines of {seal['stream']}, "
             f"trace.jsonl holds {len(lines)}")
    elif seal["head"] != line_hash(lines[-1]):
        fail(f"trace.jsonl.seal: head {seal['head']} is not line {len(lines)}'s hash")
    message = (f"murmur.seal/1\n{seal['stream']}\n{seal['session_id']}\n"
               f"{seal['lines']}\n{seal['head']}\n").encode()
    if not verify_signature(public_key, message, seal["signature"]):
        fail("trace.jsonl.seal: the signature does not verify with the public key")


def main():
    if len(sys.argv) != 3:
        print("usage: check_bundle.py <bundle dir> <public key file>")
        return 2
    bundle, key_file = Path(sys.argv[1]), Path(sys.argv[2])
    public_key = bytes.fromhex(key_file.read_text().strip())
    failures = []
    manifest = json.loads((bundle / "bundle.json").read_text())
    check_manifest(bundle, manifest, failures.append)
    check_blobs(bundle, failures.append)
    check_trace(bundle, public_key, failures.append)
    for failure in failures:
        print(failure)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
