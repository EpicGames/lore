# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from lore_youtrack.config import ConfigError, Secret, load_config


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.path = self.directory / "bridge.toml"
        self.text = """[bridge]
repository = "cache"
database = "state/outbox.sqlite3"
[youtrack]
receiver_url = "https://yt.invalid/api/vcsHooksReceiver/generic/1-1"
signature_env = "BRIDGE_SECRET"
[authors.alice]
user_name = "alice"
email = "alice@example.invalid"
"""

    def config(self, text=None):
        self.path.write_text(text or self.text, encoding="utf-8")
        return load_config(self.path)

    def test_paths_and_author_mapping_are_operator_owned(self):
        config = self.config()
        self.assertEqual(config.database, self.directory / "state/outbox.sqlite3")
        self.assertEqual(config.authors["alice"]["userName"], "alice")
        self.assertEqual(config.branches, ("main",))
        self.assertEqual(config.signature, Secret(env="BRIDGE_SECRET"))

    def test_credentials_are_read_only_when_requested(self):
        config = self.config()
        with patch.dict("os.environ", {"BRIDGE_SECRET": "literal-secret"}, clear=True):
            self.assertEqual(config.signature.read(), "literal-secret")
        with patch.dict("os.environ", {"BRIDGE_SECRET": "bad\r\nvalue"}, clear=True):
            with self.assertRaises(ConfigError) as error:
                config.signature.read()
            self.assertNotIn("bad", str(error.exception))

    def test_secret_file_allows_terminal_newline_but_rejects_empty_and_large(self):
        path = self.directory / "secret"
        path.write_bytes(b"secret-value\r\n")
        self.assertEqual(Secret(file=path).read(), "secret-value")
        for content in (b" \n", b"x" * 8193):
            path.write_bytes(content)
            with self.assertRaises(ConfigError):
                Secret(file=path).read()

    def test_tracked_configuration_and_state_are_rejected(self):
        with self.assertRaises(ConfigError):
            self.config(self.text.replace('repository = "cache"', 'repository = "."'))
        with self.assertRaises(ConfigError):
            self.config(
                self.text.replace(
                    'database = "state/outbox.sqlite3"',
                    'database = "cache/outbox.sqlite3"',
                )
            )
        with self.assertRaises(ConfigError):
            self.config(
                self.text.replace(
                    'signature_env = "BRIDGE_SECRET"', 'signature_file = "cache/secret"'
                )
            )

    def test_typos_nonpositive_limits_and_ambiguous_secret_sources_fail(self):
        cases = [
            self.text.replace(
                'signature_env = "BRIDGE_SECRET"',
                'signature_env = "BRIDGE_SECRET"\nsignature_file = "secret"',
            ),
            self.text.replace(
                'database = "state/outbox.sqlite3"',
                'database = "state/outbox.sqlite3"\nscan_limit = 0',
            ),
            self.text.replace("signature_env", "signatur_env"),
            self.text.replace(
                'signature_env = "BRIDGE_SECRET"',
                'signature_env = "BRIDGE_SECRET"\nread_token_env = "READ_TOKEN"',
            ),
        ]
        for text in cases:
            with self.subTest(text=text), self.assertRaises(ConfigError):
                self.config(text)


if __name__ == "__main__":
    unittest.main()
