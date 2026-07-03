/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! A store of `download_file` bytes, keyed by the checksum declared by the
//! action and shared between projects on the same machine.
//!
//! Every store operation is best effort: a store that is missing, corrupt or
//! unwritable degrades to a plain download, never to a build failure. A
//! directory that was explicitly configured and cannot be used is the one
//! exception, and is reported at startup rather than silently ignored. Bytes
//! are hashed on the way in and on the way out, so nothing the store hands
//! back, including an entry's size, is ever taken on trust.
//!
//! The store is per user. Pointing several users at one directory is not
//! supported.

use std::io;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use allocative::Allocative;
use buck2_common::cas_digest::CasDigestConfig;
use buck2_common::cas_digest::DigestAlgorithmFamily;
use buck2_common::file_ops::metadata::FileDigest;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_common::init::DownloadCacheStartupConfig;
use buck2_error::BuckErrorContext;
use buck2_fs::error::IoError;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use dupe::Dupe;

use crate::digest_config::DigestConfig;
use crate::execute::blocking::BlockingExecutor;
use crate::materialize::http::Checksum;
use crate::materialize::http::ChecksumVerifier;

/// Bumped when the on-disk layout changes in a way older daemons can't read.
const FORMAT_VERSION: &str = "v1";

const TMP_DIR: &str = "tmp";
const GC_STAMP: &str = "gc.stamp";

const COPY_BUF_SIZE: usize = 64 * 1024;

const DEFAULT_GC_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

const ONE_HOUR: Duration = Duration::from_secs(60 * 60);
const ONE_DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Tmp files this old were leaked by a daemon that died mid-write.
const TMP_MAX_AGE: Duration = ONE_DAY;

/// Upper bound on how stale an entry's mtime may get before a hit rewrites it,
/// so that reads don't turn into writes on every build.
const MAX_TOUCH_INTERVAL: Duration = ONE_DAY;

#[derive(Debug, Clone, Copy, Dupe, PartialEq, Eq, Allocative)]
enum KeyAlgorithm {
    Sha256,
    Sha1,
}

impl KeyAlgorithm {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha1 => "sha1",
        }
    }

    fn hex_len(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha1 => 40,
        }
    }

    fn family(self) -> DigestAlgorithmFamily {
        match self {
            Self::Sha256 => DigestAlgorithmFamily::Sha256,
            Self::Sha1 => DigestAlgorithmFamily::Sha1,
        }
    }
}

/// The declared checksums usable as store keys, strongest first.
fn keys(checksum: &Checksum) -> Vec<(KeyAlgorithm, &str)> {
    let mut keys = Vec::with_capacity(2);
    if let Some(sha256) = checksum.sha256() {
        keys.push((KeyAlgorithm::Sha256, sha256));
    }
    if let Some(sha1) = checksum.sha1() {
        keys.push((KeyAlgorithm::Sha1, sha1));
    }
    keys.retain(|(algo, hex)| is_valid_key(*algo, hex));
    keys
}

/// `Checksum` validates on construction; this is what keeps a malformed one
/// from being interpolated into a path.
fn is_valid_key(algo: KeyAlgorithm, hex: &str) -> bool {
    hex.len() == algo.hex_len() && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct DownloadCacheConfig {
    dir: AbsNormPathBuf,
    /// `None` disables collection of entries. Leaked tmp files are always
    /// collected.
    gc_max_age: Option<Duration>,
    skip_head_request: bool,
}

impl DownloadCacheConfig {
    /// `Ok(None)` means the store is disabled, either because it wasn't asked
    /// for or because a default location couldn't be worked out. Only an
    /// explicitly configured directory that can't be used is an error.
    pub fn new(config: &DownloadCacheStartupConfig) -> buck2_error::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }

        let dir = match config.dir.as_deref().map(str::trim) {
            Some(dir) if !dir.is_empty() => parse_dir(dir)?,
            _ => match default_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    tracing::warn!(
                        "`buck2.download_cache_enabled` is set, but the store is disabled: {e:#}"
                    );
                    return Ok(None);
                }
            },
        };

        let gc_max_age_secs = config
            .gc_max_age_secs
            .unwrap_or(DEFAULT_GC_MAX_AGE.as_secs());

        Ok(Some(Self {
            dir,
            gc_max_age: (gc_max_age_secs > 0).then(|| Duration::from_secs(gc_max_age_secs)),
            skip_head_request: config.skip_head_request,
        }))
    }
}

fn parse_dir(dir: &str) -> buck2_error::Result<AbsNormPathBuf> {
    let home_relative = dir
        .strip_prefix("~/")
        .or_else(|| dir.strip_prefix("~\\").filter(|_| cfg!(windows)))
        .or_else(|| (dir == "~").then_some(""));

    let path = match home_relative {
        Some(rest) => {
            let home = dirs::home_dir().ok_or_else(|| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "`buck2.download_cache_dir` starts with `~`, but there is no home directory"
                )
            })?;
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        }
        None => PathBuf::from(dir),
    };

    AbsNormPathBuf::new(path).buck_error_context(
        "`buck2.download_cache_dir` must be an absolute, normalized path (`~/` is expanded, \
         environment variables are not)",
    )
}

fn default_dir() -> buck2_error::Result<AbsNormPathBuf> {
    let cache_dir = dirs::cache_dir().ok_or_else(|| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "no user cache directory was found; set `buck2.download_cache_dir` explicitly"
        )
    })?;
    AbsNormPathBuf::new(cache_dir.join("buck2").join("downloads")).buck_error_context(
        "the user cache directory is not an absolute, normalized path; set \
         `buck2.download_cache_dir` explicitly",
    )
}

#[derive(Debug, Allocative)]
pub struct DownloadCache {
    /// The versioned root, i.e. `<configured dir>/v1`.
    root: AbsNormPathBuf,
    gc_max_age: Option<Duration>,
    skip_head_request: bool,
    #[allocative(skip)]
    tmp_counter: AtomicU64,
    /// Distinguishes daemons that share a store but not a pid namespace.
    nonce: u64,
    /// A store that can never be written to would otherwise fail silently on
    /// every single build.
    #[allocative(skip)]
    reported_write_failure: AtomicBool,
}

