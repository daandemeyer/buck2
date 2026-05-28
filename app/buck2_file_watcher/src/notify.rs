/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;
use std::mem;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_common::file_ops::dice::FileChangeTracker;
use buck2_common::ignores::ignore_set::IgnoreSet;
use buck2_common::invocation_paths::InvocationPaths;
use buck2_core::cells::CellResolver;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::name::CellName;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_data::FileWatcherEventType;
use buck2_data::FileWatcherKind;
use buck2_error::conversion::from_any_with_tag;
use buck2_events::dispatch::span_async;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_hash::StdBuckHashMap;
use dice::DiceTransactionUpdater;
use dupe::Dupe;
use notify::EventKind;
use notify::RecommendedWatcher;
use notify::WatchFilter;
use notify::Watcher;
use notify::event::CreateKind;
use notify::event::MetadataKind;
use notify::event::ModifyKind;
use notify::event::RemoveKind;
use starlark_map::ordered_set::OrderedSet;
use tracing::debug;
use tracing::info;

use crate::file_watcher::FileWatcher;
use crate::mergebase::Mergebase;
use crate::stats::FileWatcherStats;
use crate::watchman::utils::find_first_valid_parent;

fn ignore_event_kind(event_kind: EventKind) -> bool {
    match event_kind {
        EventKind::Access(_) => true,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Ownership))
        | EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)) => false,
        EventKind::Modify(ModifyKind::Metadata(_)) => true,
        _ => false,
    }
}

/// Buffer containing the events that have happened since we last got a message.
/// Used to dedupe events, since notify sends a notification on every change.
#[derive(Allocative)]
struct NotifyFileData {
    ignored: u64,
    #[allocative(skip)]
    events: OrderedSet<(CellPath, EventKind)>,
    /// Whether file system changes were missed
    missed_events: bool,
}

impl NotifyFileData {
    fn new() -> Self {
        Self {
            ignored: 0,
            events: OrderedSet::new(),
            missed_events: false,
        }
    }

    fn process(
        &mut self,
        event: notify::Result<notify::Event>,
        root: &ProjectRoot,
        cells: &CellResolver,
        ignore_specs: &StdBuckHashMap<CellName, IgnoreSet>,
    ) -> buck2_error::Result<()> {
        let event = match event {
            Ok(event) => event,
            // The watcher failed at something, typically installing a watch for a directory that
            // just appeared. It is not fatal, but from here on the watch tree covers less than the
            // whole project, and a directory nobody watches produces no events at all: the daemon
            // would keep serving whatever it read last. Treat it exactly like dropped events, which
            // clears DICE and registers the tree again.
            Err(e) => {
                self.missed_events = true;
                info!("FileWatcher: watcher error, coverage may be incomplete: {e:?}");
                return Ok(());
            }
        };

        // Checked before the path loop: the kernel-overflow rescan signal can arrive with no
        // paths attached (platform dependent), and a missed rescan means the daemon keeps
        // answering from a stale graph.
        if event.need_rescan() {
            self.missed_events = true;
            debug!("FileWatcher: File change events were missed");
        }

        for path in &event.paths {
            // We ignore the buck-out prefix, as those are uninteresting events caused by us.
            // We also ignore other buck-out directories, as if you have two isolation dirs running at once, they are not interesting.
            // The watch filter already prunes buck-out at watch registration time, but backends
            // that cannot watch selectively may still let events slip through in rare cases.
            //
            // Checked on the raw path so this dominant event class is discarded
            // cheaply, whatever bytes the path contains.
            if let Ok(rel) = path.strip_prefix(root.root().as_path())
                && rel.starts_with(InvocationPaths::buck_out_dir_prefix().as_str())
            {
                // We don't want to event add them as ignored events, since they are super common
                // and very boring
                continue;
            }

            // Uninteresting event kinds don't need the path at all.
            if ignore_event_kind(event.kind) {
                self.ignored += 1;
                continue;
            }

            // Testing shows that we get absolute paths back from the `notify` library.
            // It's not documented though.
            //
            // Relativized leniently because ignored directories can transiently
            // contain names `ProjectRelativePath` rejects (e.g. a literal backslash),
            // and those must reach the ignore check below rather than fail: an error
            // would poison the watcher permanently.
            let rel = match AbsNormPath::new(&path).and_then(|path| root.relativize_relaxed(path)) {
                Ok(rel) => rel,
                Err(e) => match path.strip_prefix(root.root().as_path()) {
                    // Not relativizable at all (in practice: a non-UTF-8 name).
                    Ok(raw_rel) => {
                        self.degrade_to_parent(raw_rel, cells, ignore_specs);
                        continue;
                    }
                    // Outside the project root: a genuine error.
                    Err(_) => return Err(e),
                },
            };

            // The relaxed path may violate the `ProjectRelativePath` invariants; it
            // is only used to match the ignores, never to identify a file.
            let cell_path = cells.get_cell_path(ProjectRelativePath::unchecked_new(&rel));
            let ignore = ignore_specs
                .get(&cell_path.cell())
                // See the comment on the analogous code in `watchman/interface.rs`
                .is_some_and(|ignore| ignore.is_match(cell_path.path()));

            info!(
                "FileWatcher: {:?} {:?} (ignore = {})",
                rel, &event.kind, ignore
            );

            if ignore {
                self.ignored += 1;
            } else if ProjectRelativePath::new(&*rel).is_ok() {
                self.events.insert((cell_path, event.kind));
            } else {
                // Interesting, but not representable (e.g. an embedded backslash).
                self.degrade_to_parent(Path::new(&*rel), cells, ignore_specs);
            }
        }
        Ok(())
    }

