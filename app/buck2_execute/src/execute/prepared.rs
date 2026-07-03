/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ops::ControlFlow;
use std::sync::Arc;

use async_trait::async_trait;
use buck2_core::execution_types::executor_config::ReGangWorker;
use buck2_core::execution_types::executor_config::RemoteExecutorDependency;
use buck2_data::NetworkAccess;
use buck2_hash::BuckMutInsertionMap;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use remote_execution as RE;

use crate::artifact_value::ArtifactValue;
use crate::digest_config::DigestConfig;
use crate::execute::action_digest::ActionDigest;
use crate::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use crate::execute::manager::CommandExecutionManager;
use crate::execute::request::CommandExecutionOutput;
use crate::execute::request::CommandExecutionRequest;
use crate::execute::request::ExecutorPreference;
use crate::execute::result::CommandExecutionResult;
use crate::execute::target::CommandExecutionTarget;
use crate::materialize::materializer::CasDownloadInfo;
use crate::materialize::materializer::DeclareArtifactPayload;

pub struct PreparedAction {
    pub action_and_blobs: ActionDigestAndBlobs,
    pub platform: RE::Platform,
    pub remote_execution_dependencies: Vec<RemoteExecutorDependency>,
    pub re_gang_workers: Vec<ReGangWorker>,
    pub worker_tool_init_action: Option<ActionDigestAndBlobs>,
    pub network_access: Option<NetworkAccess>,
}

impl PreparedAction {
    pub fn digest(&self) -> ActionDigest {
        self.action_and_blobs.action.dupe()
    }
}

pub struct PreparedCommand<'a, 'b> {
    pub request: &'a CommandExecutionRequest,
    pub target: &'b dyn CommandExecutionTarget,
    pub prepared_action: &'a PreparedAction,
    pub digest_config: DigestConfig,
}

#[async_trait]
pub trait PreparedCommandExecutor: Send + Sync {
    /// Execute a command.
    ///
    /// This intentionally does not return a Result since we want to capture information about the
    /// execution even if there are errors. Any errors can be propagated by converting them
    /// to a result with CommandExecutionManager::error.
    async fn exec_cmd(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> CommandExecutionResult;

    /// Checks if there is any possibility for a command with a given executor preference to
    /// be executed locally.
    fn is_local_execution_possible(&self, executor_preference: ExecutorPreference) -> bool;
}

#[async_trait]
pub trait PreparedCommandOptionalExecutor: Send + Sync {
    /// Take a command and evaluate whether it needs to be actually executed (locally or remotely) or can be skipped.
    /// In the skip case, this should handle all the things that would happen in an actual execution (materialization, output declaration, etc.).
    ///
    /// Given a command, evaluate whether the execution can be skipped. (for example because it is already cached)
    /// If it can be skipped, return a CommandExecutionResult that can be used as if the action was just executed.
    /// Otherwise, return a CommandExecutionManager that can be used to execute the action.
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager>;

    /// buck2 looks up output values and lookup-first actions only when this returns `true`.
    fn lookups_can_hit(&self) -> bool {
        false
    }

    /// Returns the output values that the digests-only entry of `command` records, or `None` when
    /// the cache holds no entry for it. Unlike `maybe_execute`, it declares nothing in the
    /// materializer, because the CAS may not hold the output files of `command`.
    async fn look_up_digests_only(
        &self,
        _command: &PreparedCommand<'_, '_>,
    ) -> buck2_error::Result<Option<BuckMutInsertionMap<CommandExecutionOutput, ArtifactValue>>>
    {
        Ok(None)
    }

    /// Looks up the normal entry of `command` like `maybe_execute`, but declares nothing in the
    /// materializer, so that the caller can check the output values first. It uploads nothing,
    /// because the input values of `command` can name files that neither the disk nor the CAS
    /// holds. Returns `None` when the cache holds no successful result for `command`.
    async fn look_up_normal(
        &self,
        _command: &PreparedCommand<'_, '_>,
        _manager: CommandExecutionManager,
    ) -> buck2_error::Result<Option<ActionCacheHit>> {
        Ok(None)
    }
}

/// The outputs of an `ActionCacheHit` aren't declared in the materializer yet.
pub struct ActionCacheHit {
    pub result: CommandExecutionResult,
    pub stage: buck2_data::CacheHit,
    pub info: CasDownloadInfo,
    pub to_declare: Vec<DeclareArtifactPayload>,
    pub executor_preference: ExecutorPreference,
    pub input_files_bytes: u64,
    /// The cache doesn't know the `allow_dep_file_cache_upload` of the action, so `look_up_normal`
    /// returns false. The caller of `look_up_normal` sets the value of the action.
    pub allow_dep_file_cache_upload: bool,
}

#[async_trait]
impl PreparedCommandOptionalExecutor for Arc<dyn PreparedCommandOptionalExecutor> {
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        (**self)
            .maybe_execute(command, manager, cancellations)
            .await
    }

    fn lookups_can_hit(&self) -> bool {
        (**self).lookups_can_hit()
    }

    async fn look_up_digests_only(
        &self,
        command: &PreparedCommand<'_, '_>,
    ) -> buck2_error::Result<Option<BuckMutInsertionMap<CommandExecutionOutput, ArtifactValue>>>
    {
        (**self).look_up_digests_only(command).await
    }

    async fn look_up_normal(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
    ) -> buck2_error::Result<Option<ActionCacheHit>> {
        (**self).look_up_normal(command, manager).await
    }
}

// When we don't want to check a command can be skipped, just use the NoOpCommandOptionalExecutor that always returns the continue case.
pub struct NoOpCommandOptionalExecutor {}

#[async_trait]
impl PreparedCommandOptionalExecutor for NoOpCommandOptionalExecutor {
    async fn maybe_execute(
        &self,
        _command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        _cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        ControlFlow::Continue(manager)
    }
}
