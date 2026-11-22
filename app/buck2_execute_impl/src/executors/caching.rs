/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Debug;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use buck2_action_metadata_proto::DIGESTS_ONLY_KEY;
use buck2_action_metadata_proto::DigestsOnlyFile;
use buck2_action_metadata_proto::DigestsOnlyFiles;
use buck2_action_metadata_proto::REMOTE_DEP_FILE_KEY;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::execution_types::executor_config::RePlatformFields;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorContext;
use buck2_events::dispatch::span_async;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest::CasDigestToReExt;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::directory_to_re_tree;
use buck2_execute::execute::action_digest::ActionDigest;
use buck2_execute::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use buck2_execute::execute::action_digest_and_blobs::ActionDigestAndBlobsBuilder;
use buck2_execute::execute::blobs::ActionBlobs;
use buck2_execute::execute::cache_uploader::CacheUploadInfo;
use buck2_execute::execute::cache_uploader::CacheUploadOutcome;
use buck2_execute::execute::cache_uploader::CacheUploadResults;
use buck2_execute::execute::cache_uploader::DepFileCacheUploadOutcome;
use buck2_execute::execute::cache_uploader::IntoRemoteDepFile;
use buck2_execute::execute::cache_uploader::UploadCache;
use buck2_execute::execute::result::CommandExecutionResult;
use buck2_execute::materialize::materializer::MaterializationPurpose;
use buck2_execute::materialize::materializer::MaterializeRequest;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::re::client::ActionCacheWriteType;
use buck2_execute::re::manager::ManagedRemoteExecutionClient;
use dupe::Dupe;
use futures::future;
use futures::future::FutureExt;
use prost::Message;
use remote_execution as RE;
use remote_execution::DigestWithStatus;
use remote_execution::NamedDigest;
use remote_execution::TActionResult2;
use remote_execution::TAny;
use remote_execution::TCode;
use remote_execution::TDigest;
use remote_execution::TDirectory2;
use remote_execution::TExecutedActionMetadata;
use remote_execution::TFile;
use remote_execution::TStatus;
use remote_execution::TTimestamp;

use crate::executors::action_cache_upload_permission_checker::ActionCacheUploadPermissionChecker;
use crate::executors::to_re_platform::RePlatformFieldsToRePlatform;

/// A PreparedCommandExecutor that will write to cache after invoking the inner executor
pub struct CacheUploader {
    artifact_fs: ArtifactFs,
    materializer: Arc<dyn Materializer>,
    re_client: ManagedRemoteExecutionClient,
    /// For what the upload has to materialize first; the upload itself runs under `re_client`'s.
    invocation_re_use_case: RemoteExecutorUseCase,
    platform: RePlatformFields,
    max_bytes: Option<u64>,
    cache_upload_permission_checker: Arc<ActionCacheUploadPermissionChecker>,
}

impl CacheUploader {
    pub fn new(
        artifact_fs: ArtifactFs,
        materializer: Arc<dyn Materializer>,
        re_client: ManagedRemoteExecutionClient,
        invocation_re_use_case: RemoteExecutorUseCase,
        platform: RePlatformFields,
        max_bytes: Option<u64>,
        cache_upload_permission_checker: Arc<ActionCacheUploadPermissionChecker>,
    ) -> CacheUploader {
        CacheUploader {
            artifact_fs,
            materializer,
            re_client,
            invocation_re_use_case,
            platform,
            max_bytes,
            cache_upload_permission_checker,
        }
    }

