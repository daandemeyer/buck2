---
id: download_cache
title: Download Cache
---

`download_file` (and therefore `http_file` and `http_archive`) writes its output
into `buck-out`, which is per project. Building the same dependency in a second
checkout, or after `buck2 clean`, downloads it again.

The download cache is an opt-in store of downloaded bytes that lives outside any
`buck-out` and is shared by every project on the machine. Entries are keyed by
the checksum the action declares, and their bytes are hashed both on the way in
and on the way out, so nothing is ever handed to a build without having been
checked against that checksum first.

To enable, add this to your Buckconfig:

```ini
[buck2]
download_cache_enabled = true
```

The store defaults to `<user cache dir>/buck2/downloads`: `$XDG_CACHE_HOME`, or
`~/.cache`, on Linux; `~/Library/Caches` on macOS; `%LOCALAPPDATA%` on Windows.
It can be moved:

```ini
[buck2]
download_cache_dir = ~/somewhere/else
```

The path must be absolute and normalized (no `..` components); a leading `~` is
expanded, environment variables are not. Inside it, entries live under a
format-version directory, so the store you will actually see is
`<dir>/v1/<algorithm>/<xx>/<checksum>`.

Changing any setting on this page restarts the daemon on the next command, so
there is no need to kill it by hand.

## What it does and does not cover

- Only downloads with a declared `sha1` or `sha256` are cached, which is every
  download `http_archive` and `http_file` make, since `download_file` requires
  a checksum.
- Nothing else is shared. Archive extraction, `git_fetch` and ordinary action
  outputs are unaffected; those are what a remote cache is for.
- The store is per user. Pointing several users at one directory is not
  supported: they will not be able to write each other's entries, and only the
  first such failure is logged, at warning level in the daemon log. Other
  symptoms of a store a daemon cannot write to, such as being unable to refresh
  an entry's last-used time, are only visible at debug level.
- The store is not an offline cache. `download_file` still makes a HEAD request
  to determine an artifact's size unless `size_bytes` is declared or the
  behavior below is enabled.

## Skipping the HEAD request

By default buck2 issues a HEAD request per `download_file` whose `size_bytes` is
not declared, in order to learn the size before deferring the download. That
request happens even when the bytes are already in the store, so a build with
many downloads still talks to the network on every daemon restart.

```ini
[buck2]
download_cache_skip_head_request = true
```

makes a store hit supply the size instead. Buck2 hashes the stored entry to
establish that size, rather than trusting its length, so a damaged entry is
discarded and re-downloaded rather than corrupting the build.

Two trade-offs come with that. Buck2 stops noticing, at that point in the build,
that a URL has gone away or changed: it is trusting that the checksum still
describes what is at the URL. And establishing the size means reading the whole
entry, which is on top of the read that materializing it already costs, so this
is a clear win for many small downloads and a loss for a few very large ones.

## Garbage collection

Entries record the time they were last used. A daemon sweeps the store when it
starts and periodically after that, deleting entries that haven't been used in
14 days. The sweep runs at least daily, and more often for shorter retentions,
and daemons rate limit each other so the work is not repeated. To change the
retention, or to stop deleting entries entirely:

```ini
[buck2]
# 30 days
download_cache_gc_max_age_secs = 2592000
```

```ini
[buck2]
# keep entries forever
download_cache_gc_max_age_secs = 0
```

There is no size limit, only an age limit.

Retention is configured per project but the store is machine-wide, so the
shortest retention among the projects on a machine is the one that effectively
applies: a daemon sweeping with a one-hour retention deletes entries the other
projects would have kept for two weeks. Keep this setting the same across
projects that share a store.

The store is always safe to delete by hand: every entry is content-addressed and
can be downloaded again. When a buck2 upgrade changes the on-disk format, the
sweep also removes the superseded `v<n>` directory, so an older buck2 sharing a
store with a newer one will keep having its entries discarded.

## Checking that it works

`buck2 log what-materialized` reports a download served from the store as
`download-cache` rather than `http`.

A download only appears there at all if it was deferred to the materializer,
which needs the declared checksum to use the same algorithm as the daemon's
digest. When it doesn't, `download_file` downloads inline and the store is still
consulted, but no materialization is recorded.
