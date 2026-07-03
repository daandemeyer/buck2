# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

script = """
import os, sys;
if '--list' in sys.argv:
    print('test1\\n')
sys.exit(int(os.environ.get('EXIT_CODE', '0')))
"""

def _simple_test_impl(ctx):
    out = ctx.actions.declare_output("file", has_content_based_path = False)
    ctx.actions.run(
        ["touch", out.as_output()],
        category = "touch",
    )
    env = {}
    if ctx.attrs.seed:
        env["SEED"] = ctx.attrs.seed
    if ctx.attrs.exit_code:
        env["EXIT_CODE"] = str(ctx.attrs.exit_code)
    return [
        DefaultInfo(out),
        ExternalRunnerTestInfo(
            command = ["fbpython", "-c", script],
            use_project_relative_paths = True,
            type = "lionhead",
            supports_test_execution_caching = ctx.attrs.supports_test_execution_caching,
            env = env,
        ),
    ]

simple_test = rule(
    attrs = {
        "exit_code": attrs.int(default = 0),
        "seed": attrs.string(default = ""),
        "supports_test_execution_caching": attrs.bool(default = False),
    },
    impl = _simple_test_impl,
)
