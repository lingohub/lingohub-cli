#!/usr/bin/env python3
# Copyright (c) 2026 lingohub GmbH and/or its affiliates.
# SPDX-License-Identifier: Apache-2.0
"""Refresh the bundled V2 contract and honest command inventory from a clean backend revision."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("backend", type=Path)
args = parser.parse_args()
backend = args.backend.resolve()
root = Path(__file__).resolve().parents[1]
if subprocess.check_output(["git", "status", "--porcelain"], cwd=backend, text=True).strip():
    parser.error("Backend checkout must be clean so the contract has committed provenance")
try:
    origin = subprocess.check_output(["git", "remote", "get-url", "origin"], cwd=backend, text=True, stderr=subprocess.DEVNULL).strip().removesuffix(".git").rstrip("/")
except subprocess.CalledProcessError:
    parser.error("Backend checkout must have the official origin configured")
if origin not in {"https://github.com/lingohub/lh-backend", "git@github.com:lingohub/lh-backend", "ssh://git@github.com/lingohub/lh-backend"}:
    parser.error("Backend origin must identify lingohub/lh-backend on github.com")
revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=backend, text=True).strip()
# Older backend revisions keep the snapshot under integrationTest.
for source in ("src/main/resources/api-v2-documentation/openapi.json", "src/integrationTest/resources/openapi/v2.json"):
    if subprocess.run(["git", "cat-file", "-e", f"{revision}:{source}"], cwd=backend, capture_output=True).returncode == 0:
        break
else:
    parser.error("Backend revision contains no V2 contract snapshot")
content = subprocess.check_output(["git", "show", f"{revision}:{source}"], cwd=backend)
spec = json.loads(content)
(root / "contract/v2.json").write_bytes(content)
(root / "contract/provenance.json").write_text(json.dumps({"repository": "lingohub/lh-backend", "revision": revision, "path": source, "sha256": hashlib.sha256(content).hexdigest()}, indent=2) + "\n")
lines = ["# V2 CLI operation inventory", "", f"Backend revision: `{revision}`. This records addressability, not completed workflow tests.", "", f"Named-operation support does not establish a complete workflow; the [workflow cookbook](cookbook.md) shows complete examples. JSON operations support one-request `api call OPERATION --path NAME=VALUE --query NAME=VALUE`; resource commands also cover workspace and stack/project/style-guide CRUD. Multipart operations use `api upload OPERATION --upload PATH --form NAME=VALUE`; completed export artifacts use `api download-export EXPORT_ID --output PATH`, individual entries use `api download-export-entry EXPORT_ID ENTRY_ID --output PATH`, distribution releases use `api download-distribution-release RELEASE_ID --output PATH`; known async resources use `api wait OPERATION --path NAME=VALUE`; dedicated commands for other groups remain pending.", "", "| Operation | Method / path | CLI support |", "|---|---|---|"]
for path, item in spec["paths"].items():
    for method, op in item.items():
        if "operationId" not in op:
            continue
        media = op.get("requestBody", {}).get("content", {})
        supported = not media or any(m == "application/json" or m.endswith("+json") for m in media)
        support = "JSON operation call" if supported else ("Multipart operation upload" if "multipart/form-data" in media else "Pending binary workflow")
        if op["operationId"] == "getExport":
            support += "; completed artifact download"
        if op["operationId"] == "getDistributionReleaseDownload":
            support += "; completed release archive download"
        if op["operationId"] == "listExportEntries":
            support += "; individual entry download"
        if op["operationId"] in {"getImport", "getExport", "getBranch", "getWebhookDelivery", "getBackup", "getBackupRestore", "getProjectMove", "getLinaExecution"}:
            support += "; bounded async wait"
        lines.append(f"| `{op['operationId']}` | `{method.upper()} {path}` | {support} |")
(root / "docs/operations.md").write_text("\n".join(lines) + "\n")
print(f"Updated {len(lines) - 8} operation rows from {revision}")
