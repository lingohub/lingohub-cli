# Copyright (c) 2026 lingohub GmbH and/or its affiliates.
# SPDX-License-Identifier: Apache-2.0
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

OFFICIAL = "https://github.com/lingohub/lh-backend.git"
SNAPSHOT = "src/main/resources/api-v2-documentation/openapi.json"
LEGACY_SNAPSHOT = "src/integrationTest/resources/openapi/v2.json"


class ContractSyncTest(unittest.TestCase):
    def test_origin_identity_is_checked_before_replacing_contract(self):
        for origin, expected in [("https://github.com/elsewhere/backend.git", False),
                                 (OFFICIAL, True),
                                 ("git@github.com:lingohub/lh-backend.git", True)]:
            with self.subTest(origin=origin):
                self.assert_sync(origin, SNAPSHOT, expected)

    def test_revisions_before_the_snapshot_move_sync_from_the_legacy_path(self):
        self.assert_sync(OFFICIAL, LEGACY_SNAPSHOT, True)

    def test_revision_without_a_snapshot_is_rejected(self):
        self.assert_sync(OFFICIAL, "src/main/resources/elsewhere.json", False)

    def assert_sync(self, origin, snapshot, expected):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            backend = root / "backend"
            backend.mkdir()
            def git(*args):
                return subprocess.run(["git", *args], cwd=backend, check=True, capture_output=True)
            git("init")
            git("remote", "add", "origin", origin)
            source = backend / snapshot
            source.parent.mkdir(parents=True)
            source.write_text(json.dumps({"paths": {}}))
            git("add", ".")
            git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-m", "Fixture")
            cli = root / "cli"
            for folder in ["scripts", "contract", "docs"]:
                (cli / folder).mkdir(parents=True)
            script = cli / "scripts/sync-contract.py"
            shutil.copyfile(Path(__file__).with_name("sync-contract.py"), script)
            contract = cli / "contract/v2.json"
            contract.write_text("unchanged")
            committed = source.read_text()
            if expected:
                # A clean status can hide local bytes; provenance must use the Git blob.
                git("update-index", "--assume-unchanged", snapshot)
                source.write_text('{"paths":{"/uncommitted":{}}}')
            result = subprocess.run([sys.executable, str(script), str(backend)], capture_output=True, text=True)
            self.assertEqual(result.returncode == 0, expected, result.stderr)
            self.assertEqual(contract.read_text(), committed if expected else "unchanged")
            if expected:
                provenance = json.loads((cli / "contract/provenance.json").read_text())
                self.assertEqual(provenance["path"], snapshot)
