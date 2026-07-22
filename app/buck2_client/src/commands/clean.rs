/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use buck2_client_ctx::client_ctx::BuckSubcommand;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::BuckArgMatches;
use buck2_client_ctx::common::CommonCommandOptions;
use buck2_client_ctx::common::CommonEventLogOptions;
use buck2_client_ctx::common::target_cfg::TargetCfgUnusedOptions;
use buck2_client_ctx::common::ui::ConsoleType;
use buck2_client_ctx::daemon::client::BuckdLifecycleLock;
use buck2_client_ctx::daemon::client::kill::kill_command_impl;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::final_console::FinalConsole;
use buck2_client_ctx::startup_deadline::StartupDeadline;
use buck2_client_ctx::subscribers::superconsole::StatefulSuperConsole;
use buck2_common::daemon_dir::DaemonDir;
use buck2_error::BuckErrorContext;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::abs_path::AbsPath;
use dupe::Dupe;
use gazebo::prelude::SliceExt;
use superconsole::Line;
use superconsole::SuperConsole;
use superconsole::components::Spinner;
use threadpool::ThreadPool;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::commands::clean_stale::CleanStaleCommand;
use crate::commands::clean_stale::parse_clean_stale_args;

/// Delete generated files and caches.
///
/// The command also kills the buck2 daemon.
#[derive(Debug, clap::Parser)]
pub struct CleanCommand {
    #[clap(
        long = "dry-run",
        help = "Performs a dry-run and prints the paths that would be removed."
    )]
    dry_run: bool,

    #[clap(
        long = "stale",
        help = "Delete artifacts from buck-out older than 1 week or older than
the specified duration, without killing the daemon",
        value_name = "DURATION"
    )]
    stale: Option<Option<humantime::Duration>>,

    // Like stale but since a specific timestamp, for testing
    #[clap(long = "keep-since-time", conflicts_with = "stale", hide = true)]
    keep_since_time: Option<i64>,

    /// Only considers tracked artifacts for cleanup.
    ///
    /// `buck-out` can contain untracked artifacts for different reasons:
    ///  - Outputs from aborted actions
    ///  - State getting deleted (e.g., new buckversion that changes the on-disk state format)
    ///  - Writing to `buck-out` without being expected by Buck
    #[clap(long = "tracked-only", requires = "stale")]
    tracked_only: bool,

    /// Enable adaptive low-disk promotion: after the regular stale scan,
    /// promote retained, non-active artifacts (oldest-access first) to stale
    /// until projected free disk % rises above this threshold (0.0 - 100.0).
    #[clap(
        long = "adaptive-low-disk-threshold",
        value_name = "PERCENT",
        requires = "stale"
    )]
    adaptive_low_disk_threshold: Option<f64>,

    /// Minimum TTL floor for adaptive low-disk promotion: retained artifacts
    /// accessed within this duration of now are never promoted, even under
    /// disk pressure. Ignored unless `--adaptive-low-disk-threshold` is set.
    #[clap(
        long = "adaptive-min-ttl",
        value_name = "DURATION",
        requires = "adaptive_low_disk_threshold",
        default_value = "12h"
    )]
    adaptive_min_ttl: humantime::Duration,

    #[clap(
        long = "background",
        help = "Run the clean operation in the background"
    )]
    background: bool,

    /// Command doesn't need these flags, but they are used in mode files, so we need to keep them.
    #[clap(flatten)]
    _target_cfg: TargetCfgUnusedOptions,

    #[clap(flatten)]
    common_opts: CommonCommandOptions,
}

impl CleanCommand {
    pub fn exec(
        self,
        matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        if let Some(keep_since_arg) = parse_clean_stale_args(self.stale, self.keep_since_time)? {
            if let Some(t) = self.adaptive_low_disk_threshold {
                if !(0.0..=100.0).contains(&t) || t.is_nan() {
                    return ExitResult::bail(format!(
                        "`--adaptive-low-disk-threshold` must be between 0.0 and 100.0, got `{t}`"
                    ));
                }
            }
            let cmd = CleanStaleCommand {
                common_opts: self.common_opts,
                keep_since_arg,
                dry_run: self.dry_run,
                tracked_only: self.tracked_only,
                adaptive_low_disk_threshold: self.adaptive_low_disk_threshold,
                adaptive_min_ttl: Some(self.adaptive_min_ttl.into()),
            };
            ctx.exec(cmd, matches, events_ctx)
        } else {
            ctx.exec(
                InnerCleanCommand {
                    dry_run: self.dry_run,
                    background: self.background,
                    common_opts: self.common_opts,
                },
                matches,
                events_ctx,
            )
        }
    }

