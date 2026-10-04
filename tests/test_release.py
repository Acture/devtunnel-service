import re
import unittest
from pathlib import Path

import devtunnel_service

CHANGELOG: Path = Path(__file__).resolve().parents[1] / "packaging/debian/changelog"


class ReleaseTests(unittest.TestCase):
    @unittest.skipUnless(CHANGELOG.is_file(), "packaging/ is not part of the sdist")
    def test_debian_package_has_the_python_version(self) -> None:
        first = CHANGELOG.read_text().splitlines()[0]
        match = re.fullmatch(
            r"devtunnel-service \(([^()-]+)-[^()]+\) \S+; urgency=\w+", first
        )
        self.assertIsNotNone(match, first)
        assert match is not None
        self.assertEqual(match[1], devtunnel_service.__version__)
