#!/usr/bin/env python3
"""Test-only trust fixture. Ephemeral keys cannot authorize Marketplace release."""
import base64
import hashlib
import io
import json
from pathlib import Path
import stat
import sys
import unittest
import zipfile

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from package import DOMAIN, archive, canonical


def verify(data, public_key):
    with zipfile.ZipFile(io.BytesIO(data)) as bundle:
        records = bundle.infolist()
        if len(records) > 10000 or sum(r.file_size for r in records) > 250 * 1024 ** 2:
            raise ValueError("archive limits")
        names = [r.filename for r in records]
        if len(names) != len(set(n.casefold() for n in names)):
            raise ValueError("duplicate names")
        for record in records:
            name = record.filename
            if not name.isascii() or "\\" in name or ":" in name or "\0" in name or any(
                    p in ("", ".", "..") for p in name.split("/")):
                raise ValueError("invalid path")
            mode = record.external_attr >> 16
            if stat.S_ISLNK(mode) or mode & 0o111:
                raise ValueError("non-resource archive mode")
        def unique(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError("duplicate JSON key")
                result[key] = value
            return result
        manifest = json.loads(bundle.read("manifest.json"), object_pairs_hook=unique)
        signature = manifest.pop("signature")
        if signature["algorithm"] != "Ed25519" or signature["keyId"] != "TEST-ONLY":
            raise ValueError("fixture signer")
        public_key.verify(base64.urlsafe_b64decode(signature["value"] + "=="), DOMAIN + canonical(manifest))
        if set(names) != {"manifest.json", *manifest["files"]}:
            raise ValueError("unlisted resource")
        for name, record in manifest["files"].items():
            resource = bundle.read(name)
            if len(resource) != record["bytes"] or hashlib.sha256(resource).hexdigest() != record["sha256"]:
                raise ValueError("resource digest mismatch")
        return manifest


class PackageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.resources = Path(sys.argv[1])
        cls.manifest = json.loads((cls.resources / "manifest.unsigned.json").read_text())
        cls.key = Ed25519PrivateKey.generate()
        cls.signed = {**cls.manifest, "signature": {
            "algorithm": "Ed25519", "keyId": "TEST-ONLY", "value": base64.urlsafe_b64encode(
                cls.key.sign(DOMAIN + canonical(cls.manifest))).rstrip(b"=").decode()}}
        cls.path = cls.resources.parent / "TEST-ONLY-chatgpt-auto-confirm.fabapp"
        archive(cls.resources, cls.signed, cls.path)
        cls.data = cls.path.read_bytes()

    def mutate(self, name, content):
        out = io.BytesIO()
        with zipfile.ZipFile(io.BytesIO(self.data)) as original, zipfile.ZipFile(out, "w") as changed:
            for item in original.infolist():
                changed.writestr(item, content if item.filename == name else original.read(item))
            if name not in original.namelist():
                item = zipfile.ZipInfo(name); item.external_attr = 0o100644 << 16
                changed.writestr(item, content)
        return out.getvalue()

    def test_complete_package(self):
        result = verify(self.data, self.key.public_key())
        self.assertEqual(result["appId"], "org.fabushi.chatgpt-auto-confirm")
        self.assertTrue(result["offlineCore"])
        self.assertEqual(result["networkPolicy"], {"mode": "deny", "allowedOrigins": []})

    def test_resource_tamper(self):
        with self.assertRaises(ValueError):
            verify(self.mutate("app.js", b"tampered"), self.key.public_key())

    def test_extra_file(self):
        with self.assertRaises(ValueError):
            verify(self.mutate("extra.js", b"x"), self.key.public_key())

    def test_traversal(self):
        with self.assertRaises(ValueError):
            verify(self.mutate("../escape", b"x"), self.key.public_key())

    def test_wrong_key(self):
        with self.assertRaises(Exception):
            verify(self.data, Ed25519PrivateKey.generate().public_key())

    def test_manifest_tamper(self):
        broken = {**self.signed, "version": "9.0.0"}
        with self.assertRaises(Exception):
            verify(self.mutate("manifest.json", canonical(broken)), self.key.public_key())

    def test_no_unsigned_fabapp(self):
        with self.assertRaises(ValueError):
            archive(self.resources, self.manifest, self.resources.parent / "unsigned.fabapp")


if __name__ == "__main__":
    # Retain resources argument for setUpClass, omit it from unittest selection.
    unittest.main(argv=[sys.argv[0]], verbosity=2)