    pub fn command_name(&self) -> &'static str {
        if let Ok(Some(_)) = parse_clean_stale_args(self.stale, self.keep_since_time) {
            "clean-stale"
        } else {
            "clean"
        }
    }
}

struct InnerCleanCommand {
    dry_run: bool,
    background: bool,
    common_opts: CommonCommandOptions,
}

impl BuckSubcommand for InnerCleanCommand {
    const COMMAND_NAME: &'static str = "clean";

    async fn exec_impl(
        self,
        _matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        _events_ctx: &mut buck2_client_ctx::events_ctx::EventsCtx,
    ) -> ExitResult {
        let paths = ctx.paths()?;
        let buck_out_dir = paths.buck_out_path();
        let daemon_dir = paths.daemon_dir()?;
        let trash_dir = paths.trash_dir();
        let console = &self.common_opts.console_opts.final_console();

        if self.dry_run {
            return clean(
                buck_out_dir,
                daemon_dir,
                trash_dir,
                console,
                self.common_opts.console_opts.console_type,
                None,
                self.background,
            )
            .await
            .into();
        }

        // Kill the daemon and make sure a new daemon does not spin up while we're performing clean up operations
        // This will ensure we have exclusive access to the directories in question
        let lifecycle_lock = BuckdLifecycleLock::lock_with_timeout(
            daemon_dir.clone(),
            StartupDeadline::duration_from_now(Duration::from_secs(10))?,
        )
        .await?;

        kill_command_impl(&lifecycle_lock, "`buck2 clean` was invoked").await?;

        clean(
            buck_out_dir,
            daemon_dir,
            trash_dir,
            console,
            self.common_opts.console_opts.console_type,
            Some(&lifecycle_lock),
            self.background,
        )
        .await
        .into()
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        &self.common_opts.event_log_opts
    }
}

async fn clean(
    buck_out_dir: AbsNormPathBuf,
    daemon_dir: DaemonDir,
    trash_dir: AbsNormPathBuf,
    console: &FinalConsole,
    console_type: ConsoleType,
    // None means "dry run".
    lifecycle_lock: Option<&BuckdLifecycleLock>,
    background: bool,
) -> buck2_error::Result<()> {
    let paths_to_clean = if background {
        let trash_uuid = Uuid::new_v4();
        let trash_target = trash_dir.as_abs_path().join(trash_uuid.to_string());

        // Create trash directory if it doesn't exist
        if !trash_dir.exists() {
            fs_util::create_dir_all(&trash_dir)?;
        }

        // Move buck-out to trash folder
        if buck_out_dir.exists() {
            console.print_stderr(&format!(
                "Moving {} to {}",
                buck_out_dir.display(),
                trash_target.display()
            ))?;
            fs_util::rename(&buck_out_dir, &trash_target).categorize_internal()?;
        }

        // Clean the daemon_dir first
        let mut paths_to_clean = Vec::new();
        if daemon_dir.path.exists() {
            paths_to_clean.push(daemon_dir.to_string());
            if let Some(lifecycle_lock) = lifecycle_lock {
                lifecycle_lock.clean_daemon_dir(false)?;
            }
        }

        console.print_stderr("Buck-out moved to trash. Now cleaning up...")?;
        console.print_stderr(
            "Tip: Use Ctrl-Z to put this in the background, or run in a new terminal.",
        )?;
        console.print_stderr("You can run other buck2 commands while this completes.")?;

        // Delete the moved directory
        let trash_target_normalized = AbsNormPathBuf::new(trash_target.to_path_buf())?;
        if trash_target_normalized.exists() {
            paths_to_clean.extend(
                collect_paths_to_clean(&trash_target_normalized)?
                    .map(|path| path.display().to_string()),
            );
            tokio::task::spawn_blocking(move || {
                clean_buck_out_with_retry(&trash_target_normalized, console_type)
            })
            .await?
            .buck_error_context("Failed to spawn clean")?;
        }
        paths_to_clean
    } else {
        let mut paths_to_clean = Vec::new();

        if buck_out_dir.exists() {
            paths_to_clean =
                collect_paths_to_clean(&buck_out_dir)?.map(|path| path.display().to_string());
            if lifecycle_lock.is_some() {
                tokio::task::spawn_blocking(move || {
                    clean_buck_out_with_retry(&buck_out_dir, console_type)
                })
                .await?
                .buck_error_context("Failed to spawn clean")?;
            }
        }

        if daemon_dir.path.exists() {
            paths_to_clean.push(daemon_dir.to_string());
            if let Some(lifecycle_lock) = lifecycle_lock {
                lifecycle_lock.clean_daemon_dir(false)?;
            }
        }

        paths_to_clean
    };

    if paths_to_clean.is_empty() {
        console.print_stderr("Nothing to clean.")?;
    }
    for path in paths_to_clean {
        console.print_stderr(&path)?;
    }

    Ok(())
}

