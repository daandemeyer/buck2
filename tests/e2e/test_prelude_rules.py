# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict

import gzip
import io
import json
import tarfile
from pathlib import Path

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test, BuckTestCase, parametrize


async def _output(buck: Buck, target: str, *args: str) -> Path:
    res = await buck.build_without_report(target, "--show-full-simple-output", *args)
    return Path(res.stdout.strip())


async def _values(buck: Buck, package: str) -> dict[str, str]:
    """The `value` attribute of every target in the package, by target name."""
    res = await buck.uquery("--output-attribute", "value", f"root//{package}:")
    return {
        target.split(":")[1]: attrs["value"]
        for target, attrs in json.loads(res.stdout).items()
    }


class PreludeRulesTest(BuckTestCase):
    # prelude/apple/apple_bundle_destination.bzl `bundle_relative_path_for_destination`
    @buck_test(inplace=False)
    async def test_extensionkit_extensions_destination(self, buck: Buck) -> None:
        assert await _values(buck, "bundle") == {
            "iphoneos_app_extensionkit": "Extensions",
            "macosx_app_extensionkit": "Contents/Extensions",
            "maccatalyst_app_extensionkit": "Contents/Extensions",
        }

    # prelude/cxx/headers.bzl `_infer_include_prefix_from_paths`
    @buck_test(inplace=False)
    async def test_include_prefix_needs_a_whole_path_suffix(self, buck: Buck) -> None:
        assert await _values(buck, "headers") == {
            "renamed_header_prefix": "None",
            "subdir_header_prefix": "third-party/foo/config",
        }

    # prelude/cxx/cxx_sources.bzl `get_srcs_with_flags`
    @buck_test(inplace=False)
    async def test_distinct_flag_sets_compile_separately(self, buck: Buck) -> None:
        for target in ("colliding_flag_sets", "quote_colliding_flag_sets"):
            out = await _output(buck, f"root//srcs_flags:{target}")
            assert out.read_text().split() == ["0", "1"], target
        out = await _output(buck, "root//srcs_flags:repeated_flag_set")
        assert out.read_text().split() == ["None"]

    # prelude/sh_binary.bzl `_generate_script`
    @buck_test(inplace=False, skip_for_os=["windows"])
    async def test_two_resources_at_one_link_are_an_error(self, buck: Buck) -> None:
        await expect_failure(
            buck.run("root//sh_resources:show"),
            stderr_regex="would both be placed at `sh_resources/gen`",
        )

    @buck_test(inplace=False, skip_for_os=["windows"])
    async def test_a_resource_at_the_main_link_is_an_error(self, buck: Buck) -> None:
        await expect_failure(
            buck.build("root//sh_resources:resource_at_main_link"),
            stderr_regex="would both be placed at `show.sh`",
        )

    @buck_test(inplace=False, skip_for_os=["windows", "darwin"])
    async def test_archive_extraction_rollout(self, buck: Buck) -> None:
        # Switching the rollout key must select native extraction and then restore the legacy
        # behavior, including each implementation's treatment of literal bracket filenames.
        for enabled in [False, True, False]:
            args = ["-c", "buck2.extract_archives_in_daemon=true"] if enabled else []
            out = await _output(buck, "root//archive:unpacked", *args)
            expected = ["d.js", "i.js", "keep.js"] if enabled else ["[id].js", "keep.js"]
            assert sorted(p.name for p in (out / "pages").iterdir()) == expected


    @buck_test(inplace=False, skip_for_os=["windows", "darwin"])
    @parametrize("daemon_extraction", [False, True])
    async def test_excluding_tar_directory_without_slash(
        self, buck: Buck, daemon_extraction: bool
    ) -> None:
        # A directory listed after its child must exclude that child in either rollout mode.
        # Give the directory a name without a trailing slash and retain an unrelated file.
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as archive:
            root = tarfile.TarInfo("top")
            root.type = tarfile.DIRTYPE
            archive.addfile(root)
            child = tarfile.TarInfo("top/bin/tool")
            child.size = 4
            archive.addfile(child, io.BytesIO(b"data"))
            directory = tarfile.TarInfo("top/bin")
            directory.type = tarfile.DIRTYPE
            offset = archive.offset
            archive.addfile(directory)
            keep = tarfile.TarInfo("top/keep")
            keep.size = 4
            archive.addfile(keep, io.BytesIO(b"keep"))
        # Python adds a slash to directory headers, so rewrite the spelling used by the fixture.
        data = bytearray(buf.getvalue())
        data[offset:offset + 100] = b"top/bin".ljust(100, b"\0")
        data[offset + 148:offset + 156] = b" " * 8
        checksum = sum(data[offset:offset + 512])
        data[offset + 148:offset + 156] = f"{checksum:06o}\0 ".encode()
        (buck.cwd / "archive" / "directory.tar.gz").write_bytes(gzip.compress(data))
        # The selected implementation must omit the excluded directory and its earlier child.
        args = ["-c", "buck2.extract_archives_in_daemon=true"] if daemon_extraction else []
        out = await _output(
            buck, "root//archive:directory_exclusion", *args,
            "-c", "test.directory_archive=directory.tar.gz",
        )
        assert sorted(p.name for p in out.iterdir()) == ["keep"]
