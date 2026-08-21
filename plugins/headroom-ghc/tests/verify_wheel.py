"""Validate a built headroom-ghc-plugin wheel before publication."""

from __future__ import annotations

import email
import re
import sys
import tomllib
import zipfile
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 2:
        raise SystemExit("usage: verify_wheel.py <wheel>")

    wheel = Path(sys.argv[1])
    if not wheel.is_file():
        raise SystemExit(f"wheel not found: {wheel}")

    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        metadata_name = next(name for name in names if name.endswith(".dist-info/METADATA"))
        entry_points_name = next(name for name in names if name.endswith("entry_points.txt"))
        metadata = email.message_from_bytes(archive.read(metadata_name))
        source = archive.read("headroom_ghc_plugin/__init__.py").decode()
        entry_points = archive.read(entry_points_name).decode()

    project_path = Path(__file__).resolve().parents[1] / "pyproject.toml"
    project = tomllib.loads(project_path.read_text(encoding="utf-8"))["project"]
    marker = re.search(r'^PLUGIN_VERSION\s*=\s*"([^"]+)"\s*$', source, re.MULTILINE)
    if marker is None:
        raise AssertionError("PLUGIN_VERSION is missing from the packaged module")

    assert metadata["Name"] == "headroom-ghc-plugin"
    assert metadata["Version"] == project["version"] == marker.group(1)
    assert "headroom.proxy_extension" in entry_points
    requirements = metadata.get_all("Requires-Dist") or []
    assert any("headroom-ai[proxy]" in item for item in requirements), requirements
    assert any(item.startswith("PyYAML") for item in requirements), requirements

    print(f"validated {wheel.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