fn collect_paths_to_clean(
    buck_out_path: &AbsNormPathBuf,
) -> buck2_error::Result<Vec<AbsNormPathBuf>> {
    if !buck_out_path.exists() {
        return Ok(vec![]);
    }
    let mut paths_to_clean = vec![];
    let dir = fs_util::read_dir(buck_out_path).categorize_internal()?;
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        paths_to_clean.push(path);
    }

    Ok(paths_to_clean)
}

/// In Windows, we've observed the buck-out clean immediately after killing
/// the daemon can fail with this error: `The process cannot access the
/// file because it is being used by another process.`. To get around this,
/// add a single retry.
fn clean_buck_out_with_retry(
    path: &AbsNormPathBuf,
    console_type: ConsoleType,
) -> buck2_error::Result<()> {
    let mut result = clean_buck_out(path, console_type);
    match result {
        Ok(_) => {
            return result;
        }
        Err(e) => {
            tracing::info!(
                "Retrying buck-out clean, first attempted failed with: {:#}",
                e
            );
            result = clean_buck_out(path, console_type);
        }
    }
    result
}

/// State shared between the progress display and the file deletion threads.
struct CleanProgressState {
    files_deleted: Arc<AtomicUsize>,
    start_time: Instant,
}

impl CleanProgressState {
    fn new() -> Self {
        Self {
            files_deleted: Arc::new(AtomicUsize::new(0)),
            start_time: Instant::now(),
        }
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        self.files_deleted.dupe()
    }

    fn files_deleted(&self) -> usize {
        self.files_deleted.load(Ordering::Relaxed)
    }

    fn format_message(&self) -> Line {
        let elapsed = Instant::now() - self.start_time;
        Line::sanitized(&format!(
            "Cleaning buck-out: {} files deleted ({}s)",
            self.files_deleted(),
            elapsed.as_secs()
        ))
    }

    fn format_final_message(&self) -> Line {
        let elapsed = Instant::now() - self.start_time;
        Line::sanitized(&format!(
            "Cleaned {} files in {:.1}s",
            self.files_deleted(),
            elapsed.as_secs_f64()
        ))
    }
}