    /// The event path cannot be represented as a `ProjectRelativePath`. Buck
    /// cannot read such paths anyway, so record a change of the nearest
    /// representable parent directory instead — mirroring the watchman and
    /// edenfs watchers — rather than erroring, which would poison the watcher.
    fn degrade_to_parent(
        &mut self,
        rel: &Path,
        cells: &CellResolver,
        ignore_specs: &StdBuckHashMap<CellName, IgnoreSet>,
    ) {
        let parent = find_first_valid_parent(rel).unwrap_or(ProjectRelativePath::empty());
        let cell_path = cells.get_cell_path(parent);
        let ignore = ignore_specs
            .get(&cell_path.cell())
            .is_some_and(|ignore| ignore.is_match(cell_path.path()));

        info!(
            "FileWatcher: {:?} -> {:?} (unrepresentable path, ignore = {})",
            rel, parent, ignore
        );

        if ignore {
            self.ignored += 1;
        } else {
            // Maps to `dir_added_or_removed` in `sync`, invalidating the
            // parent's directory listing.
            self.events
                .insert((cell_path, EventKind::Create(CreateKind::Folder)));
        }
    }

    fn sync(self) -> (buck2_data::FileWatcherStats, Option<FileChangeTracker>) {
        // The changes that go into the DICE transaction
        let mut changed = FileChangeTracker::new();
        // If we missed events, sync2() will drop the entire DICE graph and register the watch
        // tree again. Surface that to telemetry/UI by reusing the fresh-instance fields the
        // watchman path uses when it clears DICE for the same reason.
        let base = if self.missed_events {
            buck2_data::FileWatcherStats {
                fresh_instance: true,
                fresh_instance_data: Some(buck2_data::FreshInstance {
                    new_mergebase: false,
                    cleared_dice: true,
                    cleared_dep_files: false,
                }),
                incomplete_events_reason: Some(
                    "notify dropped events or failed to watch a directory".to_owned(),
                ),
                ..Default::default()
            }
        } else {
            Default::default()
        };
        let mut stats = FileWatcherStats::new(base, self.events.len());
        stats.add_ignored(self.ignored);

        for (cell_path, event_kind) in self.events {
            let cell_path_str = cell_path.to_string();
            match event_kind {
                EventKind::Create(create_kind) => match create_kind {
                    CreateKind::File => {
                        changed.file_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Create,
                            FileWatcherKind::File,
                        );
                    }
                    CreateKind::Folder => {
                        changed.dir_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Create,
                            FileWatcherKind::Directory,
                        );
                    }
                    CreateKind::Any | CreateKind::Other => {
                        changed.file_added_or_removed(cell_path.clone());
                        stats.add(
                            cell_path_str.clone(),
                            FileWatcherEventType::Create,
                            FileWatcherKind::File,
                        );
                        changed.dir_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Create,
                            FileWatcherKind::Directory,
                        );
                    }
                },
                EventKind::Modify(modify_kind) => match modify_kind {
                    ModifyKind::Data(_) | ModifyKind::Metadata(_) => {
                        changed.file_contents_changed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Modify,
                            FileWatcherKind::File,
                        );
                    }
                    ModifyKind::Name(_) | ModifyKind::Any | ModifyKind::Other => {
                        changed.file_added_or_removed(cell_path.clone());
                        stats.add(
                            cell_path_str.clone(),
                            FileWatcherEventType::Create,
                            FileWatcherKind::File,
                        );
                        stats.add(
                            cell_path_str.clone(),
                            FileWatcherEventType::Delete,
                            FileWatcherKind::File,
                        );
                        changed.dir_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str.clone(),
                            FileWatcherEventType::Create,
                            FileWatcherKind::Directory,
                        );
                        stats.add(
                            cell_path_str.clone(),
                            FileWatcherEventType::Delete,
                            FileWatcherKind::Directory,
                        );
                    }
                },
                EventKind::Remove(remove_kind) => match remove_kind {
                    RemoveKind::File => {
                        changed.file_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Delete,
                            FileWatcherKind::File,
                        );
                    }
                    RemoveKind::Folder => {
                        changed.dir_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Delete,
                            FileWatcherKind::Directory,
                        );
                    }
                    RemoveKind::Any | RemoveKind::Other => {
                        changed.file_added_or_removed(cell_path.clone());
                        stats.add(
                            cell_path_str.clone(),
                            FileWatcherEventType::Delete,
                            FileWatcherKind::File,
                        );
                        changed.dir_added_or_removed(cell_path);
                        stats.add(
                            cell_path_str,
                            FileWatcherEventType::Delete,
                            FileWatcherKind::Directory,
                        );
                    }
                },
                _ => {}
            }
        }

        let stats = stats.finish();
        let changed = if self.missed_events {
            None
        } else {
            Some(changed)
        };

        (stats, changed)
    }
}

