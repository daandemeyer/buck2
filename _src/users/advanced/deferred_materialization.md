---
id: deferred_materialization
title: Deferred Materialization
---

import { OssOnly } from 'docusaurus-plugin-internaldocs-fb/internal';

When using [Remote Execution](../remote_execution.md), Buck2 operates with
Deferred Materialization, which means that Buck2 will avoid downloading outputs
until they are required by a local action.

This can provide very substantial performance savings on builds that execute
primarily on Remote Execution, since those builds become able to proceed without
ever downloading any intermediary outputs.

At Meta, despite very fast networks being used internally, this was observed
to make real-world builds finish approximately 2.5 times faster.

## Pitfalls

Buck2's deferred materialization makes assumptions about your Remote Execution
backend. In particular, it expects that the TTL returned from action cache
entries by your Remote Execution backend always exceeds the TTL of all output
artifacts it references.

Nonetheless, artifacts may also eventually expire from your Remote Execution
backend. When that happens, builds using Deferred Materialization may fail if
those artifacts are needed locally.

A kill is necessary to recover from those builds. However, the
[Restarter](restarter.md) can be used to mitigate this issue by restarting Buck2
daemon when it encounters an expired artifact.

<OssOnly>
At Meta, artifacts get periodically refreshed, but open source RE backends do not expose the TTL of artifacts, so this feature does not work outside of Meta.
</OssOnly>

## On-disk state

Buck2 can also optionally track its state on disk in a SQLite database. This
allows Buck2 to remember what files are on disk across restarts.

This can allow Buck2 to avoid re-downloading outputs from your Remote Execution
backend if they are already on disk.

To enable, add this to your Buckconfig:

```ini
[buck2]
sqlite_materializer_state = true
```

## Deferring Write Actions

To further speedup builds, Buck2 can also be instructed to not execute any
writes on the critical path for a build.

To enable, add this to your Buckconfig:

```ini
[buck2]
defer_write_actions = true
```

This mechanism is recommended if you're using the On-disk State, since it means
Buck can omit writes entirely if the same content is already on disk.

## `buck2 clean --stale`

The deferred materializer can be configured to continuously delete stale
artifacts, that haven't been recently accessed, or untracked artifacts, that
exist in buck-out but not in the materalizer state.

Unlike `buck2 clean` this does not fully wipe buck-out but it should not
negatively impact build performance if you are building and rebasing regularly.

Enabling this requires enabling [on-disk state](#on-disk-state) and
[deferred write actions](#deferring-write-actions), and adding this to your
Buckconfig:

```ini
[buck2]
clean_stale_enabled = true
```

It can be further configured by changing these default values:

```ini
[buck2]
# one week
clean_stale_artifact_ttl_hours = 24 * 7
clean_stale_period_hours = 24
clean_stale_start_offset_hours = 12
```

- `clean_stale_start_offset_hours` determines the time following daemon start up
  before the first clean will be scheduled.
- `clean_stale_period_hours` determines how frequently to schedule recurring
  clean events.
- `clean_stale_artifact_ttl_hours` determines how long artifacts should be kept
  in buck-out before cleaning them.
- `clean_stale_dry_run` (default false) reports what would be cleaned without
  deleting it.
- `clean_stale_low_disk_threshold` (percent of total disk free, e.g. `10.0`)
  enables more aggressive cleaning when free disk drops at or below it. The
  low-disk behavior below never engages unless this is set.
- `clean_stale_low_disk_artifact_ttl_hours` (default 48) sets a shorter fixed
  TTL to use while free disk is at or below the threshold.
- `clean_stale_low_disk_adaptive_enabled` (default false) replaces the fixed
  shorter TTL with adaptive cleaning: buck2 keeps promoting the oldest retained
  artifacts to stale until projected free disk rises back above the threshold,
  while protecting any artifact younger than
  `clean_stale_low_disk_adaptive_min_ttl_hours` (default 12).
- `clean_stale_low_disk_adaptive_delete_intermediate_within_min_ttl` (default
  false) allows adaptive cleaning to delete non-active artifacts marked as
  intermediate-only even when they are below the adaptive minimum TTL.
- `clean_stale_low_disk_adaptive_unmaterialize_active` (default false) adds a
  final adaptive escalation step that discards active intermediate artifacts
  backed by CAS or HTTP downloads. Their materializer entries return to the
  declared state, so a later build downloads them again without rerunning the
  producing action.
- `clean_stale_low_disk_unmaterialization_threshold` (defaults to
  `clean_stale_low_disk_threshold`) sets the free-disk percentage that active
  unmaterialization recovers to. It must not exceed
  `clean_stale_low_disk_threshold` and only applies when
  `clean_stale_low_disk_adaptive_unmaterialize_active` is enabled. Active
  unmaterialization is also suppressed unless `ttl_refresh_enabled` is enabled
  for the daemon, because CAS blobs must remain available for future
  rematerialization.

If clean stale is running in the background at the same time that a build begins
to materialize artifacts, the clean will be interrupted and not run again until
after the next scheduled period, but it should be able to make gradual progress
and prevent long term accumulation of artifacts.

If needed, a clean can be manually triggered by calling `buck2 clean --stale`.
With no cleanup-policy flags, the command uses the configured artifact TTL,
dry-run setting, and fixed or adaptive low-disk policy. These settings apply to
manual cleanup even when `clean_stale_enabled` is false; that setting only
controls periodic scheduling.

An explicit duration or adaptive low-disk flag selects an explicit command-line
policy instead. The equivalent explicit manual escalation is
`--adaptive-unmaterialize-active`, which requires
`--adaptive-low-disk-threshold` and the same TTL-refresh support.

### Clean before the first command

With on-disk state and deferred write actions enabled, you can make Buck2 check
free disk space before the first build, query, or other command that loads
project state:

```ini
[buck2]
clean_stale_on_startup = true
clean_stale_enabled = false
clean_stale_low_disk_threshold = 10.0
clean_stale_low_disk_adaptive_enabled = true
```

When free disk space is at or below 10%, the first command runs the configured
cleanup policy and waits for it to finish before starting its work. Commands
that start while the cleanup runs wait for it as well, and they don't interrupt
it. Above the threshold, Buck2 skips the cleanup scan.

`buck2 clean --stale` doesn't start the cleanup, so `--dry-run` and the explicit
policy flags of that command decide what gets deleted. `buck2 lsp`,
`buck2 subscribe`, `buck2 debug trace-io` and `buck2 debug file-status` neither
start the cleanup nor wait for it.

`--no-buckd` performs this check on every invocation. A resident daemon performs
it at most once per project and isolation directory during its lifetime.

`clean_stale_on_startup` defaults to false and works independently of periodic
cleanup. Without `clean_stale_low_disk_threshold`, startup cleanup uses the
configured artifact TTL on every startup. `clean_stale_dry_run` also applies.
If the cleanup fails, Buck2 prints a warning and the command continues. The
daemon doesn't retry a failed startup cleanup.
