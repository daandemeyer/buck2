# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict


import asyncio
import os
import shutil
import tempfile
from pathlib import Path

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test
from buck2.tests.e2e_util.helper.utils import expect_exec_count


def setup_symlink(symlink_path: Path, target: Path) -> None:
    symlink_path.parent.mkdir(parents=True, exist_ok=True)

    if not os.path.islink(symlink_path) and os.path.isdir(symlink_path):
        shutil.rmtree(symlink_path)
    else:
        symlink_path.unlink(missing_ok=True)

    os.symlink(target, symlink_path)


# Digest computation used to hang forever on the symlinks below, so bound the builds instead of
# stalling the suite.
SYMLINK_BUILD_TIMEOUT_S = 200


@buck_test(extra_buck_config={"buck2": {"use_correct_source_symlink_reading": "true"}})
async def test_symlink_target_tracked_for_rebuild(buck: Buck) -> None:
    setup_symlink(buck.cwd / "src" / "link", Path("../dir"))

    await buck.build("//:cp")
    await expect_exec_count(buck, 1)

    await buck.build("//:cp")
    await expect_exec_count(buck, 0)

    with open(buck.cwd / "dir/file", "w") as file:
        file.write("GOODBYE\n")

    # This isn't really behavior  we want to guarantee and we'd rather users
    # don't use symlinks, but this is very observable (and it's not worse than
    # just reading the files then pretending they are never used!)
    await buck.build("//:cp")
    await expect_exec_count(buck, 1)


@buck_test(
    setup_eden=True,
    extra_buck_config={"buck2": {"use_correct_source_symlink_reading": "true"}},
)
async def test_symlinks_redirection(buck: Buck) -> None:
    setup_symlink(buck.cwd / "src" / "link", Path("../dir"))

    await buck.build("//:cp")
    await expect_exec_count(buck, 1)

    await buck.build("//:cp")
    await expect_exec_count(buck, 0)

    # We change the symlink which should invalidate all files depending on it
    setup_symlink(buck.cwd / "src" / "link", Path("../dir2"))

    await buck.build("//:cp")
    await expect_exec_count(buck, 1)


@buck_test(
    setup_eden=True,
    extra_buck_config={"buck2": {"use_correct_source_symlink_reading": "true"}},
)
async def test_symlinks_external(buck: Buck) -> None:
    top_level = Path(tempfile.mkdtemp())

    (top_level / "nested1").mkdir()
    (top_level / "nested2").mkdir()
    (top_level / "nested1" / "file").write_text("HELLO")
    (top_level / "nested2" / "file").write_text("GOODBYE")

    setup_symlink(buck.cwd / "ext" / "link", top_level / "nested1")

    await buck.build("//:ext")
    await expect_exec_count(buck, 1)

    await buck.build("//:ext")
    await expect_exec_count(buck, 0)

    setup_symlink(buck.cwd / "ext" / "link", top_level / "nested2")

    await buck.build("//:ext")
    await expect_exec_count(buck, 1)


@buck_test(extra_buck_config={"buck2": {"use_correct_source_symlink_reading": "true"}})
async def test_no_read_through_symlinks(buck: Buck) -> None:
    res = await buck.build_without_report(
        "//:stat_symlink",
        "--out",
        "-",
        "--remote-only",
    )
    # Just check that we don't always return `True`
    assert res.stdout.strip() == "False"

    setup_symlink(buck.cwd / "src" / "link", Path("..") / "dir")

    res = await buck.build_without_report(
        "//:stat_symlink",
        "--out",
        "-",
        "--remote-only",
    )
    assert res.stdout.strip() == "True"

    res = await buck.build_without_report(
        "//:stat_symlink_in_dir",
        "--out",
        "-",
        "--remote-only",
    )
    assert res.stdout.strip() == "True"


@buck_test(extra_buck_config={"buck2": {"use_correct_source_symlink_reading": "true"}})
async def test_no_read_through_source_symlinks_to_file(buck: Buck) -> None:
    res = await buck.build_without_report(
        "//:stat_symlink",
        "--out",
        "-",
        "--remote-only",
    )
    # Just check that we don't always return `True`
    assert res.stdout.strip() == "False"

    setup_symlink(
        buck.cwd / "src" / "link",
        Path("..") / "dir" / "file",
    )

    res = await buck.build_without_report(
        "//:stat_symlink",
        "--out",
        "-",
        "--remote-only",
    )
    assert res.stdout.strip() == "True"


@buck_test(extra_buck_config={"buck2": {"use_correct_source_symlink_reading": "true"}})
async def test_no_read_through_source_symlinks_to_in_symlink_target(buck: Buck) -> None:
    for s in ("dir", "dir2/dir"):
        (buck.cwd / s).mkdir(parents=True, exist_ok=True)
        (buck.cwd / s / "file").write_text(s)
    setup_symlink(buck.cwd / "redirectvia", Path("dir2") / "dir")

    setup_symlink(
        buck.cwd / "src" / "link",
        Path("..") / "redirectvia" / ".." / "dir" / "file",
    )

    res = await buck.build_without_report(
        "//:cp_src_link_via_builtin",
        "--out",
        "-",
    )
    # FIXME(JakobDegen): Should be `dir2/dir`. The fact that `redirectvia`, found in the symlink
    # target, is itself a symlink is completely ignored
    assert res.stdout.strip() == "dir"