/// What it takes to register the watch tree, kept so that it can be built again.
struct Registration {
    root: ProjectRoot,
    cells: CellResolver,
    ignore_specs: StdBuckHashMap<CellName, IgnoreSet>,
}

/// Paths whose watch could not be installed.
///
/// A path that failed once fails again on every registration, a directory the user has no
/// permission for being the obvious case. Remembering them keeps such a directory from making every
/// single command drop DICE and walk the tree again; they cost that once.
type FailedWatches = Arc<Mutex<HashSet<PathBuf>>>;

/// A filter that prunes buck-out and ignored directories at watch-registration
/// time: inotify never installs watches beneath them, so they generate no
/// events at all. Backends that cannot watch selectively (FSEvents, Windows)
/// suppress the events on delivery instead, before they reach our callback.
///
/// This prunes any directory whose own path matches an ignore pattern, so a
/// file-shaped glob (e.g. `*.tmp`) matching a directory name prunes that
/// whole subtree.
fn ignore_watch_filter(
    root: &ProjectRoot,
    cells: &CellResolver,
    ignore_specs: &StdBuckHashMap<CellName, IgnoreSet>,
) -> WatchFilter {
    let root = root.dupe();
    let cells = cells.dupe();
    let ignore_specs = ignore_specs.clone();
    WatchFilter::with_filter(move |path| {
        // Prune paths we cannot represent (e.g. non-UTF-8): buck cannot read
        // them anyway, so their events would be unusable.
        let Ok(rel) = AbsNormPath::new(path).and_then(|abs| root.relativize(abs)) else {
            return false;
        };
        if rel.starts_with(InvocationPaths::buck_out_dir_prefix()) {
            return false;
        }
        let cell_path = cells.get_cell_path(&rel);
        !ignore_specs
            .get(&cell_path.cell())
            .is_some_and(|i| i.is_match(cell_path.path()))
    })
}