/// Runs the progress display loop using superconsole.
fn run_superconsole_progress(
    mut console: SuperConsole,
    state: &CleanProgressState,
    stop: impl Fn() -> bool,
) {
    let mut tick = 0;
    while !stop() {
        let spinner = Spinner::new(tick, state.format_message());
        if console.render(&spinner).is_err() {
            break;
        }
        tick += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    // Finalize with the final message (no spinner prefix in Final mode)
    let final_spinner = Spinner::new(tick, state.format_final_message());
    drop(console.finalize(&final_spinner));
}

/// Handle for the superconsole-based progress display.
/// When dropped, it stops the display thread and shows the completion message.
struct CleanProgressHandle {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CleanProgressHandle {
    fn new(state: Arc<CleanProgressState>, console: SuperConsole) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.dupe();

        let handle = std::thread::spawn(move || {
            run_superconsole_progress(console, &state, || stop_flag_clone.load(Ordering::Relaxed));
        });

        Self {
            stop_flag,
            handle: Some(handle),
        }
    }
}

impl Drop for CleanProgressHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

/// Every cell forest must be removed before the `.owners` directory that owns them: an
/// interrupted clean that took them the other way around would leave a forest with no owner
/// record, the one state `Namespace::ensure` refuses to adopt.
fn view_removal_order(mut entries: Vec<PathBuf>) -> Vec<PathBuf> {
    entries.sort_by_key(|entry| entry.file_name().is_some_and(|name| name == ".owners"));
    entries
}

/// Nothing here may follow a link into the physical cell sources: [`is_plain_dir`] rejects reparse
/// points before `read_dir`, and `fs_util::remove_all` unlinks a planted forest rather than
/// descending into it.
fn clean_execution_view(buck_out: &AbsNormPathBuf) -> buck2_error::Result<()> {
    let view_root = buck_out.as_path().join("cell_sources");
    if is_plain_dir(&view_root) {
        for version in read_dir_if_exists(&view_root)? {
            if !is_plain_dir(&version) {
                continue;
            }
            for entry in view_removal_order(read_dir_if_exists(&version)?) {
                fs_util::remove_all(AbsPath::new(&entry)?).categorize_internal()?;
            }
        }
    }
    fs_util::remove_all(AbsPath::new(&view_root)?).categorize_internal()
}

fn read_dir_if_exists(path: &std::path::Path) -> buck2_error::Result<Vec<PathBuf>> {
    match std::fs::read_dir(path) {
        Ok(entries) => Ok(entries
            .map(|entry| Ok(entry?.path()))
            .collect::<Result<Vec<_>, std::io::Error>>()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// `symlink_metadata` is required: a followed stat of a symlink reports a plain directory.
fn is_plain_dir(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| fs_util::is_plain_directory(&metadata))
}

fn clean_buck_out(path: &AbsNormPathBuf, console_type: ConsoleType) -> buck2_error::Result<()> {
    // Canonical cell execution roots are links to the user's checkout, so they must go before the
    // parallel pass, whose `follow_links` is kept explicit for the same reason.
    clean_execution_view(path)?;
    let walk = WalkDir::new(path).follow_links(false);
    let thread_pool = ThreadPool::new(buck2_util::threads::available_parallelism());
    let error = Arc::new(Mutex::new(None));

    let state = Arc::new(CleanProgressState::new());
    let counter = state.counter();

    // Show progress using superconsole, respecting the --console option.
    // Use the same console_builder() as other buck2 commands to ensure consistent behavior.
    let _progress_handle = match console_type {
        ConsoleType::None
        | ConsoleType::Simple
        | ConsoleType::SimpleNoTty
        | ConsoleType::SimpleTty => None,
        ConsoleType::Auto | ConsoleType::Super => StatefulSuperConsole::console_builder()
            .build()
            .ok()
            .flatten()
            .map(|console| CleanProgressHandle::new(state, console)),
    };

    for dir_entry in walk.into_iter().flatten() {
        let file_type = dir_entry.file_type();
        // As in the daemon, heavily parallel writes to directories in btrfs perform really poorly,
        // so we only parallelize file deletions and do the rest synchronously.
        //
        // FIXME(JakobDegen): The parallelism cap for file deletions in the daemon is much smaller
        // than it is here. Change that or write a comment justifying it.
        if !file_type.is_dir() && !file_type.is_symlink() {
            let error = error.dupe();
            let counter = counter.dupe();
            thread_pool.execute(move || {
                // The wlak gives us back absolute paths since we give it absolute paths.
                let res = AbsPath::new(dir_entry.path()).and_then(|p| {
                    fs_util::remove_file(p)
                        .categorize_internal()
                        .map_err(Into::into)
                });

                match res {
                    Ok(_) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let mut error = error.lock().unwrap();
                        if error.is_none() {
                            *error = Some(e);
                        }
                    }
                }
            })
        }
    }

    thread_pool.join();

    // Drop the progress handle to stop the display and show final message
    drop(_progress_handle);

    if let Some(e) = error.lock().unwrap().take() {
        return Err(e);
    }

    // Buck's cwd is typically the directory that is passed in here, which means that on Windows we
    // often fail to delete this if we don't clean up all our child processes. Leaving zombies
    // around isn't great though...
    let dir = fs_util::read_dir(path).categorize_internal()?;
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        fs_util::remove_dir_all(path).categorize_internal()?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    /// Entered at `buck-out/<isolation>`, the shape production passes and the only one under which
    /// `clean_execution_view` finds the view at all.
    #[test]
    fn clean_does_not_follow_canonical_cell_view_link() -> buck2_error::Result<()> {
        let temp = tempfile::tempdir()?;
        let iso_buck_out = temp.path().join("buck-out/v2");
        let generated = iso_buck_out.join("gen/output");
        let owners = iso_buck_out.join("cell_sources/v1/.owners");
        let canonical_cell = iso_buck_out.join("cell_sources/v1/c_73616d706c65");
        let canonical_entry = canonical_cell.join("src");
        let physical_cell = temp.path().join("physical-sample");
        let physical_source = physical_cell.join("src/source.cpp");

        fs::create_dir_all(generated.parent().unwrap())?;
        fs::write(&generated, b"generated")?;
        fs::create_dir_all(&canonical_cell)?;
        fs::create_dir_all(&owners)?;
        fs::write(owners.join("c_73616d706c65"), b"owner")?;
        // Leftover record for a vanished cell.
        fs::write(owners.join("c_6f6c64"), b"stale owner")?;
        fs::create_dir_all(physical_source.parent().unwrap())?;
        fs::write(&physical_source, b"source")?;
        symlink(physical_cell.join("src"), &canonical_entry)?;

        clean_buck_out(
            &AbsNormPathBuf::new(iso_buck_out.clone())?,
            ConsoleType::None,
        )?;

        assert!(
            physical_source.exists(),
            "planted link must not be followed"
        );
        assert!(!generated.exists());
        assert!(!iso_buck_out.join("cell_sources").exists());
        Ok(())
    }

    #[test]
    fn view_removal_order_puts_owners_last() {
        for entries in [
            vec![".owners", "c_73616d706c65", "c_6f6c64"],
            vec!["c_73616d706c65", ".owners", "c_6f6c64"],
            vec!["c_73616d706c65", "c_6f6c64", ".owners"],
        ] {
            let ordered = view_removal_order(
                entries
                    .iter()
                    .map(|name| PathBuf::from("v1").join(name))
                    .collect(),
            );
            assert_eq!(
                ordered.last().unwrap().file_name().unwrap(),
                ".owners",
                "input order {entries:?} must still remove `.owners` last"
            );
            assert_eq!(ordered.len(), entries.len(), "no entry may be dropped");
        }
    }

    #[test]
    fn clean_execution_view_removes_forests_before_owners() -> buck2_error::Result<()> {
        let temp = tempfile::tempdir()?;
        let iso_buck_out = temp.path().join("buck-out/v2");
        let version = iso_buck_out.join("cell_sources/v1");
        let owners = version.join(".owners");
        let forest = version.join("c_73616d706c65");

        fs::create_dir_all(&owners)?;
        fs::create_dir_all(&forest)?;
        fs::write(owners.join("c_73616d706c65"), b"owner")?;
        fs::write(forest.join("child"), b"x")?;

        // Standing in for an interrupted clean. The read-only bit goes on the parent because
        // `remove_dir_all`'s permission retry only chmods the tree it was handed.
        let restore = fs::metadata(&version)?.permissions();
        let mut readonly = restore.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&version, readonly)?;
        // Root ignores the permission bits this relies on.
        if fs::write(version.join(".probe"), b"").is_ok() {
            fs::set_permissions(&version, restore)?;
            return Ok(());
        }

        let result = clean_execution_view(&AbsNormPathBuf::new(iso_buck_out.clone())?);
        let owner_survived = owners.join("c_73616d706c65").exists();
        fs::set_permissions(&version, restore)?;

        assert!(result.is_err(), "removal should have failed on the forest");
        assert!(
            owner_survived,
            "the owner record must outlive the forest it owns; recovery is another `buck2 clean`, \
             which is useless once the daemon is left refusing to adopt an unowned forest"
        );
        Ok(())
    }

    #[test]
    fn clean_execution_view_does_not_follow_namespace_root_link() -> buck2_error::Result<()> {
        let temp = tempfile::tempdir()?;
        let iso_buck_out = temp.path().join("buck-out/v2");
        let target = temp.path().join("victim");
        let target_file = target.join("v1/c_73616d706c65/precious.txt");

        // The victim needs a real view's shape (`<root>/v1/c_*/file`), or a regression that
        // follows the link would find nothing to delete.
        fs::create_dir_all(target_file.parent().unwrap())?;
        fs::create_dir_all(&iso_buck_out)?;
        fs::write(&target_file, b"precious")?;
        symlink(&target, iso_buck_out.join("cell_sources"))?;

        clean_execution_view(&AbsNormPathBuf::new(iso_buck_out.clone())?)?;

        assert!(target_file.exists());
        assert!(
            fs::symlink_metadata(iso_buck_out.join("cell_sources")).is_err(),
            "the planted link itself must be removed"
        );
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::fs;

    use super::*;

    /// Mirrors the unix `clean_does_not_follow_canonical_cell_view_link`. The junction sits inside
    /// a forest, below anything `is_plain_dir` inspects, so this covers `remove_dir_all` unlinking
    /// it rather than descending.
    #[test]
    fn clean_does_not_follow_canonical_cell_view_junction() -> buck2_error::Result<()> {
        let temp = tempfile::tempdir()?;
        let iso_buck_out = temp.path().join("buck-out/v2");
        let generated = iso_buck_out.join("gen/output");
        let owners = iso_buck_out.join("cell_sources/v1/.owners");
        let canonical_cell = iso_buck_out.join("cell_sources/v1/c_73616d706c65");
        let canonical_entry = canonical_cell.join("src");
        let physical_cell = temp.path().join("physical-sample");
        let physical_source = physical_cell.join("src/source.cpp");

        fs::create_dir_all(generated.parent().unwrap())?;
        fs::write(&generated, b"generated")?;
        fs::create_dir_all(&canonical_cell)?;
        fs::create_dir_all(&owners)?;
        fs::write(owners.join("c_73616d706c65"), b"owner")?;
        fs::write(owners.join("c_6f6c64"), b"stale owner")?;
        fs::create_dir_all(physical_source.parent().unwrap())?;
        fs::write(&physical_source, b"source")?;
        junction::create(physical_cell.join("src"), &canonical_entry)?;

        clean_buck_out(
            &AbsNormPathBuf::new(iso_buck_out.clone())?,
            ConsoleType::None,
        )?;

        assert!(
            physical_source.exists(),
            "the planted junction must be unlinked, not descended into"
        );
        assert!(!generated.exists());
        assert!(!iso_buck_out.join("cell_sources").exists());
        Ok(())
    }

    /// A junction at the namespace root, the case that goes through `is_plain_dir`.
    #[test]
    fn clean_execution_view_does_not_follow_namespace_root_junction() -> buck2_error::Result<()> {
        let temp = tempfile::tempdir()?;
        let iso_buck_out = temp.path().join("buck-out/v2");
        let target = temp.path().join("victim");
        let target_file = target.join("v1/c_73616d706c65/precious.txt");

        fs::create_dir_all(target_file.parent().unwrap())?;
        fs::create_dir_all(&iso_buck_out)?;
        fs::write(&target_file, b"precious")?;
        junction::create(&target, iso_buck_out.join("cell_sources"))?;

        clean_execution_view(&AbsNormPathBuf::new(iso_buck_out.clone())?)?;

        assert!(target_file.exists());
        assert!(
            fs::symlink_metadata(iso_buck_out.join("cell_sources")).is_err(),
            "the planted junction itself must be removed"
        );
        Ok(())
    }
}
