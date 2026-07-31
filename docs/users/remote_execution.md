---
id: remote_execution
title: Remote Execution
---

Buck2 can use services that expose
[Bazel's remote execution API](https://github.com/bazelbuild/remote-apis) in
order to run actions remotely.

Buck2 projects have been successfully tested for remote execution against
[EngFlow](https://www.engflow.com/),
[BuildBarn](https://github.com/buildbarn/bb-remote-execution) and
[BuildBuddy](https://www.buildbuddy.io). Sample project configurations for those
providers are available under
[examples/remote_execution](https://github.com/facebook/buck2/tree/main/examples/remote_execution).

## RE configuration in `.buckconfig`

Configuration for remote execution can be found under `[buck2_re_client]` in
`.buckconfig`.

Keys supported include:

- `engine_address` - address to your RE's engine.
- `action_cache_address` - address to your action cache endpoint.
- `cas_address` - address to your content-addressable storage (CAS) endpoint.
- `tls_ca_certs` - path to a CA certificates bundle. This must be PEM-encoded.
  If none is set, a default bundle will be used. This path contains environment
  variables using shell interpolation syntax (i.e. $VAR). They will be
  substituted before reading the file.
- `tls_client_cert` - path to a client certificate (and intermediate chain), as
  well as its associated private key. This must be PEM-encoded. This path can
  contain environment variables using shell interpolation syntax (i.e. $VAR).
  They will be substituted before reading the file.
- `http_headers` - HTTP headers to inject in all requests to RE. This is a
  comma-separated list of `Header: Value` pairs. Minimal validation of those
  headers is done here. This can contain environment variables using shell
  interpolation syntax ($VAR). They will be substituted before reading the file.
- `instance_name` - an instance name to pass on execution, action cache, and CAS
  requests.

Buck2 uses `SHA256` for all its hashing by default. If your RE engine requires
something else, this can be configured in `.buckconfig` as follows:

```ini
[buck2]
# Accepts BLAKE3, SHA1, or SHA256
digest_algorithms = BLAKE3
```

## Canonical cell source paths (experimental)

Projects that switch a non-root cell between an external Git origin and a local
checkout can opt into stable action-visible source paths:

```ini
[buck2]
cell_execution_paths = canonical_v1
```

The default, `physical`, preserves existing paths and action digests. In
`canonical_v1`, every cell's sources, including the root cell, appear to actions
below a versioned path derived from the cell's execution name, such as
`buck-out/v2/cell_sources/v1/c_73616d706c65/...`. Identical cell contents, commands,
configuration, and execution platform can therefore share full remote action-cache
entries when a cell moves between a standalone project, a nested workspace checkout,
and external storage, provided its execution name stays the same.

### Execution names

By default a cell's execution name is its cell name, which is the label the root
`.buckconfig` mounts it under. Renaming a cell would therefore move every one of its
sources and invalidate its share of the action cache, and two workspaces that vendor
one repository under different names cannot share cache entries for it. The root cell
can pin any cell's execution name to break that coupling:

```ini
[cells]
sample_v2 = third-party/sample

[cell_execution_names]
sample_v2 = sample
```

This is also how to keep cache entries when moving a cell between a Git origin and a
local checkout under a different name: pin both spellings to the same execution name.

Execution names are set only here, in the root cell. Letting each cell declare its own
would mean reading every cell's `.buckconfig` on every command, which for an external
cell also means fetching it. Workspaces that vendor a repository under different names
must therefore agree on the execution name by convention rather than by reading it out
of the cell.

`[cell_execution_names]` cannot be given on the command line. A `-c`/`--config` override
applies to every cell's merged configuration; `[cells]` is banned for the same reason.

Execution names must be unique across cells, and are rejected if empty, longer than 126
UTF-8 bytes, or containing a path separator. A cell given an execution name no longer
answers to its own name: only one spelling of a source reaches actions. The setting is
inert in `physical` mode.

Changing a cell's execution name leaves its previous execution forest and ownership
record in Buck-out. They are inert, but they retain links into the checkout until the
next `buck2 clean`, so a workspace that migrates execution names repeatedly accumulates
one orphaned forest per migration.

The literal on-disk spelling, `c_` followed by the hex-encoded execution name, is not
a stable interface and may change with the layout version. Do not hard-code it in
rules or tooling.

Workspaces sharing an action cache must agree on execution names, as they must on
every other part of the layout. Changing one is a cache migration for that cell.

This option changes action semantics. For selected local and host consumers Buck2
creates a sparse execution forest: each cell root is a real directory and its
declared top-level entries are links to physical source storage. On Windows,
directory entries use junctions and file entries require symlink capability.
The canonical prefix also lengthens every source path by roughly 40 characters
(more for long execution names); on Windows, tools that are not long-path aware, such
as `cl.exe` without a long-path manifest, fail on paths past 260 characters even
when long paths are enabled system-wide. Buck2 logs a warning if even the view's
own directory paths cross that limit, but deep source paths inside a cell can
exceed it without a warning; keep checkouts shallow on Windows.
`--no-buckd` is supported; the client holds the normal daemon lifecycle lock from
before it replaces any existing daemon until its own process exits.

The project-root working directory remains supported. An exact canonical cell root
is supported only by a local-only request. Working directories below a cell root and
relative paths that escape a canonical root are rejected because local kernel path
traversal would differ from remote lexical traversal. A bare structured
`cell_root()`/cell-relative command-line path is also rejected: use a declared
artifact so the input Merkle tree and local forest contain the same member.

The feature normalizes declared paths, not every way a program can discover its
location. Unsandboxed actions that inspect `pwd`, call `realpath`, traverse
undeclared files, or otherwise depend on the physical checkout topology are
cache-unsound under normalized action keys. Audit such actions before enabling the
option, enable it consistently for workspaces sharing an action cache, and expect a
one-time cold-cache boundary when switching modes. The selected mode is recorded in
the daemon startup log for rollout measurement. Debug logging for
`buck2_execute::re::uploader` records source identity, action-visible execution path,
and physical upload path as separate fields when diagnosing canonical inputs. Paths
handed to a detached host
process, such as a process started by `buck2 run`, are not leased: a later daemon
with a different cell topology may rebind the sparse view while that process is
still running.

## RE platform configuration

Next, your build will need an
[execution platform](https://buck2.build/docs/concepts/glossary/#execution-platform)
that specifies how and where actions should be executed. For a sample platform
definition that sets up an execution platform to utilize RE, take a look at the
[EngFlow example](https://github.com/facebook/buck2/blob/main/examples/remote_execution/engflow/platforms/defs.bzl),
[BuildBarn example](https://github.com/facebook/buck2/blob/main/examples/remote_execution/buildbarn/platforms/defs.bzl),
or the
[BuildBuddy example](https://github.com/facebook/buck2/blob/main/examples/remote_execution/buildbuddy/platforms/defs.bzl).

To enable remote execution, configure the following fields in
[CommandExecutorConfig](https://buck2.build/docs/api/build/globals/#commandexecutorconfig)
as follows:

- `remote_enabled` - set to `True`.
- `local_enabled` - set to `True` if you also want to run actions locally.
- `use_limited_hybrid` - set to `False` unless you want to exclusively run
  remotely when possible.
- `remote_execution_properties` - other additional properties.
  - If the RE engine requires a container image, this can be done by setting
    `container-image` to an image URL, as is done in the example above.