impl DownloadCache {
    /// Does no IO: a store that can't be created is only discovered, and
    /// tolerated, the first time it is used.
    pub fn new(config: DownloadCacheConfig) -> Arc<Self> {
        Arc::new(Self {
            root: config
                .dir
                .join(ForwardRelativePath::unchecked_new(FORMAT_VERSION)),
            gc_max_age: config.gc_max_age,
            skip_head_request: config.skip_head_request,
            tmp_counter: AtomicU64::new(0),
            nonce: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64),
            reported_write_failure: AtomicBool::new(false),
        })
    }

    /// Whether a store hit may stand in for the HEAD request `download_file`
    /// otherwise issues to discover an artifact's size.
    pub fn skip_head_request(&self) -> bool {
        self.skip_head_request
    }

    fn entry_path(&self, algo: KeyAlgorithm, hex: &str) -> Option<AbsNormPathBuf> {
        if !is_valid_key(algo, hex) {
            return None;
        }
        let relative = format!("{}/{}/{}", algo.dir_name(), &hex[..2], hex);
        Some(
            self.root
                .join(ForwardRelativePath::unchecked_new(&relative)),
        )
    }

    fn entry_paths(&self, checksum: &Checksum) -> Vec<(KeyAlgorithm, AbsNormPathBuf)> {
        keys(checksum)
            .into_iter()
            .filter_map(|(algo, hex)| Some((algo, self.entry_path(algo, hex)?)))
            .collect()
    }

    /// An entry is only useful for as long as it is younger than the retention
    /// of whichever daemon sweeps the store, so its mtime has to be refreshed
    /// more often than that. A daemon that collects nothing itself still
    /// refreshes, because some other daemon sharing the store may collect.
    fn touch_interval(&self) -> Duration {
        self.gc_max_age
            .map_or(MAX_TOUCH_INTERVAL, |max_age| max_age / 4)
            .min(MAX_TOUCH_INTERVAL)
    }

    /// The size of a stored entry, after checking its bytes against the key it
    /// is filed under. Unlike a HEAD request this cannot be wrong, which
    /// matters because the caller commits this size to the artifact's digest.
    pub async fn verified_size(
        &self,
        checksum: &Checksum,
        io: &dyn BlockingExecutor,
    ) -> Option<u64> {
        let candidates = self.entry_paths(checksum);
        if candidates.is_empty() {
            return None;
        }
        let touch_interval = self.touch_interval();

        let result = io
            .execute_io_inline(|| {
                for (algo, entry) in &candidates {
                    match read_verified(entry, *algo, checksum, None, None, None) {
                        Ok(Some(read)) => {
                            touch(entry, touch_interval);
                            return Ok(Some(read.size));
                        }
                        Ok(None) => {}
                        Err(e) => report(e, entry),
                    }
                }
                Ok(None)
            })
            .await;

        result.unwrap_or_else(|e| {
            tracing::debug!("download cache: size lookup failed: {e:#}");
            None
        })
    }

    /// Copy a stored entry to `dest`, verifying it against every declared
    /// checksum on the way. Returns the digest of the copied file, or `None` if
    /// the store could not satisfy the request.
    pub async fn get(
        &self,
        checksum: &Checksum,
        expected_size: Option<u64>,
        dest: &AbsNormPath,
        is_executable: bool,
        digest_config: DigestConfig,
        io: &dyn BlockingExecutor,
    ) -> Option<TrackedFileDigest> {
        let candidates = self.entry_paths(checksum);
        if candidates.is_empty() {
            return None;
        }
        let cas_digest_config = digest_config.cas_digest_config();
        let touch_interval = self.touch_interval();

        let result = io
            .execute_io_inline(|| {
                for (algo, entry) in &candidates {
                    let mut wrote_dest = false;
                    let read = read_verified(
                        entry,
                        *algo,
                        checksum,
                        expected_size,
                        Some(cas_digest_config),
                        Some((dest, &mut wrote_dest)),
                    );
                    match read {
                        Ok(Some(read)) => {
                            if is_executable && let Err(e) = fs_util::set_executable(dest, true) {
                                tracing::debug!("download cache: chmod of {dest} failed: {e:?}");
                                let _ignored = fs_util::remove_file(dest);
                                return Ok(None);
                            }
                            touch(entry, touch_interval);
                            return Ok(read.digest);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            if wrote_dest {
                                let _ignored = fs_util::remove_file(dest);
                            }
                            report(e, entry);
                        }
                    }
                }
                Ok(None)
            })
            .await;

        match result {
            Ok(digest) => digest.map(|digest| TrackedFileDigest::new(digest, cas_digest_config)),
            Err(e) => {
                tracing::debug!("download cache: read failed: {e:#}");
                None
            }
        }
    }

    /// Store `src` under every declared checksum, replacing whatever was there.
    /// The bytes are re-hashed on the way in, so a `src` that changed since the
    /// caller verified it is dropped rather than stored under a key it doesn't
    /// match.
    pub async fn put(&self, checksum: &Checksum, src: &AbsNormPath, io: &dyn BlockingExecutor) {
        let entries = self.entry_paths(checksum);
        if entries.is_empty() {
            return;
        }

        let tmp_dir = self.root.join(ForwardRelativePath::unchecked_new(TMP_DIR));
        let work: Vec<(KeyAlgorithm, AbsNormPathBuf, AbsNormPathBuf)> = entries
            .into_iter()
            .map(|(algo, entry)| (algo, self.tmp_path(), entry))
            .collect();

        let result = io
            .execute_io_inline(|| {
                fs_util::create_dir_all(&tmp_dir)?;
                // Later keys are aliases of the first one; hardlinking them keeps
                // a download declaring both a sha1 and a sha256 to one copy on
                // disk.
                let mut stored: Option<&AbsNormPathBuf> = None;
                for (algo, tmp, entry) in &work {
                    let linked = match stored {
                        Some(from) => link_entry(from, tmp, entry).is_ok(),
                        None => false,
                    };
                    if !linked && !insert(src, *algo, checksum, tmp, entry)? {
                        // The source didn't match the key it was to be filed
                        // under, so there is nothing for the other keys to
                        // alias either.
                        return Ok(());
                    }
                    if stored.is_none() {
                        stored = Some(entry);
                    }
                }
                Ok(())
            })
            .await;

        let failure = match result {
            Ok(()) => return,
            Err(e) => e,
        };
        if !self.reported_write_failure.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "download cache: could not write to {}, downloads will not be shared: {failure:#}",
                self.root
            );
        } else {
            tracing::debug!("download cache: insert failed: {failure:#}");
        }
    }

    fn tmp_path(&self) -> AbsNormPathBuf {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}/{}-{}-{}", TMP_DIR, std::process::id(), self.nonce, n);
        self.root.join(ForwardRelativePath::unchecked_new(&name))
    }

    fn gc_interval(&self) -> Duration {
        match self.gc_max_age {
            Some(max_age) => (max_age / 14).clamp(ONE_HOUR, ONE_DAY),
            None => ONE_DAY,
        }
    }

    /// Periodically delete entries that haven't been used within the configured
    /// max age, and tmp files leaked by daemons that died mid-write. Rate
    /// limited across daemons by the mtime of a stamp file, and safe to run
    /// concurrently with builds and with itself.
    pub fn spawn_gc(self: &Arc<Self>) {
        self.spawn_gc_every(self.gc_interval());
    }

    fn spawn_gc_every(self: &Arc<Self>, interval: Duration) {
        let this = self.dupe();
        tokio::spawn(async move {
            loop {
                let gc = this.dupe();
                let run = tokio::task::spawn_blocking(move || gc.run_gc(SystemTime::now())).await;
                match run {
                    Ok(Err(e)) => tracing::debug!("download cache: gc failed: {e:#}"),
                    Ok(Ok(())) | Err(_) => {}
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// Takes the right to sweep, if it hasn't been taken recently. The stamp is
    /// written before any work is done, so that a daemon starting up alongside
    /// this one backs off instead of duplicating the scan.
    fn claim_gc(&self, now: SystemTime) -> buck2_error::Result<bool> {
        let stamp = self.root.join(ForwardRelativePath::unchecked_new(GC_STAMP));
        if let Ok(metadata) = fs_util::symlink_metadata(&stamp)
            && !older_than(&metadata, now, self.gc_interval())
        {
            return Ok(false);
        }
        fs_util::write(&stamp, b"").categorize_internal()?;
        Ok(true)
    }

    /// A format bump makes the previous version's tree unreachable, so the
    /// sweep is the only thing that can reclaim it. Only `v<n>` directories
    /// older than the current format are touched, since the configured
    /// directory is a user-chosen path that may hold other things.
    fn remove_superseded_formats(&self) {
        let (Some(dir), Some(current)) = (self.root.parent(), format_version(FORMAT_VERSION))
        else {
            return;
        };
        for child in children(dir) {
            let superseded = child
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(format_version)
                .is_some_and(|version| version < current);
            if superseded {
                tracing::info!("download cache: removing superseded format at {child}");
                let _ignored = fs_util::remove_dir_all(&child);
            }
        }
    }

    fn run_gc(&self, now: SystemTime) -> buck2_error::Result<()> {
        // Before the gates below: a superseded tree is unreachable garbage
        // rather than a retained entry, and the state it most needs collecting
        // in is the one right after a bump, where this daemon's own root does
        // not exist yet.
        self.remove_superseded_formats();

        if !fs_util::try_exists(&self.root)? {
            return Ok(());
        }
        if let Some(max_age) = self.gc_max_age {
            if !self.claim_gc(now)? {
                return Ok(());
            }
            for algo in [KeyAlgorithm::Sha256, KeyAlgorithm::Sha1] {
                let algo_dir = self
                    .root
                    .join(ForwardRelativePath::unchecked_new(algo.dir_name()));
                for fanout in children(&algo_dir) {
                    for entry in children(&fanout) {
                        remove_if_older_than(&entry, now, max_age);
                    }
                }
            }
        }

        let tmp_dir = self.root.join(ForwardRelativePath::unchecked_new(TMP_DIR));
        for tmp in children(&tmp_dir) {
            remove_if_older_than(&tmp, now, TMP_MAX_AGE);
        }

        Ok(())
    }
}

/// `v3` -> `Some(3)`, anything else -> `None`.
fn format_version(name: &str) -> Option<u32> {
    name.strip_prefix('v')?.parse().ok()
}

/// Directory listing for garbage collection: anything unreadable is skipped
/// rather than aborting the sweep, since one stray file in the store would
/// otherwise stop it from ever collecting again.
fn children(dir: &AbsNormPath) -> Vec<AbsNormPathBuf> {
    let entries = match fs_util::read_dir_if_exists(dir) {
        Ok(Some(entries)) => entries,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::debug!("download cache: gc could not list {dir}: {e:#}");
            return Vec::new();
        }
    };

    let mut children = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(name) = ForwardRelativePath::new(name) else {
            continue;
        };
        children.push(dir.join(name));
    }
    children
}

fn remove_if_older_than(path: &AbsNormPath, now: SystemTime, age: Duration) {
    let Ok(metadata) = fs_util::symlink_metadata(path) else {
        return;
    };
    if fs_util::is_plain_file(&metadata) && older_than(&metadata, now, age) {
        let _ignored = fs_util::remove_file(path);
    }
}

fn older_than(metadata: &std::fs::Metadata, now: SystemTime, age: Duration) -> bool {
    match metadata.modified() {
        // A file from the future is not stale; it is a clock that moved.
        Ok(modified) => now.duration_since(modified).is_ok_and(|d| d > age),
        Err(_) => false,
    }
}

fn touch(entry: &AbsNormPath, interval: Duration) {
    let now = SystemTime::now();
    match fs_util::symlink_metadata(entry) {
        Ok(metadata) if older_than(&metadata, now, interval) => {}
        _ => return,
    }
    let times = std::fs::FileTimes::new().set_modified(now);
    let touched = std::fs::File::options()
        .write(true)
        .open(entry.as_maybe_relativized())
        .and_then(|file| file.set_times(times));
    if let Err(e) = touched {
        tracing::debug!("download cache: touch of {entry} failed: {e}");
    }
}

enum StoreError {
    /// The entry does not match the key it is filed under.
    Corrupt(String),
    /// The entry matches its key, but not something else the action declared.
    Mismatch(String),
    Io(buck2_error::Error),
}

fn report(e: StoreError, entry: &AbsNormPath) {
    match e {
        StoreError::Corrupt(reason) => {
            tracing::warn!("download cache: evicting {entry}: {reason}");
            let _ignored = fs_util::remove_file(entry);
        }
        StoreError::Mismatch(reason) => {
            tracing::debug!("download cache: skipping {entry}: {reason}");
        }
        StoreError::Io(e) => {
            tracing::debug!("download cache: read of {entry} failed: {e:#}");
        }
    }
}

struct VerifiedRead {
    size: u64,
    /// Present iff a `CasDigestConfig` was supplied.
    digest: Option<FileDigest>,
}

/// Reads `src`, checking its bytes against every checksum the action declared,
/// and optionally copies them out as it goes. `Ok(None)` means `src` does not
/// exist.
///
/// `dest` carries a flag that is set once the destination has been created, so
/// the caller knows whether there is a partial file to clean up.
fn read_verified(
    src: &AbsNormPath,
    key_algo: KeyAlgorithm,
    checksum: &Checksum,
    expected_size: Option<u64>,
    cas_digest_config: Option<CasDigestConfig>,
    dest: Option<(&AbsNormPath, &mut bool)>,
) -> Result<Option<VerifiedRead>, StoreError> {
    let Some(metadata) = fs_util::symlink_metadata_if_exists(src).map_err(StoreError::Io)? else {
        return Ok(None);
    };
    if !fs_util::is_plain_file(&metadata) {
        return Err(StoreError::Mismatch("entry is not a plain file".to_owned()));
    }
    if let Some(expected_size) = expected_size
        && metadata.len() != expected_size
    {
        return Err(StoreError::Mismatch(format!(
            "expected {} bytes, entry has {}",
            expected_size,
            metadata.len()
        )));
    }

    let Some(mut reader) = fs_util::open_file_if_exists(src).map_err(StoreError::Io)? else {
        return Ok(None);
    };

    let mut writer = match dest {
        Some((dest, wrote_dest)) => {
            if let Some(dir) = dest.parent() {
                fs_util::create_dir_all(dir).map_err(StoreError::Io)?;
            }
            // Unlink rather than truncate: `dest` may be a hardlink to
            // something else, most easily a store entry that a failed alias
            // left a second name for.
            let _ignored = fs_util::remove_file(dest);
            let file =
                fs_util::create_file(dest).map_err(|e| StoreError::Io(e.categorize_internal()))?;
            *wrote_dest = true;
            Some((file, dest))
        }
        None => None,
    };

    let mut digester = cas_digest_config.map(FileDigest::digester);
    let mut verifier = ChecksumVerifier::new(checksum, digester.as_ref().map(|d| d.algorithm()));

    let mut size = 0u64;
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) => return Err(StoreError::Io(io_error(e, "read", src))),
        };
        let chunk = &buf[..read];
        if let Some((writer, dest)) = writer.as_mut()
            && let Err(e) = writer.write_all(chunk)
        {
            return Err(StoreError::Io(io_error(e, "write", dest)));
        }
        size += read as u64;
        if let Some(digester) = digester.as_mut() {
            digester.update(chunk);
        }
        verifier.update(chunk);
    }
    if let Some((writer, dest)) = writer.as_mut() {
        if let Err(e) = writer.flush() {
            return Err(StoreError::Io(io_error(e, "flush", dest)));
        }
    }

    let digest = digester.map(|digester| digester.finalize());

    if let Err(mismatches) = verifier.finish(digest.as_ref()) {
        // Failing the key it is filed under means the entry is corrupt.
        // Failing something else the action declared means the entry is fine
        // and the action wanted different bytes.
        let corrupt = mismatches.iter().any(|m| m.family == key_algo.family());
        let reason = mismatches
            .iter()
            .map(|m| {
                format!(
                    "expected {} {}, entry has {}",
                    m.kind, m.expected, m.obtained
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if corrupt {
            StoreError::Corrupt(reason)
        } else {
            StoreError::Mismatch(reason)
        });
    }

    Ok(Some(VerifiedRead { size, digest }))
}

/// Copies `src` into the store, but only if its bytes still match `checksum`.
/// Returns whether an entry was stored.
fn insert(
    src: &AbsNormPath,
    key_algo: KeyAlgorithm,
    checksum: &Checksum,
    tmp: &AbsNormPath,
    entry: &AbsNormPath,
) -> buck2_error::Result<bool> {
    let result = (|| -> buck2_error::Result<bool> {
        let mut wrote_tmp = false;
        let read = read_verified(
            src,
            key_algo,
            checksum,
            None,
            None,
            Some((tmp, &mut wrote_tmp)),
        );
        match read {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(false),
            Err(StoreError::Io(e)) => return Err(e),
            Err(StoreError::Corrupt(reason) | StoreError::Mismatch(reason)) => {
                tracing::debug!("download cache: not storing {src}: {reason}");
                return Ok(false);
            }
        }

        if let Some(dir) = entry.parent() {
            fs_util::create_dir_all(dir)?;
        }
        fs_util::rename(tmp, entry).categorize_internal()?;
        Ok(true)
    })();

    match result {
        Ok(true) => Ok(true),
        Ok(false) => {
            let _ignored = fs_util::remove_file(tmp);
            Ok(false)
        }
        Err(e) => {
            let _ignored = fs_util::remove_file(tmp);
            Err(e)
        }
    }
}

/// Files a second key as a hardlink to the entry already stored under the
/// first, so aliases don't cost a second copy of the payload.
fn link_entry(
    from: &AbsNormPath,
    tmp: &AbsNormPath,
    entry: &AbsNormPath,
) -> buck2_error::Result<()> {
    if let Some(dir) = entry.parent() {
        fs_util::create_dir_all(dir)?;
    }
    fs_util::hard_link(from, tmp).categorize_internal()?;
    if let Err(e) = fs_util::rename(tmp, entry).categorize_internal() {
        let _ignored = fs_util::remove_file(tmp);
        return Err(e);
    }
    Ok(())
}

fn io_error(e: io::Error, op: &str, path: &AbsNormPath) -> buck2_error::Error {
    IoError::new_with_path(op, path, e).categorize_internal()
}

#[cfg(test)]
mod tests {
    use buck2_common::cas_digest::DigestAlgorithm;
    use buck2_core::fs::project::ProjectRoot;

    use super::*;
    use crate::execute::blocking::testing::DummyBlockingExecutor;

    /// sha1 and sha256 of `b"hello"`.
    const HELLO_SHA1: &str = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    struct Fixture {
        _dir: tempfile::TempDir,
        root: AbsNormPathBuf,
        cache: Arc<DownloadCache>,
        io: DummyBlockingExecutor,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_gc(Some(DEFAULT_GC_MAX_AGE))
        }

        fn with_gc(gc_max_age: Option<Duration>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = AbsNormPathBuf::new(dir.path().canonicalize().unwrap()).unwrap();
            let cache = DownloadCache::new(DownloadCacheConfig {
                dir: root.join(ForwardRelativePath::unchecked_new("cache")),
                gc_max_age,
                skip_head_request: false,
            });
            let io = DummyBlockingExecutor {
                fs: ProjectRoot::new_unchecked(root.clone()),
            };
            Self {
                _dir: dir,
                root,
                cache,
                io,
            }
        }

        fn path(&self, name: &str) -> AbsNormPathBuf {
            self.root.join(ForwardRelativePath::unchecked_new(name))
        }

        fn src(&self, contents: &[u8]) -> AbsNormPathBuf {
            let path = self.path("src");
            write(&path, contents);
            path
        }

        fn entry(&self, algo: KeyAlgorithm, hex: &str) -> AbsNormPathBuf {
            self.cache.entry_path(algo, hex).unwrap()
        }

        async fn put(&self, checksum: &Checksum, src: &AbsNormPath) {
            self.cache.put(checksum, src, &self.io).await
        }

        async fn verified_size(&self, checksum: &Checksum) -> Option<u64> {
            self.cache.verified_size(checksum, &self.io).await
        }

        async fn get(&self, checksum: &Checksum, expected_size: Option<u64>) -> Option<u64> {
            self.get_with(checksum, expected_size, DigestConfig::testing_default())
                .await
        }

        async fn get_with(
            &self,
            checksum: &Checksum,
            expected_size: Option<u64>,
            digest_config: DigestConfig,
        ) -> Option<u64> {
            self.cache
                .get(
                    checksum,
                    expected_size,
                    &self.path("dest"),
                    false,
                    digest_config,
                    &self.io,
                )
                .await
                .map(|digest| digest.size())
        }
    }

    fn write(path: &AbsNormPath, contents: &[u8]) {
        if let Some(dir) = path.parent() {
            fs_util::create_dir_all(dir).unwrap();
        }
        fs_util::write(path, contents).unwrap();
    }

    fn set_mtime(path: &AbsNormPath, time: SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path.as_maybe_relativized())
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(time))
            .unwrap();
    }

    fn mtime(path: &AbsNormPath) -> SystemTime {
        fs_util::symlink_metadata(path).unwrap().modified().unwrap()
    }

    /// Waits, briefly, for a background sweep to remove `path`.
    async fn collected(path: &AbsNormPath) -> bool {
        for _ in 0..400 {
            if !fs_util::try_exists(path).unwrap() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    fn sha1(hex: &str) -> Checksum {
        Checksum::new(Some(hex), None).unwrap()
    }

    fn sha256(hex: &str) -> Checksum {
        Checksum::new(None, Some(hex)).unwrap()
    }

    fn sha256_digest_config() -> DigestConfig {
        DigestConfig::leak_new(vec![DigestAlgorithm::Sha256], None).unwrap()
    }

    #[tokio::test]
    async fn put_then_get_roundtrips() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);

        f.put(&checksum, &src).await;

        assert_eq!(f.get(&checksum, Some(5)).await, Some(5));
        assert_eq!(fs_util::read(f.path("dest")).unwrap(), b"hello");
    }

    /// The sha256 read path is the one OSS daemons take, and it exercises the
    /// digester-reuse branch that the sha1 default does not.
    #[tokio::test]
    async fn put_then_get_roundtrips_with_sha256() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha256(HELLO_SHA256);

        f.put(&checksum, &src).await;

        assert_eq!(
            f.get_with(&checksum, Some(5), sha256_digest_config()).await,
            Some(5)
        );
        assert_eq!(fs_util::read(f.path("dest")).unwrap(), b"hello");
    }

    /// A checksum whose algorithm is not the daemon's digest algorithm has to
    /// be hashed separately, in both directions.
    #[tokio::test]
    async fn get_verifies_a_checksum_the_daemon_does_not_digest_with() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha256(HELLO_SHA256);
        f.put(&checksum, &src).await;

        assert_eq!(f.get(&checksum, None).await, Some(5));

        let entry = f.entry(KeyAlgorithm::Sha256, HELLO_SHA256);
        write(&entry, b"not hello");
        assert_eq!(f.get(&checksum, None).await, None);
        assert!(!fs_util::try_exists(&entry).unwrap());
    }

    #[tokio::test]
    async fn get_misses_when_empty() {
        let f = Fixture::new();
        assert_eq!(f.get(&sha1(HELLO_SHA1), None).await, None);
        assert!(!fs_util::try_exists(f.path("dest")).unwrap());
    }

    #[tokio::test]
    async fn corrupt_entry_is_evicted() {
        let f = Fixture::new();
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        write(&entry, b"not hello");

        assert_eq!(f.get(&sha1(HELLO_SHA1), None).await, None);
        assert!(!fs_util::try_exists(&entry).unwrap());
        assert!(!fs_util::try_exists(f.path("dest")).unwrap());
    }

    #[tokio::test]
    async fn wrong_size_entry_is_skipped_but_kept() {
        let f = Fixture::new();
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        write(&entry, b"hello");

        assert_eq!(f.get(&sha1(HELLO_SHA1), Some(1234)).await, None);
        // The entry matches its own key, so the declared size is what's wrong.
        assert!(fs_util::try_exists(&entry).unwrap());
    }

    /// A rejection that happens before the destination is created must not
    /// delete whatever was already there.
    #[tokio::test]
    async fn a_skipped_entry_leaves_the_destination_alone() {
        let f = Fixture::new();
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        write(&entry, b"hello");
        let dest = f.path("dest");
        write(&dest, b"pre-existing");

        assert_eq!(f.get(&sha1(HELLO_SHA1), Some(1234)).await, None);
        assert_eq!(fs_util::read(&dest).unwrap(), b"pre-existing");
    }

    #[tokio::test]
    async fn entry_matching_key_but_not_other_checksum_is_kept() {
        let f = Fixture::new();
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        write(&entry, b"hello");

        let checksum = Checksum::new(Some(HELLO_SHA1), Some(&"0".repeat(64))).unwrap();
        assert_eq!(f.get(&checksum, None).await, None);
        assert!(fs_util::try_exists(&entry).unwrap());
        assert!(!fs_util::try_exists(f.path("dest")).unwrap());
    }

    #[tokio::test]
    async fn put_stores_under_every_declared_key() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = Checksum::new(Some(HELLO_SHA1), Some(HELLO_SHA256)).unwrap();

        f.put(&checksum, &src).await;

        let sha1_entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        let sha256_entry = f.entry(KeyAlgorithm::Sha256, HELLO_SHA256);
        assert_eq!(fs_util::read(&sha1_entry).unwrap(), b"hello");
        assert_eq!(fs_util::read(&sha256_entry).unwrap(), b"hello");
        // The alias is a hardlink, not a second copy of the payload.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs_util::metadata(&sha1_entry).unwrap().ino(),
                fs_util::metadata(&sha256_entry).unwrap().ino()
            );
        }
    }

    #[tokio::test]
    async fn put_replaces_an_unusable_entry() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        write(&entry, b"truncated");

        f.put(&sha1(HELLO_SHA1), &src).await;

        assert_eq!(fs_util::read(&entry).unwrap(), b"hello");
    }

    /// The caller verified a stream; the file it points at can have changed
    /// since, and must not be filed under a key it no longer matches.
    #[tokio::test]
    async fn put_rejects_a_source_that_does_not_match_the_key() {
        let f = Fixture::new();
        let src = f.src(b"not hello");

        f.put(&sha1(HELLO_SHA1), &src).await;

        assert!(!fs_util::try_exists(f.entry(KeyAlgorithm::Sha1, HELLO_SHA1)).unwrap());
        assert!(
            children(
                &f.cache
                    .root
                    .join(ForwardRelativePath::unchecked_new(TMP_DIR))
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn put_of_a_missing_source_leaves_no_tmp_file() {
        let f = Fixture::new();

        f.put(&sha1(HELLO_SHA1), &f.path("does-not-exist")).await;

        assert!(
            children(
                &f.cache
                    .root
                    .join(ForwardRelativePath::unchecked_new(TMP_DIR))
            )
            .is_empty()
        );
    }

    /// The destination is unlinked, not truncated: it can be a second name for
    /// a store entry, which truncating would destroy.
    #[tokio::test]
    async fn get_replaces_the_destination_instead_of_writing_through_it() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);
        f.put(&checksum, &src).await;

        let other = f.entry(KeyAlgorithm::Sha256, HELLO_SHA256);
        write(&other, b"a different entry");
        let dest = f.path("dest");
        std::fs::hard_link(other.as_maybe_relativized(), dest.as_maybe_relativized()).unwrap();

        assert_eq!(f.get(&checksum, None).await, Some(5));

        assert_eq!(fs_util::read(&dest).unwrap(), b"hello");
        assert_eq!(
            fs_util::read(&other).unwrap(),
            b"a different entry",
            "the file the destination was linked to must be untouched"
        );
    }

    /// A stale executable bit on the destination must not survive a download
    /// that is not executable.
    #[tokio::test]
    async fn get_does_not_inherit_the_destination_permissions() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);
        f.put(&checksum, &src).await;

        let dest = f.path("dest");
        write(&dest, b"was executable");
        fs_util::set_executable(&dest, true).unwrap();

        assert_eq!(f.get(&checksum, None).await, Some(5));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs_util::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0);
        }
    }

    #[tokio::test]
    async fn executable_bit_is_not_stored_but_is_applied() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        fs_util::set_executable(&src, true).unwrap();
        let checksum = sha1(HELLO_SHA1);

        f.put(&checksum, &src).await;

        let dest = f.path("dest");
        assert!(
            f.cache
                .get(
                    &checksum,
                    None,
                    &dest,
                    true,
                    DigestConfig::testing_default(),
                    &f.io,
                )
                .await
                .is_some()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
            let entry_mode = fs_util::metadata(&entry).unwrap().permissions().mode();
            assert_eq!(entry_mode & 0o111, 0, "store entry must not be executable");
            let dest_mode = fs_util::metadata(&dest).unwrap().permissions().mode();
            assert_ne!(dest_mode & 0o111, 0, "executable bit must be applied");
        }
    }

    #[tokio::test]
    async fn get_overwrites_an_existing_destination() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);
        f.put(&checksum, &src).await;

        let dest = f.path("dest");
        write(&dest, b"stale contents that are longer");

        assert_eq!(f.get(&checksum, None).await, Some(5));
        assert_eq!(fs_util::read(&dest).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn verified_size_reports_entry_length() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);

        assert_eq!(f.verified_size(&checksum).await, None);
        f.put(&checksum, &src).await;
        assert_eq!(f.verified_size(&checksum).await, Some(5));
    }

    /// A size is committed to the artifact's digest, so it must never come
    /// from bytes that don't match the key.
    #[tokio::test]
    async fn verified_size_rejects_and_evicts_a_corrupt_entry() {
        let f = Fixture::new();
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        write(&entry, b"");

        assert_eq!(f.verified_size(&sha1(HELLO_SHA1)).await, None);
        assert!(!fs_util::try_exists(&entry).unwrap());
    }

    #[tokio::test]
    async fn a_hit_refreshes_the_entry_mtime() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);
        f.put(&checksum, &src).await;
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);

        let stale = SystemTime::now() - DEFAULT_GC_MAX_AGE / 2;
        set_mtime(&entry, stale);
        assert_eq!(f.get(&checksum, None).await, Some(5));
        assert!(mtime(&entry) > stale, "get must refresh the mtime");

        set_mtime(&entry, stale);
        assert_eq!(f.verified_size(&checksum).await, Some(5));
        assert!(
            mtime(&entry) > stale,
            "a size lookup is a use, and must refresh the mtime too"
        );
    }

    /// With a short retention the touch interval has to shrink with it, or
    /// entries expire while they are still being used on every build.
    #[tokio::test]
    async fn a_hit_refreshes_the_mtime_under_a_short_retention() {
        let f = Fixture::with_gc(Some(ONE_HOUR));
        let src = f.src(b"hello");
        let checksum = sha1(HELLO_SHA1);
        f.put(&checksum, &src).await;
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);

        let stale = SystemTime::now() - ONE_HOUR / 2;
        set_mtime(&entry, stale);
        assert_eq!(f.get(&checksum, None).await, Some(5));

        assert!(mtime(&entry) > stale);
    }

    #[tokio::test]
    async fn gc_removes_stale_entries_and_keeps_fresh_ones() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;
        f.put(&sha256(HELLO_SHA256), &src).await;

        let stale = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        let fresh = f.entry(KeyAlgorithm::Sha256, HELLO_SHA256);
        let now = SystemTime::now();
        set_mtime(&stale, now - DEFAULT_GC_MAX_AGE * 2);

        f.cache.run_gc(now).unwrap();

        assert!(!fs_util::try_exists(&stale).unwrap());
        assert!(fs_util::try_exists(&fresh).unwrap());
    }

    /// The states a superseded tree most needs collecting in are the ones the
    /// entry sweep skips: right after a bump this daemon's own root does not
    /// exist yet, and a store told to keep entries forever still shouldn't keep
    /// a tree nothing can read.
    #[tokio::test]
    async fn gc_removes_superseded_formats_before_its_own_root_exists() {
        let f = Fixture::with_gc(None);
        let dir = f.cache.root.parent().unwrap().to_buf();
        let superseded = dir.join(ForwardRelativePath::unchecked_new("v0/sha1/aa/entry"));
        write(&superseded, b"x");
        assert!(!fs_util::try_exists(&f.cache.root).unwrap());

        f.cache.run_gc(SystemTime::now()).unwrap();

        assert!(!fs_util::try_exists(&superseded).unwrap());
    }

    /// A deletion path must not follow a link out of the store.
    #[cfg(unix)]
    #[tokio::test]
    async fn gc_does_not_follow_a_link_out_of_the_store() {
        let f = Fixture::new();
        let outside = f.path("outside/precious");
        write(&outside, b"precious");

        let dir = f.cache.root.parent().unwrap().to_buf();
        let link = dir.join(ForwardRelativePath::unchecked_new("v0"));
        fs_util::create_dir_all(&dir).unwrap();
        fs_util::symlink(f.path("outside").as_path(), &link).unwrap();

        f.cache.run_gc(SystemTime::now()).unwrap();

        assert_eq!(fs_util::read(&outside).unwrap(), b"precious");
    }

    /// A format bump makes the old tree unreachable; only the sweep can
    /// reclaim it, and it must not reach anything else in the directory.
    #[tokio::test]
    async fn gc_removes_superseded_formats_and_nothing_else() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;

        let dir = f.cache.root.parent().unwrap().to_buf();
        let superseded = dir.join(ForwardRelativePath::unchecked_new("v0/sha1/aa/entry"));
        let future = dir.join(ForwardRelativePath::unchecked_new("v9/sha1/aa/entry"));
        let unrelated = dir.join(ForwardRelativePath::unchecked_new("notes.txt"));
        let unrelated_dir = dir.join(ForwardRelativePath::unchecked_new("vendor/thing"));
        for path in [&superseded, &future, &unrelated, &unrelated_dir] {
            write(path, b"x");
        }

        f.cache.run_gc(SystemTime::now()).unwrap();

        assert!(!fs_util::try_exists(&superseded).unwrap());
        assert!(
            fs_util::try_exists(&future).unwrap(),
            "never delete forwards"
        );
        assert!(fs_util::try_exists(&unrelated).unwrap());
        assert!(
            fs_util::try_exists(&unrelated_dir).unwrap(),
            "a directory that is not a format version is not ours to delete"
        );
        assert!(fs_util::try_exists(f.entry(KeyAlgorithm::Sha1, HELLO_SHA1)).unwrap());
    }

    #[tokio::test]
    async fn gc_removes_leaked_tmp_files() {
        let f = Fixture::new();
        let leaked = f.cache.tmp_path();
        write(&leaked, b"partial");
        let now = SystemTime::now();
        set_mtime(&leaked, now - TMP_MAX_AGE * 2);

        f.cache.run_gc(now).unwrap();

        assert!(!fs_util::try_exists(&leaked).unwrap());
    }

    /// Turning off entry collection is not a request to leak partial copies.
    #[tokio::test]
    async fn gc_removes_leaked_tmp_files_even_when_collection_is_off() {
        let f = Fixture::with_gc(None);
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        let leaked = f.cache.tmp_path();
        write(&leaked, b"partial");

        let now = SystemTime::now();
        set_mtime(&leaked, now - TMP_MAX_AGE * 2);
        set_mtime(&entry, now - DEFAULT_GC_MAX_AGE * 100);

        f.cache.run_gc(now).unwrap();

        assert!(!fs_util::try_exists(&leaked).unwrap());
        assert!(fs_util::try_exists(&entry).unwrap());
    }

    /// One stray file in the store must not stop it from ever collecting.
    #[tokio::test]
    async fn gc_tolerates_junk_in_the_store() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);

        let junk = f
            .cache
            .root
            .join(ForwardRelativePath::unchecked_new("sha1/.DS_Store"));
        write(&junk, b"junk");
        let leaked = f.cache.tmp_path();
        write(&leaked, b"partial");

        let now = SystemTime::now();
        set_mtime(&entry, now - DEFAULT_GC_MAX_AGE * 2);
        set_mtime(&leaked, now - TMP_MAX_AGE * 2);

        f.cache.run_gc(now).unwrap();

        assert!(!fs_util::try_exists(&entry).unwrap());
        assert!(!fs_util::try_exists(&leaked).unwrap());
    }

    #[tokio::test]
    async fn gc_is_rate_limited_by_the_stamp() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;

        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        let now = SystemTime::now();
        set_mtime(&entry, now - DEFAULT_GC_MAX_AGE * 2);

        let stamp = f
            .cache
            .root
            .join(ForwardRelativePath::unchecked_new(GC_STAMP));
        write(&stamp, b"");

        f.cache.run_gc(now).unwrap();

        assert!(fs_util::try_exists(&entry).unwrap());
    }

    /// The stamp has to be claimed before the sweep, not after, or two daemons
    /// starting together both do the whole scan.
    #[tokio::test]
    async fn claiming_gc_writes_the_stamp_and_excludes_the_next_claim() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;

        let stamp = f
            .cache
            .root
            .join(ForwardRelativePath::unchecked_new(GC_STAMP));
        let now = SystemTime::now();

        assert!(f.cache.claim_gc(now).unwrap());
        assert!(fs_util::try_exists(&stamp).unwrap());
        assert!(!f.cache.claim_gc(now).unwrap(), "the claim is exclusive");
        assert!(
            f.cache.claim_gc(now + DEFAULT_GC_MAX_AGE).unwrap(),
            "a later claim, past the interval, succeeds"
        );
    }

    /// A source that doesn't match its key stores nothing, so there is nothing
    /// for the remaining keys to be an alias of either.
    #[tokio::test]
    async fn a_rejected_put_does_not_alias_the_other_keys() {
        let f = Fixture::new();
        let src = f.src(b"not hello");

        f.put(
            &Checksum::new(Some(HELLO_SHA1), Some(HELLO_SHA256)).unwrap(),
            &src,
        )
        .await;

        assert!(!fs_util::try_exists(f.entry(KeyAlgorithm::Sha1, HELLO_SHA1)).unwrap());
        assert!(!fs_util::try_exists(f.entry(KeyAlgorithm::Sha256, HELLO_SHA256)).unwrap());
    }

    #[tokio::test]
    async fn gc_writes_the_stamp_so_other_daemons_back_off() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;

        let stamp = f
            .cache
            .root
            .join(ForwardRelativePath::unchecked_new(GC_STAMP));
        assert!(!fs_util::try_exists(&stamp).unwrap());

        f.cache.run_gc(SystemTime::now()).unwrap();

        assert!(fs_util::try_exists(&stamp).unwrap());
    }

    /// The sweep has to keep happening; a daemon can outlive many retentions.
    #[tokio::test]
    async fn gc_keeps_sweeping_after_the_first_pass() {
        let f = Fixture::new();
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;
        let first = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        set_mtime(&first, SystemTime::now() - DEFAULT_GC_MAX_AGE * 2);

        f.cache.spawn_gc_every(Duration::from_millis(5));
        assert!(collected(&first).await, "the first sweep must run");

        // A second entry goes stale after that sweep. Nothing re-arms the loop,
        // so only a genuinely periodic sweep collects it.
        f.put(&sha256(HELLO_SHA256), &src).await;
        let second = f.entry(KeyAlgorithm::Sha256, HELLO_SHA256);
        let stamp = f
            .cache
            .root
            .join(ForwardRelativePath::unchecked_new(GC_STAMP));
        let stale = SystemTime::now() - DEFAULT_GC_MAX_AGE * 2;
        set_mtime(&second, stale);
        set_mtime(&stamp, stale);

        assert!(collected(&second).await, "a later sweep must run too");
    }

    /// Two projects can disagree about retention. The one that keeps entries
    /// forever must not stop the other from collecting.
    #[tokio::test]
    async fn a_non_collecting_daemon_does_not_claim_the_sweep() {
        let f = Fixture::with_gc(None);
        let src = f.src(b"hello");
        f.put(&sha1(HELLO_SHA1), &src).await;
        let entry = f.entry(KeyAlgorithm::Sha1, HELLO_SHA1);
        let now = SystemTime::now();
        set_mtime(&entry, now - DEFAULT_GC_MAX_AGE * 2);

        f.cache.run_gc(now).unwrap();

        let stamp = f
            .cache
            .root
            .join(ForwardRelativePath::unchecked_new(GC_STAMP));
        assert!(!fs_util::try_exists(&stamp).unwrap());
        assert!(
            f.cache.claim_gc(now).unwrap(),
            "a collector can still claim"
        );
    }

    #[tokio::test]
    async fn gc_on_a_store_that_was_never_written_is_a_no_op() {
        let f = Fixture::new();
        f.cache.run_gc(SystemTime::now()).unwrap();
        assert!(!fs_util::try_exists(&f.cache.root).unwrap());
    }

    #[test]
    fn keys_are_strongest_first() {
        let checksum = Checksum::new(Some(HELLO_SHA1), Some(HELLO_SHA256)).unwrap();
        assert_eq!(
            keys(&checksum),
            vec![
                (KeyAlgorithm::Sha256, HELLO_SHA256),
                (KeyAlgorithm::Sha1, HELLO_SHA1),
            ]
        );
    }

    #[test]
    fn a_malformed_key_is_never_turned_into_a_path() {
        assert!(!is_valid_key(KeyAlgorithm::Sha1, "../../etc/passwd"));
        assert!(!is_valid_key(KeyAlgorithm::Sha1, HELLO_SHA256));
        assert!(!is_valid_key(KeyAlgorithm::Sha256, HELLO_SHA1));
        assert!(!is_valid_key(KeyAlgorithm::Sha1, &"z".repeat(40)));
        assert!(is_valid_key(KeyAlgorithm::Sha1, HELLO_SHA1));
        assert!(is_valid_key(KeyAlgorithm::Sha256, HELLO_SHA256));
    }

    #[test]
    fn touch_interval_shrinks_with_the_retention() {
        let interval = |gc_max_age| {
            DownloadCache::new(DownloadCacheConfig {
                dir: AbsNormPathBuf::new(if cfg!(windows) {
                    PathBuf::from("C:\\store")
                } else {
                    PathBuf::from("/store")
                })
                .unwrap(),
                gc_max_age,
                skip_head_request: false,
            })
            .touch_interval()
        };

        assert_eq!(interval(Some(DEFAULT_GC_MAX_AGE)), ONE_DAY);
        assert_eq!(interval(Some(ONE_HOUR)), ONE_HOUR / 4);
        assert_eq!(
            interval(None),
            ONE_DAY,
            "another daemon sharing the store may still be collecting"
        );
    }

    #[test]
    fn config_defaults() {
        let config = DownloadCacheConfig::new(&DownloadCacheStartupConfig::default()).unwrap();
        assert_eq!(config, None, "the store is opt-in");

        let enabled = DownloadCacheStartupConfig {
            enabled: true,
            ..Default::default()
        };
        let config = DownloadCacheConfig::new(&enabled).unwrap();
        if dirs::cache_dir().is_some() {
            let config = config.unwrap();
            assert_eq!(config.gc_max_age, Some(DEFAULT_GC_MAX_AGE));
            assert!(!config.skip_head_request);
            assert!(config.dir.as_path().ends_with("buck2/downloads"));
        } else {
            assert_eq!(config, None, "an unusable default disables, not errors");
        }
    }

    #[test]
    fn config_blank_dir_falls_back_to_the_default() {
        let blank = DownloadCacheStartupConfig {
            enabled: true,
            dir: Some("   ".to_owned()),
            ..Default::default()
        };
        let explicit = DownloadCacheStartupConfig {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(
            DownloadCacheConfig::new(&blank).unwrap(),
            DownloadCacheConfig::new(&explicit).unwrap()
        );
    }

    #[test]
    fn config_zero_max_age_disables_collection() {
        let config = DownloadCacheConfig::new(&DownloadCacheStartupConfig {
            enabled: true,
            dir: Some(if cfg!(windows) { "C:\\store" } else { "/store" }.to_owned()),
            gc_max_age_secs: Some(0),
            skip_head_request: true,
        })
        .unwrap()
        .unwrap();

        assert_eq!(config.gc_max_age, None);
        assert!(config.skip_head_request);
    }

    #[test]
    fn config_rejects_a_bad_explicit_dir() {
        let bad = |dir: &str| {
            DownloadCacheConfig::new(&DownloadCacheStartupConfig {
                enabled: true,
                dir: Some(dir.to_owned()),
                ..Default::default()
            })
            .is_err()
        };

        assert!(bad("relative/path"), "relative paths are rejected");
        assert!(bad("$HOME/store"), "env vars are not expanded");
        if cfg!(unix) {
            assert!(bad("/a/../b"), "unnormalized paths are rejected");
        }
    }

    #[test]
    fn dir_expands_a_leading_tilde() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(
            parse_dir("~/x/y").unwrap().as_path(),
            home.join("x").join("y")
        );
        assert_eq!(parse_dir("~").unwrap().as_path(), home);
        if cfg!(windows) {
            assert_eq!(parse_dir("~\\x").unwrap().as_path(), home.join("x"));
        }
    }
}
