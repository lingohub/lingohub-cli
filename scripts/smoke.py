#!/usr/bin/env python3
# Copyright (c) 2026 lingohub GmbH and/or its affiliates.
# SPDX-License-Identifier: Apache-2.0
"""Smoke-test a built lingohub binary offline: version, help, operation inventory and linkage."""
import argparse
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PT_INTERP = 3


def has_interpreter(data):
    """True when a 64-bit little-endian ELF executable requests a dynamic loader."""
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("Not a 64-bit little-endian ELF executable")
    offset, = struct.unpack_from("<Q", data, 32)
    size, count = struct.unpack_from("<HH", data, 54)
    return any(struct.unpack_from("<I", data, offset + index * size)[0] == PT_INTERP for index in range(count))


def operation_count():
    spec = json.loads((ROOT / "contract/v2.json").read_bytes())
    return sum("operationId" in operation for item in spec["paths"].values() for operation in item.values() if isinstance(operation, dict))


def smoke(binary, static):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    # Offline commands resolve before an API client exists; no key or network is needed.
    env = {key: value for key, value in os.environ.items() if not key.startswith("LINGOHUB_")}
    with tempfile.TemporaryDirectory(prefix="lingohub-smoke-") as directory:
        def run(*arguments):
            return subprocess.run([str(binary), *arguments], cwd=directory, env=env, check=True, capture_output=True, text=True).stdout
        if run("--version").strip() != f"lingohub {version}":
            raise ValueError("Executable version differs from the Cargo package version")
        run("--help")
        run("upgrade", "--help")
        if len(json.loads(run("api", "operations"))) != operation_count():
            raise ValueError("Executable operation inventory differs from the bundled contract")
    if static and has_interpreter(binary.read_bytes()):
        raise ValueError("Linux release binaries must be statically linked")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--static", action="store_true", help="require a statically linked Linux executable")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.exists() and binary.with_suffix(".exe").exists():
        binary = binary.with_suffix(".exe")
    try:
        smoke(binary, args.static)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(f"Smoke test failed: {error}")
    print(f"{binary.name} passed the offline smoke test")


if __name__ == "__main__":
    main()