@buck_test(setup_eden=True)
async def test_eden_io_read_symlink_dir_build_target(buck: Buck) -> None:
    setup_symlink(buck.cwd / "testlink", buck.cwd / "symdir" / "dir")

    await buck.build("//:symlink_dep")


@buck_test(setup_eden=True)
async def test_eden_io_read_symlink_dir_list_target(buck: Buck) -> None:
    setup_symlink(buck.cwd / "testlink", buck.cwd / "symdir")

    await buck.targets("//testlink/dir:")


@buck_test()
async def test_source_dir_symlinks_to_ancestors(buck: Buck) -> None:
    setup_symlink(buck.cwd / "ancestors" / "sub" / "self", Path("."))
    setup_symlink(buck.cwd / "ancestors" / "sub" / "deeper" / "up", Path(".."))

    result = await asyncio.wait_for(
        buck.build("//:ancestors"), timeout=SYMLINK_BUILD_TIMEOUT_S
    )
    await expect_exec_count(buck, 1)

    out = result.get_build_report().output_for_target("root//:ancestors")
    assert (out / "sub" / "deeper" / "file").read_text() == "deeper\n"
    # Copying re-points symlinks at their original targets, so both still lead to the source dir.
    source = (buck.cwd / "ancestors" / "sub").resolve()
    for link in (out / "sub" / "self", out / "sub" / "deeper" / "up"):
        assert os.path.islink(link)
        assert link.resolve() == source

    await asyncio.wait_for(buck.build("//:ancestors"), timeout=SYMLINK_BUILD_TIMEOUT_S)
    await expect_exec_count(buck, 0)


@buck_test(skip_for_os=["windows"])
async def test_copy_dir_relative_symlinks_are_relocatable(buck: Buck) -> None:
    source = buck.cwd / "relocatable"
    setup_symlink(source / "dir" / "link", Path("real"))
    setup_symlink(source / "sub" / "up", Path(".."))

    relative_result = await asyncio.wait_for(
        buck.build("//:relocatable_copy"), timeout=SYMLINK_BUILD_TIMEOUT_S
    )
    relative_output = relative_result.get_build_report().output_for_target(
        "root//:relocatable_copy"
    )

    assert (relative_output / "dir" / "link").readlink() == Path("real")
    assert (relative_output / "sub" / "up").readlink() == Path("..")
    assert (relative_output / "dir" / "link").read_text() == "real\n"
    assert (relative_output / "sub" / "up" / "dir" / "real").read_text() == "real\n"

    relocated = buck.cwd / "relocated"
    shutil.copytree(relative_output, relocated, symlinks=True)
    assert (relocated / "dir" / "link").readlink() == Path("real")
    assert (relocated / "sub" / "up").readlink() == Path("..")
    assert (relocated / "dir" / "link").read_text() == "real\n"
    assert (relocated / "sub" / "up" / "dir" / "real").read_text() == "real\n"

    default_result = await asyncio.wait_for(
        buck.build("//:default_copy"), timeout=SYMLINK_BUILD_TIMEOUT_S
    )
    default_output = default_result.get_build_report().output_for_target(
        "root//:default_copy"
    )
    default_file_link = default_output / "dir" / "link"
    default_up_link = default_output / "sub" / "up"

    assert default_file_link.readlink() != Path("real")
    assert default_file_link.readlink().parts[0] == ".."
    assert default_file_link.resolve() == (source / "dir" / "real").resolve()
    assert default_up_link.readlink() != Path("..")
    assert default_up_link.readlink().parts[0] == ".."
    assert default_up_link.resolve() == source.resolve()


@buck_test()
async def test_source_dir_symlink_cycle(buck: Buck) -> None:
    setup_symlink(buck.cwd / "cycle" / "a" / "link", Path("..") / "b")
    setup_symlink(buck.cwd / "cycle" / "b" / "link", Path("..") / "a")

    failure = await asyncio.wait_for(
        expect_failure(buck.build("//:cycle"), stderr_regex="Symlink cycle detected"),
        timeout=SYMLINK_BUILD_TIMEOUT_S,
    )
    assert "root//cycle/a ->" in failure.stderr
    assert "root//cycle/b ->" in failure.stderr


@buck_test()
async def test_source_symlink_loop(buck: Buck) -> None:
    setup_symlink(buck.cwd / "loop" / "a" / "l1", Path("..") / "b" / "l2")
    setup_symlink(buck.cwd / "loop" / "b" / "l2", Path("..") / "a" / "l1")

    failure = await asyncio.wait_for(
        expect_failure(buck.build("//:loop"), stderr_regex="Symlink cycle detected"),
        timeout=SYMLINK_BUILD_TIMEOUT_S,
    )
    assert "root//loop/a/l1 ->" in failure.stderr
    assert "root//loop/b/l2 ->" in failure.stderr


@buck_test()
async def test_source_symlink_chain_that_grows(buck: Buck) -> None:
    # Resolving `a/l` goes `deep/er/path` -> `a/l/path` -> `deep/er/path/path` -> ... forever.
    setup_symlink(buck.cwd / "grow" / "a" / "l", Path("..") / "deep" / "er" / "path")
    setup_symlink(buck.cwd / "grow" / "deep" / "er", Path("..") / "a" / "l")

    await asyncio.wait_for(
        expect_failure(
            buck.build("//:grow"),
            stderr_regex="Too many levels of symlinks while reading `root//grow/a/l`",
        ),
        timeout=SYMLINK_BUILD_TIMEOUT_S,
    )
