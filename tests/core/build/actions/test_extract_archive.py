# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict


import io
import os
import tarfile
import zipfile
from pathlib import Path

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test, BuckTestCase

FILES: dict[str, tuple[bytes, int]] = {
    "pkg-1.0/README": (b"hello\n", 0o644),
    "pkg-1.0/bin/tool": (b"#!/bin/sh\n", 0o755),
    "pkg-1.0/data_test.txt": (b"excluded\n", 0o644),
}


def _tar(mode: str) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode=mode) as tar:
        for name in ["pkg-1.0", "pkg-1.0/bin"]:
            info = tarfile.TarInfo(name)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            tar.addfile(info)
        for name, (contents, perms) in FILES.items():
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            info.mode = perms
            tar.addfile(info, io.BytesIO(contents))
        info = tarfile.TarInfo("pkg-1.0/README.md")
        info.type = tarfile.SYMTYPE
        info.linkname = "README"
        tar.addfile(info)
    return buf.getvalue()


def _zip() -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name, (contents, perms) in FILES.items():
            info = zipfile.ZipInfo(name)
            info.external_attr = (0o100000 | perms) << 16
            z.writestr(info, contents)
    return buf.getvalue()


def _write_archives(root: Path) -> None:
    (root / "archive.tar.gz").write_bytes(_tar("w:gz"))
    (root / "archive.bin").write_bytes(_tar("w:gz"))
    (root / "archive.tar.xz").write_bytes(_tar("w:xz"))
    (root / "archive.zip").write_bytes(_zip())

    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        info = tarfile.TarInfo("a/../../escape")
        info.size = 1
        tar.addfile(info, io.BytesIO(b"x"))
    (root / "parent_dir.tar").write_bytes(buf.getvalue())


def _listing(root: Path) -> list[str]:
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(dirnames + filenames):
            path = Path(dirpath) / name
            rel = path.relative_to(root).as_posix()
            if path.is_symlink():
                out.append(f"{rel} -> {os.readlink(path)}")
            elif path.is_dir():
                out.append(f"{rel}/")
            else:
                x = " (x)" if os.access(path, os.X_OK) else ""
                out.append(f"{rel}: {path.read_text().strip()}{x}")
    return sorted(out)


async def _build_listing(buck: Buck, target: str) -> list[str]:
    result = await buck.build(target)
    return _listing(result.get_build_report().output_for_target(target))


class ExtractArchiveTest(BuckTestCase):
    @buck_test(skip_for_os=["windows"])
    async def test_extract_tar(self, buck: Buck) -> None:
        _write_archives(buck.cwd)
        expected = [
            "README.md -> README",
            "README: hello",
            "bin/",
            "bin/tool: #!/bin/sh (x)",
        ]
        assert await _build_listing(buck, "root//:tar_gz") == expected
        expected.append("data_test.txt: excluded")
        assert await _build_listing(buck, "root//:tar_xz") == expected
        assert await _build_listing(buck, "root//:explicit_type") == expected

    @buck_test(skip_for_os=["windows"])
    async def test_extract_zip(self, buck: Buck) -> None:
        _write_archives(buck.cwd)
        assert await _build_listing(buck, "root//:zip") == [
            "README: hello",
            "bin/",
            "bin/tool: #!/bin/sh (x)",
            "data_test.txt: excluded",
        ]

    @buck_test(skip_for_os=["windows"])
    async def test_extract_content_based(self, buck: Buck) -> None:
        _write_archives(buck.cwd)
        result = await buck.build("root//:cat_content_based")
        out = result.get_build_report().output_for_target("root//:cat_content_based")
        assert out.read_text() == "hello\n"
        assert not list((buck.cwd / "buck-out").glob("*/tmp/**/content_based"))

    @buck_test(skip_for_os=["windows"])
    async def test_extract_reruns_when_archive_changes(self, buck: Buck) -> None:
        _write_archives(buck.cwd)
        assert "README: hello" in await _build_listing(buck, "root//:tar_gz")

        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as tar:
            info = tarfile.TarInfo("pkg-1.0/README")
            info.size = 4
            tar.addfile(info, io.BytesIO(b"bye\n"))
        (buck.cwd / "archive.tar.gz").write_bytes(buf.getvalue())

        assert await _build_listing(buck, "root//:tar_gz") == ["README: bye"]

    @buck_test(skip_for_os=["windows"])
    async def test_extract_errors(self, buck: Buck) -> None:
        _write_archives(buck.cwd)
        await expect_failure(
            buck.build("root//:parent_dir"),
            stderr_regex="Archive entry `a/../../escape` has an invalid name",
        )
        await expect_failure(
            buck.build("root//:bad_type"),
            stderr_regex="Unknown archive type `rar`",
        )
