# Copyright (c) 2026 lingohub GmbH and/or its affiliates.
# SPDX-License-Identifier: Apache-2.0
import importlib.util
from pathlib import Path
import struct
import unittest

spec = importlib.util.spec_from_file_location("smoke", Path(__file__).with_name("smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def elf(*program_types):
    """A minimal ELF64 header followed by one program header per type."""
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<Q", header, 32, 64)
    struct.pack_into("<HH", header, 54, 56, len(program_types))
    return bytes(header) + b"".join(struct.pack("<I", kind) + bytes(52) for kind in program_types)


class SmokeTest(unittest.TestCase):
    def test_static_and_dynamic_executables_are_told_apart(self):
        self.assertFalse(smoke.has_interpreter(elf(1, 2, 4)))
        self.assertTrue(smoke.has_interpreter(elf(6, smoke.PT_INTERP, 1)))

    def test_non_elf_input_is_rejected(self):
        with self.assertRaises(ValueError):
            smoke.has_interpreter(b"\xcf\xfa\xed\xfe" + bytes(60))

    def test_the_bundled_contract_has_operations(self):
        self.assertGreater(smoke.operation_count(), 0)


if __name__ == "__main__":
    unittest.main()