    /// Upload an action result to the RE action cache, assuming conditions for the upload are met:
    /// the action must have been successful and must have run locally (not much point in caching
    /// something that ran on RE and is already cached), and cache uploads must be enabled for this particular action.
    /// The CacheUploader should only be used if cache uploads are enabled.
    async fn upload_local_outputs(
        &self,
        info: &CacheUploadInfo<'_>,
        result: &CommandExecutionResult,
        action_digest_and_blobs: &ActionDigestAndBlobs,
        error_on_cache_upload: bool,
        has_depfile_entry: bool,
    ) -> buck2_error::Result<(CacheUploadOutcome, Option<TActionResult2>)> {
        let digest = action_digest_and_blobs.action;
        let digest_str = digest.to_string();
        let output_bytes = result.calc_output_size_bytes();

        let (outcome, action_result_for_dep_file) = span_async(
            buck2_data::CacheUploadStart {
                key: Some(info.target.as_proto_action_key()),
                name: Some(info.target.as_proto_action_name()),
                action_digest: digest_str.clone(),
            },
            async {
                let mut file_digests = Vec::new();
                let mut tree_digests = Vec::new();

                let (outcome, action_result_for_dep_file) = async {
                    if let Some(max_bytes) = self.max_bytes {
                        if output_bytes > max_bytes {
                            return (
                                CacheUploadOutcome::RejectedOutputExceedsLimit { max_bytes },
                                None,
                            );
                        }
                    }

                    if let Err(outcome) = self
                        .check_upload_permission(info)
                        .await
                        .unwrap_or_else(|error| Err(CacheUploadOutcome::FailedOther { error }))
                    {
                        return (outcome, None);
                    }

                    // upload Action to CAS.
                    // This is necessary when writing to the ActionCache through CAS, since CAS needs to inspect the Action related to the ActionResult.
                    // Without storing the Action itself to CAS, ActionCache writes would fail.
                    if let Err(error) = self
                        .re_client
                        .upload_files_and_directories(
                            vec![],
                            vec![],
                            action_digest_and_blobs.blobs.to_inlined_blobs(),
                        )
                        .await
                    {
                        return (
                            CacheUploadOutcome::FailedUploadActionBlobs {
                                error: error.into(),
                            },
                            None,
                        );
                    }

                    // upload ActionResult to ActionCache
                    let mut result: TActionResult2 = match self
                        .upload_files_and_directories(
                            result,
                            &mut file_digests,
                            &mut tree_digests,
                            info.digest_config,
                        )
                        .await
                    {
                        Ok(Ok(result)) => result,
                        Ok(Err(outcome)) => return (outcome, None),
                        Err(error) => {
                            return (CacheUploadOutcome::FailedUploadOutputs { error }, None);
                        }
                    };
                    if let Err(error) = self
                        .re_client
                        .write_action_result(
                            digest,
                            &mut result,
                            &self.platform.to_re_platform(),
                            ActionCacheWriteType::LocalCacheUpload,
                        )
                        .await
                    {
                        return (
                            CacheUploadOutcome::FailedWriteActionResult {
                                error: error.into(),
                            },
                            None,
                        );
                    }

                    (
                        CacheUploadOutcome::Success,
                        has_depfile_entry.then_some(result),
                    )
                }
                .await;

                let cache_upload_end_event = buck2_data::CacheUploadEnd {
                    key: Some(info.target.as_proto_action_key()),
                    name: Some(info.target.as_proto_action_name()),
                    action_digest: digest_str.clone(),
                    success: outcome.uploaded(),
                    error: outcome.error(),
                    re_error_code: outcome.re_error_code(),
                    file_digests: file_digests
                        .into_iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>(),
                    tree_digests: tree_digests
                        .into_iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>(),
                    output_bytes: Some(output_bytes),
                };
                (
                    (
                        outcome.log_and_create_result(&digest_str, error_on_cache_upload),
                        action_result_for_dep_file,
                    ),
                    Box::new(cache_upload_end_event),
                )
            },
        )
        .await;
        Ok((outcome?, action_result_for_dep_file))
    }

