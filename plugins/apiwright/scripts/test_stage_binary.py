import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import stage_binary


class StageBinaryTest(unittest.TestCase):
    def test_release_metadata(self) -> None:
        self.assertEqual(
            stage_binary.release_target("Linux", "amd64"),
            ("linux-x86_64", "apiwright"),
        )
        self.assertEqual(
            stage_binary.release_target("Windows", "x86_64"),
            ("windows-x86_64", "apiwright.exe"),
        )
        self.assertEqual(
            stage_binary.release_target("Darwin", "arm64"),
            ("macOS-arm64", "apiwright"),
        )
        self.assertEqual(
            stage_binary.expected_digest(
                "abc  other\n123 *ApiWright-1.2.3-cli-linux-x86_64\n",
                "ApiWright-1.2.3-cli-linux-x86_64",
            ),
            "123",
        )

        with tempfile.TemporaryDirectory() as directory:
            plugin = Path(directory)
            (plugin / ".codex-plugin").mkdir()
            (plugin / ".codex-plugin" / "plugin.json").write_text(
                json.dumps({"version": "1.2.3+codex.local-test"}),
                encoding="utf-8",
            )
            with patch.object(stage_binary, "PLUGIN_ROOT", plugin):
                self.assertEqual(stage_binary.plugin_version(), "1.2.3")
                binary = b"verified release binary"
                checksums = (
                    f"{hashlib.sha256(binary).hexdigest()}  "
                    "ApiWright-1.2.3-cli-linux-x86_64\n"
                ).encode()
                with (
                    patch.object(
                        stage_binary,
                        "fetch",
                        side_effect=[io.BytesIO(checksums), io.BytesIO(binary)],
                    ),
                    patch.object(stage_binary.platform, "system", return_value="Linux"),
                    patch.object(stage_binary.platform, "machine", return_value="x86_64"),
                ):
                    destination = stage_binary.download_release()
                self.assertEqual(destination.read_bytes(), binary)

                source = plugin / "built-apiwright"
                source.write_bytes(b"local release binary")
                source.chmod(0o755)
                with patch.object(stage_binary, "local_build", return_value=source):
                    destination = stage_binary.stage_local_build()
                self.assertEqual(destination.read_bytes(), b"local release binary")
                self.assertTrue(destination.stat().st_mode & 0o111)

        with self.assertRaisesRegex(RuntimeError, "No ApiWright release binary"):
            stage_binary.release_target("Darwin", "x86_64")


if __name__ == "__main__":
    unittest.main()
