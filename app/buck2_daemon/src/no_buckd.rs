/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_client_ctx::daemon::client::BuckdLifecycleLock;
use buck2_client_ctx::daemon::client::connect::buckd_startup_timeout;
use buck2_client_ctx::daemon::client::kill::kill_command_impl;
use buck2_client_ctx::startup_deadline::StartupDeadline;
use buck2_common::init::DaemonStartupConfig;
use buck2_common::invocation_paths::InvocationPaths;
use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
use buck2_core::logging::LogConfigurationReloadHandle;
use buck2_error::buck2_error;
use buck2_util::threads::thread_spawn;

use crate::daemon::DaemonCommand;

pub fn start_in_process_daemon(
    daemon_startup_config: &DaemonStartupConfig,
    paths: InvocationPaths,
    runtime: &tokio::runtime::Runtime,
) -> buck2_error::Result<Option<Box<dyn FnOnce() -> buck2_error::Result<()> + Send + Sync>>> {
    let daemon_dir = paths.daemon_dir()?;
    let hold_lifecycle_lock =
        daemon_startup_config.cell_execution_paths == CellSourcePathMode::CanonicalV1;
    if !hold_lifecycle_lock {
        // Physical-mode timing: acquire/kill/release while the client context is constructed,
        // before command dispatch.
        runtime.block_on(async {
            let lifecycle_lock = BuckdLifecycleLock::lock_with_timeout(
                daemon_dir.clone(),
                StartupDeadline::duration_from_now(buckd_startup_timeout()?)?,
            )
            .await?;
            kill_command_impl(&lifecycle_lock, "A command with `--no-buckd` is invoked").await
        })?;
    }
    let daemon_dir = hold_lifecycle_lock.then_some(daemon_dir);
    let runtime_handle = runtime.handle().clone();
    let daemon_startup_config = daemon_startup_config.clone();
    // Create a function which spawns an in-process daemon.
    Ok(Some(Box::new(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        // Spawn a thread which runs the daemon.
        thread_spawn("buck2-no-buckd", move || {
            // Acquire only when this closure is actually used by a streaming command: client-only
            // commands such as `clean` and `kill` keep their own single-lock lifecycle paths.
            let lifecycle_lock = match daemon_dir {
                Some(daemon_dir) => runtime_handle.block_on(async move {
                    let lifecycle_lock = BuckdLifecycleLock::lock_with_timeout(
                        daemon_dir,
                        StartupDeadline::duration_from_now(buckd_startup_timeout()?)?,
                    )
                    .await?;
                    kill_command_impl(&lifecycle_lock, "A command with `--no-buckd` is invoked")
                        .await?;
                    buck2_error::Ok(Some(lifecycle_lock))
                }),
                None => Ok(None),
            };
            let lifecycle_lock = match lifecycle_lock {
                Ok(lock) => lock,
                Err(error) => {
                    drop(tx.send(Err(error)));
                    return;
                }
            };

            // Canonical mode holds the lock until the client process exits, not merely until the
            // daemon thread returns: a graceful stop can precede process exit, and no concurrent
            // command may replace the execution view while this process can still use it. Leaking
            // the guard is sound because the OS releases the flock at exit, even on SIGKILL.
            if let Some(lifecycle_lock) = lifecycle_lock {
                std::mem::forget(lifecycle_lock);
            }
            let tx_clone = tx.clone();
            let result = DaemonCommand::new_in_process(daemon_startup_config).exec(
                <dyn LogConfigurationReloadHandle>::noop(),
                paths,
                true,
                move || drop(tx_clone.send(Ok(()))),
            );
            // Since `tx` is unbounded, there's race here: it is possible
            // that error message will be lost in the channel and not reported anywhere.
            // Not an issue practically, because daemon does not usually error
            // after it started listening.
            if let Err(e) = tx.send(result) {
                match e.0 {
                    Ok(()) => drop(buck2_client_ctx::eprintln!(
                        "In-process daemon gracefully stopped"
                    )),
                    Err(e) => drop(buck2_client_ctx::eprintln!(
                        "In-process daemon run failed: {:#}",
                        e
                    )),
                }
            }
        })?;
        // Wait for listener to start (or to fail).
        match rx.recv() {
            Ok(r) => r,
            Err(_) => Err(buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "In-process daemon failed to start and we don't know why"
            )),
        }
    })))
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use buck2_common::daemon_dir::DaemonDir;
    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

    use super::*;

    /// The leak above only excludes other daemons because the lock is an OS flock. Were it ever to
    /// become `Drop`- or pidfile-based, the leak would silently stop excluding anything.
    #[tokio::test]
    async fn leaked_lifecycle_lock_still_excludes_another_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let daemon_dir = DaemonDir {
            path: AbsNormPathBuf::new(temp.path().join("daemon")).unwrap(),
        };
        let lock = BuckdLifecycleLock::lock_with_timeout(
            daemon_dir.clone(),
            StartupDeadline::duration_from_now(Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        std::mem::forget(lock);

        let second = BuckdLifecycleLock::lock_with_timeout(
            daemon_dir,
            StartupDeadline::duration_from_now(Duration::from_millis(25)).unwrap(),
        )
        .await;
        assert!(
            second.is_err(),
            "a leaked canonical --no-buckd lock must still exclude another daemon start"
        );
    }
}