    /// Upload an action result with additional information about dep files to the RE action cache.
    /// The conditions for the upload are: the action must have been successful and produced a depfile
    /// and cache uploads must have been enabled for this action.
    async fn upload_dep_file(
        &self,
        info: &CacheUploadInfo<'_>,
        result: &CommandExecutionResult,
        action_result: Option<&mut TActionResult2>,
        dep_file_bundle: &mut dyn IntoRemoteDepFile,
        remote_dep_file_action: &ActionDigestAndBlobs,
        error_on_cache_upload: bool,
    ) -> buck2_error::Result<CacheUploadOutcome> {
        let remote_dep_file_key = remote_dep_file_action.action.to_string();
        span_async(
            buck2_data::DepFileUploadStart {
                key: Some(info.target.as_proto_action_key()),
                name: Some(info.target.as_proto_action_name()),
                remote_dep_file_key: remote_dep_file_key.clone(),
            },
            async {
                let outcome = async {
                    let action_result = action_result.ok_or(DepFileReActionResultMissingError(
                        remote_dep_file_key.clone(),
                    ))?;

                    if let Err(rejected) = self.check_upload_permission(info).await? {
                        return Ok(rejected);
                    }
                    // The entry outlives any one build, and this runs after every cache hit of
                    // the action, so a lookup is cheaper than materializing and uploading again.
                    // A failed lookup is treated as a miss.
                    if matches!(
                        self.re_client
                            .action_cache(
                                remote_dep_file_action.action,
                                &self.platform.to_re_platform(),
                                // Not the action's cache query, so its timings aren't reported.
                                &mut buck2_data::CacheQueryTimings::default(),
                            )
                            .await,
                        Ok(Some(_))
                    ) {
                        return Ok(CacheUploadOutcome::DepFileEntryExists);
                    }
                    let remote_dep_file = dep_file_bundle
                        .make_remote_dep_file(
                            info.digest_config,
                            &self.artifact_fs,
                            self.materializer.as_ref(),
                            self.invocation_re_use_case,
                            result,
                        )
                        .await?
                        .ok_or_else(|| {
                            DepFileUploadNoDeclaredDepFiles(remote_dep_file_key.clone())
                        })?;

                    let digest = remote_dep_file_action.action;
                    let dep_file_tany = TAny {
                        type_url: REMOTE_DEP_FILE_KEY.to_owned(),
                        value: remote_dep_file.encode_to_vec(),
                        ..Default::default()
                    };
                    action_result.execution_metadata.auxiliary_metadata = vec![dep_file_tany];

                    // upload Action to CAS.
                    // This is necessary when writing to the ActionCache through CAS, since CAS needs to inspect the Action related to the ActionResult.
                    // Without storing the Action itself to CAS, ActionCache writes would fail.
                    self.re_client
                        .upload_files_and_directories(
                            vec![],
                            vec![],
                            remote_dep_file_action.blobs.to_inlined_blobs(),
                        )
                        .await?;

                    // upload ActionResult to ActionCache
                    self.re_client
                        .write_action_result(
                            digest,
                            action_result,
                            &self.platform.to_re_platform(),
                            ActionCacheWriteType::RemoteDepFile,
                        )
                        .await?;

                    Ok(CacheUploadOutcome::Success)
                }
                .await
                .unwrap_or_else(|error| CacheUploadOutcome::FailedOther { error });

                let end_event = buck2_data::DepFileUploadEnd {
                    key: Some(info.target.as_proto_action_key()),
                    name: Some(info.target.as_proto_action_name()),
                    remote_dep_file_key: remote_dep_file_key.clone(),
                    success: outcome.uploaded(),
                    error: outcome.error(),
                    re_error_code: outcome.re_error_code(),
                };
                (
                    outcome.log_and_create_result(&remote_dep_file_key, error_on_cache_upload),
                    end_event,
                )
            },
        )
        .await
    }

    async fn upload_digests_only(
        &self,
        info: &CacheUploadInfo<'_>,
        result: &CommandExecutionResult,
        action_digest_and_blobs: &ActionDigestAndBlobs,
    ) {
        if !info.publish_digests_only || result.ran_locally_in_place_of_remote() {
            return;
        }
        let outcome = async {
            let action = digests_only_action(&action_digest_and_blobs.action, info.digest_config);
            let mut blobs = action.blobs;
            let mut outputs = Vec::new();
            for output in result.resolve_outputs(&self.artifact_fs) {
                let (output, _, value) = output?;
                outputs.push((output.path().to_string(), value));
            }
            let Some(mut action_result) =
                digests_only_result(outputs, &mut blobs, info.digest_config)
            else {
                return Ok(CacheUploadOutcome::RejectedSymlinkOutput);
            };
            let tree_bytes: i64 = action_result
                .output_files
                .iter()
                .map(|tree| tree.digest.digest.size_in_bytes)
                .sum();
            if let Some(max_bytes) = self.max_bytes
                && tree_bytes as u64 > max_bytes
            {
                return Ok(CacheUploadOutcome::RejectedOutputExceedsLimit { max_bytes });
            }
            if let Err(outcome) = self.check_upload_permission(info).await? {
                return Ok(outcome);
            }
            self.re_client.upload_blobs(&blobs).await?;
            self.re_client
                .write_action_result(
                    action.action,
                    &mut action_result,
                    &self.platform.to_re_platform(),
                    ActionCacheWriteType::DigestsOnly,
                )
                .await?;
            buck2_error::Ok(CacheUploadOutcome::Success)
        }
        .await
        .unwrap_or_else(|error| CacheUploadOutcome::FailedOther { error });
        if !outcome.uploaded() {
            tracing::info!(
                "No digests-only cache entry for `{}`: {}",
                action_digest_and_blobs.action,
                outcome.error()
            );
        }
    }