#[derive(Allocative)]
pub struct NotifyFileWatcher {
    /// Never used directly, but must be kept alive: dropping the watcher removes all its watches.
    /// Replaced wholesale when the tree has to be registered again.
    #[allocative(skip)]
    watcher: Mutex<RecommendedWatcher>,
    #[allocative(skip)]
    registration: Registration,
    #[allocative(skip)]
    failed: FailedWatches,
    data: Arc<Mutex<buck2_error::Result<NotifyFileData>>>,
}

impl NotifyFileWatcher {
    pub fn new(
        root: &ProjectRoot,
        cells: CellResolver,
        ignore_specs: StdBuckHashMap<CellName, IgnoreSet>,
    ) -> buck2_error::Result<Self> {
        let data = Arc::new(Mutex::new(Ok(NotifyFileData::new())));
        let registration = Registration {
            root: root.dupe(),
            cells,
            ignore_specs,
        };
        let failed: FailedWatches = Default::default();
        let watcher = Self::register(&registration, data.dupe(), failed.dupe())?;
        Ok(Self {
            watcher: Mutex::new(watcher),
            registration,
            failed,
            data,
        })
    }

    /// Watch the whole project.
    fn register(
        registration: &Registration,
        data: Arc<Mutex<buck2_error::Result<NotifyFileData>>>,
        failed: FailedWatches,
    ) -> buck2_error::Result<RecommendedWatcher> {
        let watch_filter = ignore_watch_filter(
            &registration.root,
            &registration.cells,
            &registration.ignore_specs,
        );
        let root = registration.root.dupe();
        let cells = registration.cells.dupe();
        let ignore_specs = registration.ignore_specs.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                // A path we already know we cannot watch is not news, and reacting to it again would
                // make every command drop DICE for as long as it exists.
                if let Err(e) = &event {
                    let mut failed = failed.lock().unwrap();
                    let novel: Vec<_> = e.paths.iter().map(|p| failed.insert(p.clone())).collect();
                    if !e.paths.is_empty() && !novel.contains(&true) {
                        debug!("FileWatcher: {:?} failed to watch again", e.paths);
                        return;
                    }
                }
                let mut guard = data.lock().unwrap();
                if let Ok(state) = &mut *guard {
                    if let Err(e) = state.process(event, &root, &cells, &ignore_specs) {
                        *guard = Err(e);
                    }
                }
            })
            .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::NotifyWatcher))?;
        watcher
            .watch_filtered(
                registration.root.root().as_path(),
                notify::RecursiveMode::Recursive,
                watch_filter,
            )
            .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::NotifyWatcher))?;
        Ok(watcher)
    }

    /// Register the tree again, after events were dropped or a watch failed to install.
    ///
    /// Dropping DICE recovers from the changes we did not see, but not from a directory that has no
    /// watch at all: nothing would ever report a change there again. The new registration walks the
    /// tree and covers whatever the old one is missing. It is built before the old one is dropped,
    /// so no window opens where the project is unwatched; the overlap only duplicates events.
    fn reregister(&self) -> buck2_error::Result<()> {
        let watcher = Self::register(&self.registration, self.data.dupe(), self.failed.dupe())?;
        *self.watcher.lock().unwrap() = watcher;
        Ok(())
    }

    fn sync2(
        &self,
        mut dice: DiceTransactionUpdater,
    ) -> buck2_error::Result<(buck2_data::FileWatcherStats, DiceTransactionUpdater)> {
        let old = {
            let mut guard = self.data.lock().unwrap();
            mem::replace(&mut *guard, Ok(NotifyFileData::new()))
        };
        let (stats, changes) = old?.sync();
        if let Some(changes) = changes {
            changes.write_to_dice(&mut dice)?;
        } else {
            // We missed some file system notifications, so we drop everything and make sure the
            // watch tree covers the project again before we read it back.
            dice = dice.unstable_take();
            self.reregister()?;
        }
        Ok((stats, dice))
    }
}