    async fn check_upload_permission(
        &self,
        info: &CacheUploadInfo<'_>,
    ) -> buck2_error::Result<Result<(), CacheUploadOutcome>> {
        let outcome = if let Err(reason) = self
            .cache_upload_permission_checker
            .has_permission_to_upload_to_cache(&self.re_client, &self.platform, info.digest_config)
            .await?
        {
            Err(CacheUploadOutcome::RejectedPermissionDenied { reason })
        } else {
            Ok(())
        };
        Ok(outcome)
    }

    async fn upload_files_and_directories(
        &self,
        result: &CommandExecutionResult,
        file_digests: &mut Vec<TrackedFileDigest>,
        tree_digests: &mut Vec<TrackedFileDigest>,
        digest_config: DigestConfig,
    ) -> buck2_error::Result<Result<TActionResult2, CacheUploadOutcome>> {
        let mut upload_futs = vec![];
        let mut output_files: Vec<TFile> = Vec::new();
        let mut output_directories: Vec<TDirectory2> = Vec::new();

        let mut outputs_to_materialize = Vec::new();

        for output_result in result.resolve_outputs(&self.artifact_fs) {
            let (output, local_path, value) = output_result?;
            outputs_to_materialize.push((local_path.clone(), value.dupe()));
            match value.entry().as_ref() {
                DirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => {
                    output_files.push(TFile {
                        digest: DigestWithStatus {
                            digest: f.digest.to_re(),
                            status: TStatus {
                                code: TCode::OK,
                                message: String::new(),
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        name: output.path().to_string(),
                        executable: f.is_executable,
                        ..Default::default()
                    });

                    let named_digest = NamedDigest {
                        name: self
                            .artifact_fs
                            .fs()
                            .resolve(&local_path)
                            .as_maybe_relativized_str()?
                            .to_owned(),
                        digest: f.digest.to_re(),
                        ..Default::default()
                    };

                    file_digests.push(f.digest.dupe());
                    upload_futs.push(
                        self.re_client
                            .upload_files_and_directories(vec![named_digest], vec![], vec![])
                            .boxed(),
                    );
                }
                DirectoryEntry::Dir(d) => {
                    let tree = directory_to_re_tree(d);
                    let mut action_blobs = ActionBlobs::new(digest_config);
                    let tree_digest = action_blobs.add_protobuf_message(&tree, digest_config);

                    output_directories.push(TDirectory2 {
                        path: output.path().to_string(),
                        tree_digest: tree_digest.to_re(),
                        root_directory_digest: d.fingerprint().to_re(),
                        ..Default::default()
                    });

                    let identity = None; // TODO(#503): implement this
                    let fut = async move {
                        self.re_client
                            .upload(
                                self.artifact_fs.fs(),
                                self.materializer.as_ref(),
                                &action_blobs,
                                &local_path,
                                &d.dupe().as_immutable(),
                                identity,
                            )
                            .await
                            .map(|_| ())
                    };

                    upload_futs.push(fut.boxed());
                    tree_digests.push(tree_digest);
                }
                DirectoryEntry::Leaf(
                    ActionDirectoryMember::Symlink(..) | ActionDirectoryMember::ExternalSymlink(..),
                ) => {
                    // Bail, there is something that is not a file here and we don't handle this.
                    // This will happen if the value is a symlink. The primary output of a command
                    // being a symlink is probably unlikely. Unfortunately, we can't represent this
                    // in RE's action output, so we either have to lie about the output and pretend
                    // it's a file, or bail.
                    return Ok(Err(CacheUploadOutcome::RejectedSymlinkOutput));
                }
            }
        }

        let uploads = async {
            // The uploads below read the outputs. The execution that produced them holds them
            // through its result; one that did not, a remote run's say, has them put on disk here.
            let _outputs_lease = if result.held_outputs.is_some() {
                None
            } else {
                let response = self
                    .materializer
                    .materialize(MaterializeRequest {
                        artifacts: outputs_to_materialize,
                        outputs: Vec::new(),
                        purpose: MaterializationPurpose::IntermediateOnly,
                        re_use_case: self.invocation_re_use_case,
                    })
                    .await
                    .buck_error_context("Error materializing outputs for cache upload")?;
                for materialized in response.results {
                    materialized
                        .buck_error_context("Error materializing outputs for cache upload")?;
                }
                Some(response.lease)
            };

            buck2_util::future::try_join_all(upload_futs)
                .await
                .buck_error_context("Error uploading outputs")?;

            Ok(())
        };

        let std_streams = async {
            result
                .report
                .std_streams
                .clone()
                .into_re(&self.re_client, digest_config)
                .await
                .buck_error_context("Error accessing std_streams")
        };

        let ((), std_streams) = future::try_join(uploads, std_streams).await?;

        let worker;

        #[cfg(fbcode_build)]
        {
            let hostname = hostname::get()?;
            worker = hostname.to_string_lossy().into_owned();
        }

        #[cfg(not(fbcode_build))]
        {
            worker = "".to_owned();
        }

        let (stdout_raw, stdout_digest) = std_streams.stdout.into_raw_or_digest();
        let (stderr_raw, stderr_digest) = std_streams.stderr.into_raw_or_digest();

        let result = TActionResult2 {
            output_files,
            output_directories,
            exit_code: 0,
            stdout_raw,
            stdout_digest,
            stderr_raw,
            stderr_digest,
            execution_metadata: Box::new(TExecutedActionMetadata {
                worker,
                execution_dir: "".to_owned(),
                execution_start_timestamp: systemtime_to_ttimestamp(
                    result.report.timing.start_time,
                )?,
                execution_completed_timestamp: systemtime_to_ttimestamp(
                    result.report.timing.end_time(),
                )?,
                execution_attempts: 1,
                ..Default::default()
            }),
            ..Default::default()
        };

        Ok(Ok(result))
    }
}

#[cfg(fbcode_build)] // Relies on fbcode future sizes
buck2_util::size_assert::words_of_async_fn_future!(
    CacheUploader::upload_dep_file,
    (_, _, _, _, _, _, _),
    ~175
);

#[derive(Debug, buck2_error::Error)]
#[error("Missing action result for dep file key `{0}`")]
#[buck2(tag = Tier0)]
struct DepFileReActionResultMissingError(String);

#[derive(Debug, buck2_error::Error)]
#[error("No dep files were declared, nothing to upload for dep file key `{0}`")]
#[buck2(tag = Input)]
struct DepFileUploadNoDeclaredDepFiles(String);

#[async_trait]
impl UploadCache for CacheUploader {
    async fn upload(
        &self,
        info: &CacheUploadInfo<'_>,
        res: &CommandExecutionResult,
        re_result: Option<&mut TActionResult2>,
        dep_file_bundle: Option<&mut dyn IntoRemoteDepFile>,
        action_digest_and_blobs: &ActionDigestAndBlobs,
    ) -> buck2_error::Result<CacheUploadResults> {
        // A result RE produced is published already. A local one needs the action's
        // permission, and must be the run the command was configured for: a hybrid executor's
        // fallback or won race would publish, from one build and not the next, what remote
        // execution would otherwise have produced.
        let (cache_upload_outcome, mut local_result) = if !res.was_locally_executed() {
            if dep_file_bundle.is_some() {
                (CacheUploadOutcome::HadDepFileBundle, None)
            } else {
                tracing::info!(
                    "Cache upload for `{}` not attempted",
                    action_digest_and_blobs.action
                );
                (CacheUploadOutcome::NonLocalExecution, None)
            }
        } else if !info.allow_cache_upload {
            self.upload_digests_only(info, res, action_digest_and_blobs)
                .boxed()
                .await;
            (CacheUploadOutcome::ActionUploadNotAllowed, None)
        } else if res.ran_locally_in_place_of_remote() {
            (CacheUploadOutcome::LocalInPlaceOfRemote, None)
        } else {
            tracing::debug!(
                "Uploading action result for `{}`",
                action_digest_and_blobs.action
            );
            // TODO(bobyf, torozco) should these be critical sections?
            // Boxed so the common non-local paths do not carry the local-upload future.
            self.upload_local_outputs(
                info,
                res,
                action_digest_and_blobs,
                false,
                dep_file_bundle.is_some(),
            )
            .boxed()
            .await?
        };
        // A local upload re-sends the result it just wrote; anything else re-sends RE's.
        let action_result = if res.was_locally_executed() {
            local_result.as_mut()
        } else {
            re_result
        };

        // The dep file entry publishes the result it names, so it follows the result: a local
        // one on the action's permission, a remote one or a cache hit as RE published it.
        let should_upload_dep_file = if res.was_locally_executed() {
            info.allow_cache_upload && !res.ran_locally_in_place_of_remote()
        } else {
            res.was_remotely_executed() || res.was_action_cache_hit()
        };

        let (dep_file_cache_upload_outcome, dep_file_cache_upload_key) = match dep_file_bundle {
            Some(dep_file_bundle) if should_upload_dep_file => {
                let remote_dep_file_action = dep_file_bundle.remote_dep_file_action(
                    info.digest_config,
                    info.mergebase,
                    info.re_platform,
                );
                (
                    DepFileCacheUploadOutcome::attempted(
                        self.upload_dep_file(
                            info,
                            res,
                            action_result,
                            dep_file_bundle,
                            &remote_dep_file_action,
                            false,
                        )
                        .await?,
                    ),
                    Some(remote_dep_file_action.action.coerce()),
                )
            }
            Some(..) if res.was_locally_executed() && !info.allow_cache_upload => {
                tracing::info!(
                    "Dep file cache upload for `{}` not attempted: local cache upload not allowed",
                    action_digest_and_blobs.action
                );
                (DepFileCacheUploadOutcome::LocalUploadNotAllowed, None)
            }
            Some(..) if res.ran_locally_in_place_of_remote() => {
                tracing::info!(
                    "Dep file cache upload for `{}` not attempted: a local run in place of a remote one",
                    action_digest_and_blobs.action
                );
                (DepFileCacheUploadOutcome::LocalInPlaceOfRemote, None)
            }
            Some(..) => {
                tracing::info!(
                    "Dep file cache upload for `{}` not attempted: unsupported execution kind",
                    action_digest_and_blobs.action
                );
                (DepFileCacheUploadOutcome::UnsupportedExecutionKind, None)
            }
            None => {
                tracing::info!(
                    "Dep file cache upload for `{}` not attempted: no dep file bundle",
                    action_digest_and_blobs.action
                );
                (DepFileCacheUploadOutcome::NoDepFileBundle, None)
            }
        };

        Ok(CacheUploadResults {
            cache_upload_outcome,
            dep_file_cache_upload_outcome,
            dep_file_cache_upload_key,
        })
    }
}

/// A change to the key or to the content of a digests-only entry needs a new version, so that a
/// lookup never returns an entry in an older format.
const DIGESTS_ONLY_KEY_VERSION: u32 = 0;

/// No rule runs a program named `buck2-digests-only`, so no real action has the digest of the
/// synthetic action of a digests-only entry.
const DIGESTS_ONLY_ACTION_MARKER: &str = "buck2-digests-only";

/// Returns the synthetic action that the digests-only entry of `action` is stored under.
pub(crate) fn digests_only_action(
    action: &ActionDigest,
    digest_config: DigestConfig,
) -> ActionDigestAndBlobs {
    let mut blobs = ActionDigestAndBlobsBuilder::new(digest_config);
    let command = blobs.add_command(&RE::Command {
        arguments: vec![
            DIGESTS_ONLY_ACTION_MARKER.to_owned(),
            DIGESTS_ONLY_KEY_VERSION.to_string(),
            action.to_string(),
        ],
        ..Default::default()
    });
    blobs.build(&RE::Action {
        command_digest: Some(command.to_grpc()),
        ..Default::default()
    })
}

/// Returns `None` for a symlink output, because the normal upload rejects symlink outputs as well.
/// A cache can treat an entry as missing when the CAS lacks a file of the entry, including a file
/// that the Tree of an output directory lists. The entry therefore lists the Tree of each output
/// directory as an output file, under the path of the directory. The caller uploads the Trees.
fn digests_only_result<'a>(
    outputs: impl IntoIterator<Item = (String, &'a ArtifactValue)>,
    blobs: &mut ActionBlobs,
    digest_config: DigestConfig,
) -> Option<TActionResult2> {
    let mut files = Vec::new();
    let mut trees = Vec::new();
    for (path, value) in outputs {
        match value.entry().as_ref() {
            DirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => {
                let digest = f.digest.to_re();
                files.push(DigestsOnlyFile {
                    path,
                    hash: digest.hash,
                    size_in_bytes: digest.size_in_bytes,
                    is_executable: f.is_executable,
                });
            }
            DirectoryEntry::Dir(d) => trees.push(TFile {
                digest: DigestWithStatus {
                    digest: blobs
                        .add_protobuf_message(&directory_to_re_tree(d), digest_config)
                        .to_re(),
                    ..Default::default()
                },
                name: path,
                ..Default::default()
            }),
            DirectoryEntry::Leaf(_) => return None,
        }
    }
    Some(TActionResult2 {
        output_files: trees,
        execution_metadata: Box::new(TExecutedActionMetadata {
            auxiliary_metadata: vec![TAny {
                type_url: DIGESTS_ONLY_KEY.to_owned(),
                value: DigestsOnlyFiles { files }.encode_to_vec(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub(crate) fn digests_only_outputs(
    entry: &TActionResult2,
) -> buck2_error::Result<Option<TActionResult2>> {
    let Some(metadata) = entry
        .execution_metadata
        .auxiliary_metadata
        .iter()
        .find(|metadata| metadata.type_url == DIGESTS_ONLY_KEY)
    else {
        return Ok(None);
    };
    let files = DigestsOnlyFiles::decode(metadata.value.as_slice())?.files;
    Ok(Some(TActionResult2 {
        output_files: files
            .into_iter()
            .map(|file| TFile {
                digest: DigestWithStatus {
                    digest: TDigest {
                        hash: file.hash,
                        size_in_bytes: file.size_in_bytes,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                name: file.path,
                executable: file.is_executable,
                ..Default::default()
            })
            .collect(),
        output_directories: entry
            .output_files
            .iter()
            .map(|tree| TDirectory2 {
                path: tree.name.clone(),
                tree_digest: tree.digest.digest.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }))
}

fn systemtime_to_ttimestamp(time: SystemTime) -> buck2_error::Result<TTimestamp> {
    let duration = time.duration_since(SystemTime::UNIX_EPOCH)?;
    Ok(TTimestamp {
        seconds: duration
            .as_secs()
            .try_into()
            .buck_error_context("Invalid duration")?,
        // Max 1B so it won't wrap around.
        nanos: duration.subsec_nanos() as _,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use buck2_common::file_ops::metadata::FileMetadata;

    use super::*;

    #[test]
    fn test_cache_upload_result_maps_rejections() {
        assert_eq!(
            buck2_data::UploadResult::RejectedOutputExceedsLimit,
            CacheUploadOutcome::RejectedOutputExceedsLimit { max_bytes: 1 }.to_proto(),
        );
        assert_eq!(
            buck2_data::UploadResult::RejectedPermissionDenied,
            CacheUploadOutcome::RejectedPermissionDenied {
                reason: "denied".to_owned(),
            }
            .to_proto(),
        );
        assert_eq!(
            buck2_data::UploadResult::RejectedSymlinkOutput,
            CacheUploadOutcome::RejectedSymlinkOutput.to_proto(),
        );
    }

    #[test]
    fn test_cache_upload_result_maps_failures() {
        let mk_error = || buck2_error::buck2_error!(buck2_error::ErrorTag::Tier0, "boom");

        assert_eq!(
            buck2_data::UploadResult::FailedUploadActionBlobs,
            CacheUploadOutcome::FailedUploadActionBlobs { error: mk_error() }.to_proto(),
        );
        assert_eq!(
            buck2_data::UploadResult::FailedUploadOutputs,
            CacheUploadOutcome::FailedUploadOutputs { error: mk_error() }.to_proto(),
        );
        assert_eq!(
            buck2_data::UploadResult::FailedWriteActionResult,
            CacheUploadOutcome::FailedWriteActionResult { error: mk_error() }.to_proto(),
        );
        assert_eq!(
            buck2_data::UploadResult::FailedOther,
            CacheUploadOutcome::FailedOther { error: mk_error() }.to_proto(),
        );
    }

    #[test]
    fn test_dep_file_cache_upload_not_attempted_reasons_map_to_not_attempted() {
        assert_eq!(
            buck2_data::UploadResult::NotAttempted,
            DepFileCacheUploadOutcome::NoDepFileBundle.to_proto(),
        );
        assert_eq!(
            buck2_data::UploadResult::NotAttempted,
            DepFileCacheUploadOutcome::UnsupportedExecutionKind.to_proto(),
        );
    }

    #[test]
    fn test_digests_only_action() {
        // The key of a digests-only entry must be the same on every host. The key must differ
        // from the digest of the action, so that the entry never replaces the normal entry.
        // Derive the key of a fixed action digest, and expect a known digest that differs from
        // the action digest.
        let digest_config = DigestConfig::testing_default();
        let action = ActionDigest::from_content(b"action", digest_config.cas_digest_config());
        let key = digests_only_action(&action, digest_config).action;
        assert_eq!(
            "6b6c725dc8a6c341eb5592c0e1f414f73bb14a81:46",
            key.to_string()
        );
    }

    #[test]
    fn test_digests_only_result() {
        // The output files of the digests-only entry of a local result are the Trees of the output
        // directories. The auxiliary metadata of the entry records the output files of the local
        // result. Build the entry of an executable file and an empty directory. Expect the Tree of
        // the directory as the only output file of the entry, and expect `blobs` to hold the Tree.
        // Expect the digest and the executable bit of the file in the auxiliary metadata.
        let digest_config = DigestConfig::testing_default();
        let file = FileMetadata {
            digest: TrackedFileDigest::from_content(b"file", digest_config.cas_digest_config()),
            is_executable: true,
        };
        let dir = ArtifactValue::dir(digest_config.empty_directory());
        let mut blobs = ActionBlobs::new(digest_config);
        let result = digests_only_result(
            [
                ("out/file".to_owned(), &ArtifactValue::file(file.dupe())),
                ("out/dir".to_owned(), &dir),
            ],
            &mut blobs,
            digest_config,
        )
        .unwrap();

        assert_eq!(1, result.output_files.len());
        let tree = &result.output_files[0];
        assert_eq!("out/dir", tree.name);
        assert!(
            blobs
                .keys()
                .any(|digest| digest.to_re() == tree.digest.digest)
        );
        let metadata = &result.execution_metadata.auxiliary_metadata[0];
        assert_eq!(DIGESTS_ONLY_KEY, metadata.type_url);
        let files = DigestsOnlyFiles::decode(metadata.value.as_slice())
            .unwrap()
            .files;
        assert_eq!(file.digest.to_re().hash, files[0].hash);
        assert!(files[0].is_executable);

        // A lookup reads the entry with `digests_only_outputs`. Expect the file and the directory
        // in its result, and no result for an entry without the auxiliary metadata.
        let outputs = digests_only_outputs(&result).unwrap().unwrap();
        assert_eq!("out/file", outputs.output_files[0].name);
        assert_eq!(file.digest.to_re(), outputs.output_files[0].digest.digest);
        assert!(outputs.output_files[0].executable);
        assert_eq!("out/dir", outputs.output_directories[0].path);
        assert_eq!(
            tree.digest.digest,
            outputs.output_directories[0].tree_digest
        );
        assert!(
            digests_only_outputs(&TActionResult2::default())
                .unwrap()
                .is_none()
        );
    }
}