#[async_trait]
impl FileWatcher for NotifyFileWatcher {
    async fn sync(
        &self,
        dice: DiceTransactionUpdater,
    ) -> buck2_error::Result<(DiceTransactionUpdater, Mergebase)> {
        span_async(
            buck2_data::FileWatcherStart {
                provider: buck2_data::FileWatcherProvider::RustNotify as i32,
            },
            async {
                let (stats, res) = match self.sync2(dice) {
                    Ok((stats, dice)) => {
                        let mergebase = Mergebase(Arc::new(stats.branched_from_revision.clone()));
                        ((Some(stats)), Ok((dice, mergebase)))
                    }
                    Err(e) => (None, Err(e)),
                };
                (res, buck2_data::FileWatcherEnd { stats })
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::cells::name::CellName;
    use buck2_core::fs::project_rel_path::ProjectRelativePath;
    use buck2_fs::fs_util::uncategorized as fs_util;
    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
    use notify::event::CreateKind;
    use notify::event::Flag;

    use super::*;

    fn fixture() -> (
        ProjectRoot,
        CellResolver,
        StdBuckHashMap<CellName, IgnoreSet>,
        tempfile::TempDir,
    ) {
        let cells = CellResolver::testing_with_name_and_path(
            CellName::testing_new("root"),
            CellRootPathBuf::testing_new(""),
        );
        let tempdir = tempfile::tempdir().unwrap();
        let root_path =
            fs_util::canonicalize(AbsNormPathBuf::new(tempdir.path().to_owned()).unwrap()).unwrap();
        let root = ProjectRoot::new(root_path).unwrap();
        (root, cells, StdBuckHashMap::default(), tempdir)
    }

    /// The kernel-overflow signal can arrive as a rescan-flagged event with no paths attached
    /// (platform dependent). The flag must still be recorded, otherwise lost notifications go
    /// undetected and the daemon keeps answering from a stale graph, which is the cache
    /// inconsistency the missed-events handling exists to close.
    #[test]
    fn rescan_event_with_no_paths_sets_missed_events() {
        let (root, cells, ignores, _t) = fixture();
        let mut state = NotifyFileData::new();
        let event = notify::Event::new(EventKind::Any).set_flag(Flag::Rescan);
        state.process(Ok(event), &root, &cells, &ignores).unwrap();
        assert!(
            state.missed_events,
            "a pathless rescan event must set missed_events"
        );
    }

    #[test]
    fn rescan_event_with_paths_sets_missed_events() {
        let (root, cells, ignores, _t) = fixture();
        let mut state = NotifyFileData::new();
        let path = root.resolve(ProjectRelativePath::new("f").unwrap());
        let event = notify::Event::new(EventKind::Create(CreateKind::File))
            .add_path(path.into_abs_path_buf().into_path_buf())
            .set_flag(Flag::Rescan);
        state.process(Ok(event), &root, &cells, &ignores).unwrap();
        assert!(state.missed_events);
    }

    /// Missed events: the sync result carries no tracker (the caller drops the graph) and the
    /// stats surface the wipe the same way watchman's fresh-instance path does.
    #[test]
    fn sync_with_missed_events_reports_fresh_instance_and_drops_changes() {
        let mut state = NotifyFileData::new();
        state.missed_events = true;
        let (stats, changes) = state.sync();
        assert!(changes.is_none(), "missed events must drop the tracker");
        assert!(stats.fresh_instance);
        let fresh = stats
            .fresh_instance_data
            .expect("fresh instance data populated");
        assert!(fresh.cleared_dice);
        assert!(stats.incomplete_events_reason.is_some());
    }

    #[test]
    fn sync_without_missed_events_returns_changes() {
        let (stats, changes) = NotifyFileData::new().sync();
        assert!(
            changes.is_some(),
            "no missed events: incremental tracker must survive"
        );
        assert!(!stats.fresh_instance);
        assert!(stats.incomplete_events_reason.is_none());
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    #[cfg(target_os = "linux")]
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(target_os = "linux")]
    use std::thread::sleep;
    #[cfg(target_os = "linux")]
    use std::time::Duration;
    #[cfg(target_os = "linux")]
    use std::time::Instant;

    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::fs::project::ProjectRootTemp;
    #[cfg(target_os = "linux")]
    use buck2_fs::fs_util::uncategorized as fs_util;
    #[cfg(target_os = "linux")]
    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

    use super::*;

    fn process_path(
        fs: &ProjectRootTemp,
        rel: impl AsRef<Path>,
    ) -> buck2_error::Result<NotifyFileData> {
        let cells = CellResolver::testing_with_name_and_path(
            CellName::testing_new("root"),
            CellRootPathBuf::testing_new(""),
        );
        let mut ignore_specs = StdBuckHashMap::default();
        ignore_specs.insert(
            CellName::testing_new("root"),
            IgnoreSet::from_ignore_spec("ignored", true)?,
        );

        let event = notify::Event::new(EventKind::Create(CreateKind::File))
            .add_path(fs.path().root().as_path().join(rel.as_ref()));
        let mut data = NotifyFileData::new();
        data.process(Ok(event), fs.path(), &cells, &ignore_specs)?;
        Ok(data)
    }

    #[test]
    fn test_ignores_apply_before_path_validation() -> buck2_error::Result<()> {
        let fs = ProjectRootTemp::new()?;

        // A regular event is recorded.
        let data = process_path(&fs, "src/file")?;
        assert_eq!(1, data.events.len());
        assert_eq!(0, data.ignored);

        // Ignored paths are discarded even when they contain components
        // `ProjectRelativePath` rejects, such as a literal backslash.
        let data = process_path(&fs, r"buck-out/foo\bar")?;
        assert_eq!(0, data.events.len());
        assert_eq!(0, data.ignored);

        let data = process_path(&fs, r"ignored/foo\bar")?;
        assert_eq!(0, data.events.len());
        assert_eq!(1, data.ignored);

        Ok(())
    }

    #[test]
    fn test_unrepresentable_paths_never_error() -> buck2_error::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let fs = ProjectRootTemp::new()?;

        // An unrepresentable name outside any ignored directory degrades to a
        // change of the nearest representable parent directory instead of
        // erroring, like the watchman and edenfs watchers.
        let data = process_path(&fs, r"src/foo\bar")?;
        assert_eq!(0, data.ignored);
        let (cell_path, kind) = data.events.iter().next().unwrap();
        assert_eq!("root//src", cell_path.to_string());
        assert_eq!(EventKind::Create(CreateKind::Folder), *kind);

        // Same for non-UTF-8 names, which cannot even be relativized.
        let non_utf8 = OsStr::from_bytes(b"foo\xff");
        let data = process_path(&fs, Path::new("src").join(non_utf8))?;
        assert_eq!(0, data.ignored);
        let (cell_path, kind) = data.events.iter().next().unwrap();
        assert_eq!("root//src", cell_path.to_string());
        assert_eq!(EventKind::Create(CreateKind::Folder), *kind);

        // Under buck-out and ignored directories they are discarded like any
        // other path there.
        let data = process_path(&fs, Path::new("buck-out").join(non_utf8))?;
        assert_eq!(0, data.events.len());
        assert_eq!(0, data.ignored);

        let data = process_path(&fs, Path::new("ignored").join(non_utf8))?;
        assert_eq!(0, data.events.len());
        assert_eq!(1, data.ignored);

        Ok(())
    }

    /// Wait for the inotify thread to catch up with what the test did.
    #[cfg(target_os = "linux")]
    fn wait_for(watcher: &NotifyFileWatcher, done: impl Fn(&NotifyFileData) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(data) = &*watcher.data.lock().unwrap() {
                if done(data) {
                    return true;
                }
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    /// An inotify watch that could not be installed has to leave the daemon knowing that its
    /// coverage is incomplete, so that the next sync clears DICE and registers the tree again.
    /// FSEvents does not install a separate watch for each directory in a recursive tree.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_watch_counts_as_missed_events() {
        let tempdir = tempfile::tempdir().unwrap();
        let project = tempdir.path().join("project");
        let staging = tempdir.path().join("staging");
        fs::create_dir(&project).unwrap();
        fs::create_dir_all(staging.join("readable")).unwrap();
        let unwatchable = staging.join("unwatchable");
        fs::create_dir(&unwatchable).unwrap();
        fs::set_permissions(&unwatchable, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&unwatchable).is_ok() {
            return; // running as root, which can watch a directory it cannot read
        }

        let root = ProjectRoot::new(
            fs_util::canonicalize(AbsNormPathBuf::new(project.clone()).unwrap()).unwrap(),
        )
        .unwrap();
        let cells = CellResolver::testing_with_name_and_path(
            CellName::testing_new("root"),
            CellRootPathBuf::testing_new(""),
        );
        let watcher = NotifyFileWatcher::new(&root, cells, StdBuckHashMap::default()).unwrap();

        // Moved in whole, so the walk it triggers is certain to meet the unwatchable directory.
        let appearing = project.join("appearing");
        fs::rename(&staging, &appearing).unwrap();
        let missed = wait_for(&watcher, |data| data.missed_events);
        // Before the asserts: a directory the test cannot read is one tempfile cannot remove.
        fs::set_permissions(
            appearing.join("unwatchable"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(
            missed,
            "expected the failed watch to count as missed events"
        );

        // Registering again is what buys back the coverage the failure cost us.
        watcher.reregister().unwrap();
        fs::write(appearing.join("readable").join("file"), "x").unwrap();
        assert!(
            wait_for(&watcher, |data| data
                .events
                .iter()
                .any(|(path, _)| path.to_string().ends_with("file"))),
            "expected a change under the sibling of the unwatchable directory to be seen"
        );
    }

    #[test]
    fn test_watch_filter_prunes_buck_out_and_ignored() -> buck2_error::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let fs = ProjectRootTemp::new()?;
        let root_path = fs.path().root().as_path();
        let cells = CellResolver::testing_with_name_and_path(
            CellName::testing_new("root"),
            CellRootPathBuf::testing_new(""),
        );
        let mut specs = StdBuckHashMap::default();
        specs.insert(
            CellName::testing_new("root"),
            IgnoreSet::from_ignore_spec("**/node_modules", true)?,
        );
        let filter = ignore_watch_filter(fs.path(), &cells, &specs);

        assert!(filter.allows_dir(root_path));
        assert!(filter.allows_dir(&root_path.join("src")));
        assert!(!filter.allows_dir(&root_path.join("buck-out")));
        assert!(!filter.allows_dir(&root_path.join("buck-out/v2/gen")));
        assert!(!filter.allows_dir(&root_path.join("src/node_modules")));
        assert!(filter.allows_dir(&root_path.join("src/node_modules_not")));

        // Unrepresentable names are pruned too: buck cannot read them.
        assert!(!filter.allows_dir(&root_path.join(r"back\slash")));
        assert!(!filter.allows_dir(&root_path.join(OsStr::from_bytes(b"foo\xff"))));
        Ok(())
    }
}
