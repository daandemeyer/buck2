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
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_build_api::actions::errors::execute_error::ExecuteError;
use buck2_build_api::actions::execute::action_execution_target::ActionExecutionTarget;
use buck2_build_api::actions::execute::action_executor::ActionExecutionMetadata;
use buck2_build_api::actions::execute::action_executor::ActionOutputs;
use buck2_build_api::actions::impls::run_action_knobs::RunActionKnobs;
use buck2_build_api::artifact_groups::ArtifactGroup;
use buck2_build_api::artifact_groups::ArtifactGroupValues;
use buck2_build_signals::env::WaitingData;
use buck2_common::io::IoProvider;
use buck2_core::cells::CellResolver;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::cells::name::CellName;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::fs::buck_out_path::BuckOutPathResolver;
use buck2_core::fs::project::ProjectRootTemp;
use buck2_events::dispatch::EventDispatcher;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_execute::dep_file_state::DepFileStore;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::blocking::testing::DummyBlockingExecutor;
use buck2_execute::execute::cache_uploader::CacheUploadResults;
use buck2_execute::execute::cache_uploader::IntoRemoteDepFile;
use buck2_execute::execute::low_pass_filter::LowPassFilter;
use buck2_execute::execute::manager::CommandExecutionManager;
use buck2_execute::execute::prepared::PreparedAction;
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_execute::execute::result::CommandExecutionResult;
use buck2_execute::execute::write_action_cache::PopulateOutcome;
use buck2_execute::incremental_state::IncrementalStore;
use buck2_execute::materialize::materializer::ArtifactNotMaterializedReason;
use buck2_execute::materialize::materializer::CleanStaleArtifactsArgs;
use buck2_execute::materialize::materializer::CopiedArtifact;
use buck2_execute::materialize::materializer::DeclareMatchOutcome;
use buck2_execute::materialize::materializer::HeldContent;
use buck2_execute::materialize::materializer::MaterializationError;
use buck2_execute::materialize::materializer::MaterializeRequest;
use buck2_execute::materialize::materializer::MaterializeResponse;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::materialize::materializer::MaterializerIterItem;
use buck2_execute::materialize::materializer::ReadLease;
use buck2_execute::materialize::materializer::TrackedPaths;
use buck2_execute::materialize::materializer::WriteLease;
use buck2_execute::re::manager::UnconfiguredRemoteExecutionClient;
use buck2_execute::re::output_trees_download_config::OutputTreesDownloadConfig;
use buck2_file_watcher::dep_files::DepFileCache;
use buck2_file_watcher::mergebase::Mergebase;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_hash::BuckMutInsertionSet;
use buck2_hash::BuckMutMap;
use buck2_hash::BuckMutSet;
use buck2_http::HttpClient;
use dice_futures::cancellation::CancellationContext;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use remote_execution::TActionResult2;

use super::*;

struct RecordingWriteCache {
    populates: bool,
    uploads: Mutex<Vec<TrackedFileDigest>>,
}

#[async_trait]
impl WriteActionCache for RecordingWriteCache {
    fn use_case(&self) -> RemoteExecutorUseCase {
        RemoteExecutorUseCase::buck2_default()
    }

    fn populates(&self) -> bool {
        self.populates
    }

    async fn lookup(
        &self,
        _action: &ActionDigestAndBlobs,
    ) -> buck2_error::Result<Option<TActionResult2>> {
        unimplemented!()
    }

    async fn upload(
        &self,
        _content: &[u8],
        digest: &TrackedFileDigest,
        _digest_config: DigestConfig,
    ) -> buck2_error::Result<()> {
        self.uploads.lock().push(digest.dupe());
        Ok(())
    }

    async fn populate(
        &self,
        _action: &ActionDigestAndBlobs,
        _content: &[u8],
        _digest: &TrackedFileDigest,
        _is_executable: bool,
        _digest_config: DigestConfig,
    ) -> buck2_error::Result<PopulateOutcome> {
        unimplemented!()
    }
}

#[derive(Allocative, Default)]
struct RecordingMaterializer {
    #[allocative(skip)]
    declared_existing: Mutex<Vec<ProjectRelativePathBuf>>,
    #[allocative(skip)]
    declared_cas: Mutex<Vec<ProjectRelativePathBuf>>,
}

#[async_trait]
impl TrackedPaths for RecordingMaterializer {
    async fn hold_file(
        &self,
        _digest: &FileDigest,
    ) -> buck2_error::Result<Option<(AbsNormPathBuf, ReadLease)>> {
        unimplemented!()
    }
}

#[async_trait]
impl Materializer for RecordingMaterializer {
    fn is_disk_state(&self) -> bool {
        true
    }

    async fn hold_or_take(
        &self,
        _path: ProjectRelativePathBuf,
        _value: ArtifactValue,
    ) -> buck2_error::Result<HeldContent> {
        Ok(HeldContent::Missing(WriteLease::noop()))
    }

    async fn declare_existing(
        &self,
        _lease: &WriteLease,
        artifacts: Vec<DeclareArtifactPayload>,
    ) -> buck2_error::Result<()> {
        self.declared_existing
            .lock()
            .extend(artifacts.into_iter().map(|artifact| artifact.path));
        Ok(())
    }

    async fn materialize(
        &self,
        _request: MaterializeRequest,
    ) -> buck2_error::Result<MaterializeResponse> {
        unimplemented!()
    }

    async fn declare_copy_impl(
        &self,
        _path: ProjectRelativePathBuf,
        _value: ArtifactValue,
        _srcs: Vec<CopiedArtifact>,
    ) -> buck2_error::Result<()> {
        unimplemented!()
    }

    async fn declare_cas_many_impl<'a, 'b>(
        &self,
        _info: Arc<CasDownloadInfo>,
        artifacts: Vec<DeclareArtifactPayload>,
    ) -> buck2_error::Result<()> {
        self.declared_cas
            .lock()
            .extend(artifacts.into_iter().map(|artifact| artifact.path));
        Ok(())
    }

    async fn declare_write<'a>(
        &self,
        _generate: Box<dyn FnOnce() -> buck2_error::Result<Vec<WriteRequest>> + Send + 'a>,
    ) -> buck2_error::Result<Vec<ArtifactValue>> {
        unimplemented!()
    }

    async fn prepare_outputs(
        &self,
        _paths: Vec<ProjectRelativePathBuf>,
    ) -> buck2_error::Result<WriteLease> {
        unimplemented!()
    }

    async fn start_writing(&self, _lease: &WriteLease) -> buck2_error::Result<()> {
        unimplemented!()
    }

    async fn make_outputs_writable(
        &self,
        _paths: Vec<ProjectRelativePathBuf>,
    ) -> buck2_error::Result<()> {
        unimplemented!()
    }

    async fn fill(
        &self,
        _lease: &WriteLease,
        _path: ProjectRelativePathBuf,
        _value: ArtifactValue,
        _from: Option<ProjectRelativePathBuf>,
        _re_use_case: RemoteExecutorUseCase,
        _cancellations: &CancellationContext,
    ) -> buck2_error::Result<()> {
        unimplemented!()
    }

    async fn publish(
        &self,
        _lease: &WriteLease,
        _src: ProjectRelativePathBuf,
        _dest: ProjectRelativePathBuf,
        _value: ArtifactValue,
    ) -> buck2_error::Result<ReadLease> {
        unimplemented!()
    }

    async fn declare_match(
        &self,
        _artifacts: Vec<(ProjectRelativePathBuf, ArtifactValue)>,
    ) -> buck2_error::Result<DeclareMatchOutcome> {
        unimplemented!()
    }

    async fn has_artifact_at(&self, _path: ProjectRelativePathBuf) -> buck2_error::Result<bool> {
        unimplemented!()
    }

    async fn materialize_many(
        &self,
        _artifact_paths: Vec<ProjectRelativePathBuf>,
    ) -> buck2_error::Result<BoxStream<'static, Result<(), MaterializationError>>> {
        unimplemented!()
    }

    async fn get_materialized_file_paths(
        &self,
        _files: Vec<(ProjectRelativePathBuf, FileDigest)>,
    ) -> buck2_error::Result<Vec<Result<ProjectRelativePathBuf, ArtifactNotMaterializedReason>>>
    {
        unimplemented!()
    }

    fn iterate(&self) -> buck2_error::Result<BoxStream<'static, MaterializerIterItem>> {
        unimplemented!()
    }

    async fn allocative(&self) -> buck2_error::Result<allocative::FlameGraphOutput> {
        unimplemented!()
    }

    fn fsck(
        &self,
    ) -> buck2_error::Result<BoxStream<'static, (ProjectRelativePathBuf, buck2_error::Error)>> {
        unimplemented!()
    }

    async fn refresh_ttls(&self, _min_ttl: i64) -> buck2_error::Result<()> {
        unimplemented!()
    }

    async fn get_ttl_refresh_log(&self) -> buck2_error::Result<String> {
        unimplemented!()
    }

    async fn clean_stale_artifacts(
        &self,
        _args: CleanStaleArtifactsArgs,
    ) -> buck2_error::Result<buck2_cli_proto::CleanStaleResponse> {
        unimplemented!()
    }

    async fn clean_scratch(&self) -> buck2_error::Result<buck2_cli_proto::CleanStaleResponse> {
        unimplemented!()
    }

    async fn test_iter(&self, _count: usize) -> buck2_error::Result<String> {
        unimplemented!()
    }

    async fn flush_all_access_times(&self) -> buck2_error::Result<String> {
        unimplemented!()
    }

    fn log_materializer_state(&self, _events: &EventDispatcher) {
        unimplemented!()
    }

    fn add_snapshot_stats(&self, _snapshot: &mut buck2_data::Snapshot) {
        unimplemented!()
    }

    async fn get_artifact_entries_for_materialized_paths(
        &self,
        _paths: Vec<ProjectRelativePathBuf>,
        _fetch_root_artifact_entries_for_subpaths: bool,
    ) -> buck2_error::Result<
        Vec<
            Option<(
                ProjectRelativePathBuf,
                ActionDirectoryEntry<ActionSharedDirectory>,
            )>,
        >,
    > {
        unimplemented!()
    }
}

struct TestCtx {
    fs: ArtifactFs,
    materializer: RecordingMaterializer,
    blocking_executor: DummyBlockingExecutor,
    gate: LowPassFilter,
    write_action_cache: RecordingWriteCache,
    knobs: RunActionKnobs,
    re_client_requests: AtomicUsize,
}

#[async_trait]
impl ActionExecutionCtx for TestCtx {
    fn target(&self) -> ActionExecutionTarget<'_> {
        unimplemented!()
    }

    fn fs(&self) -> &ArtifactFs {
        &self.fs
    }

    fn executor_fs(&self) -> ExecutorFs<'_> {
        unimplemented!()
    }

    fn materializer(&self) -> &dyn Materializer {
        &self.materializer
    }

    fn events(&self) -> &EventDispatcher {
        unimplemented!()
    }

    fn command_execution_manager(&self, _waiting_data: WaitingData) -> CommandExecutionManager {
        unimplemented!()
    }

    fn mergebase(&self) -> &Mergebase {
        unimplemented!()
    }

    fn prepare_action(
        &mut self,
        _request: &CommandExecutionRequest,
        _re_outputs_required: bool,
    ) -> buck2_error::Result<PreparedAction> {
        unimplemented!()
    }

    async fn action_cache(
        &mut self,
        _manager: CommandExecutionManager,
        _request: &CommandExecutionRequest,
        _prepared_action: &PreparedAction,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        unimplemented!()
    }

    async fn remote_dep_file_cache(
        &mut self,
        _manager: CommandExecutionManager,
        _request: &CommandExecutionRequest,
        _prepared_action: &PreparedAction,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        unimplemented!()
    }

    async fn cache_upload(
        &mut self,
        _action: &ActionDigestAndBlobs,
        _execution_result: &CommandExecutionResult,
        _re_result: Option<&mut TActionResult2>,
        _dep_file_entry: Option<&mut dyn IntoRemoteDepFile>,
        _allow_cache_upload: bool,
        _publish_digests_only: bool,
    ) -> buck2_error::Result<CacheUploadResults> {
        unimplemented!()
    }

    async fn exec_cmd(
        &mut self,
        _manager: CommandExecutionManager,
        _request: &CommandExecutionRequest,
        _prepared_action: &PreparedAction,
    ) -> CommandExecutionResult {
        unimplemented!()
    }

    fn unpack_command_execution_result(
        &mut self,
        _executor_preference: ExecutorPreference,
        _result: CommandExecutionResult,
        _allows_cache_upload: bool,
        _allows_dep_file_cache_upload: bool,
        _input_files_bytes: Option<u64>,
        _incremental_kind: buck2_data::IncrementalKind,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        unimplemented!()
    }

    fn artifact_values(&self, _input: &ArtifactGroup) -> &ArtifactGroupValues {
        unimplemented!()
    }

    fn artifact_path_mapping(
        &self,
        _filter: Option<&BuckMutInsertionSet<ArtifactGroup>>,
    ) -> BuckMutMap<&Artifact, ContentBasedPathHash> {
        unimplemented!()
    }

    fn artifact_path_mapping_with_path_only(
        &self,
        _rendered: &BuckMutInsertionSet<ArtifactGroup>,
        _path_only: &BuckMutInsertionSet<ArtifactGroup>,
    ) -> (
        BuckMutMap<&Artifact, ContentBasedPathHash>,
        BuckMutSet<&Artifact>,
    ) {
        unimplemented!()
    }

    fn blocking_executor(&self) -> &dyn BlockingExecutor {
        &self.blocking_executor
    }

    fn re_client(&self) -> UnconfiguredRemoteExecutionClient {
        self.re_client_requests.fetch_add(1, Ordering::Relaxed);
        UnconfiguredRemoteExecutionClient::testing_new_dummy()
    }

    fn invocation_re_use_case(&self) -> RemoteExecutorUseCase {
        RemoteExecutorUseCase::buck2_default()
    }

    fn cas_configured(&self) -> bool {
        unimplemented!()
    }

    fn may_probe_cas(&self) -> bool {
        unimplemented!()
    }

    fn skip_cache_read(&self) -> bool {
        unimplemented!()
    }

    fn write_action_cache(&self) -> Option<&dyn WriteActionCache> {
        Some(&self.write_action_cache)
    }

    fn low_pass_filter(&self) -> &LowPassFilter {
        &self.gate
    }

    fn re_platform(&self) -> &remote_execution::Platform {
        unimplemented!()
    }

    fn digest_config(&self) -> DigestConfig {
        DigestConfig::testing_default()
    }

    fn run_action_knobs(&self) -> &RunActionKnobs {
        &self.knobs
    }

    fn dep_file_store(&self) -> Option<&dyn DepFileStore> {
        unimplemented!()
    }

    fn incremental_store(&self) -> Option<&dyn IncrementalStore> {
        unimplemented!()
    }

    fn dep_file_cache(&self) -> &dyn DepFileCache {
        unimplemented!()
    }

    fn cancellation_context(&self) -> &CancellationContext {
        unimplemented!()
    }

    fn io_provider(&self) -> &dyn IoProvider {
        unimplemented!()
    }

    fn http_client(&self) -> &HttpClient {
        unimplemented!()
    }

    fn output_trees_download_config(&self) -> &OutputTreesDownloadConfig {
        unimplemented!()
    }
}

#[tokio::test]
async fn test_content_is_made_here_when_the_executor_configuration_publishes_no_results()
-> buck2_error::Result<()> {
    // An executor configuration with cache uploads disabled must not write to the CAS, even when
    // the low-pass filter for local work has no free permit. Run a write and a copy of a source
    // file through `produce` with a low-pass filter that has no permits, a write action cache
    // that does not populate and a disk-state materializer. Check that the write action cache
    // records no upload. Check that nothing calls `re_client`, because `produce` calls it only to
    // upload source files. Check that both files are on disk, that the materializer receives both
    // paths through `declare_existing`, and that nothing is declared as CAS content.
    let temp = ProjectRootTemp::new()?;
    let project = temp.path();
    let ctx = TestCtx {
        fs: ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell".to_owned())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2".to_owned(),
            )),
            project.dupe(),
        ),
        materializer: RecordingMaterializer::default(),
        blocking_executor: DummyBlockingExecutor { fs: project.dupe() },
        gate: LowPassFilter::new(0),
        write_action_cache: RecordingWriteCache {
            populates: false,
            uploads: Mutex::new(Vec::new()),
        },
        knobs: RunActionKnobs::default(),
        re_client_requests: AtomicUsize::new(0),
    };

    // The write is made on disk.
    let write = ProjectRelativePathBuf::unchecked_new("buck-out/v2/write.txt".to_owned());
    let (_value, kind) = produce_write(
        &ctx,
        WriteRequest {
            path: write.clone(),
            content: b"written".to_vec(),
            is_executable: false,
            path_kind: BuckOutPathKind::Configuration,
        },
    )
    .await?;
    assert!(matches!(kind, ActionExecutionKind::Simple), "{kind}");
    assert_eq!(
        fs_util::read_to_string(project.resolve(&write)).categorize_internal()?,
        "written"
    );

    // The copy of the source file is made on disk. The test passes `uploadable` true, as `copy`
    // and `assembled_dir` do when the CAS presence check succeeds.
    let src = ProjectRelativePathBuf::unchecked_new("src.txt".to_owned());
    fs_util::write(project.resolve(&src), "copied").categorize_internal()?;
    let dest = ProjectRelativePathBuf::unchecked_new("buck-out/v2/copy/src.txt".to_owned());
    let copy = SourceCopy {
        src,
        dest: dest.clone(),
        value: ArtifactValue::file(FileMetadata {
            digest: TrackedFileDigest::from_content(
                b"copied",
                ctx.digest_config().cas_digest_config(),
            ),
            is_executable: false,
        }),
        executable_bit_override: None,
    };
    let kind = produce_copies(&ctx, vec![copy], true).await?;
    assert!(matches!(kind, ActionExecutionKind::Simple), "{kind}");
    assert_eq!(
        fs_util::read_to_string(project.resolve(&dest)).categorize_internal()?,
        "copied"
    );

    // Nothing was uploaded or declared as CAS content. `declare_existing` received both files.
    assert_eq!(
        *ctx.write_action_cache.uploads.lock(),
        Vec::<TrackedFileDigest>::new()
    );
    assert_eq!(ctx.re_client_requests.load(Ordering::Relaxed), 0);
    assert_eq!(
        *ctx.materializer.declared_existing.lock(),
        vec![write, dest]
    );
    assert_eq!(
        *ctx.materializer.declared_cas.lock(),
        Vec::<ProjectRelativePathBuf>::new()
    );
    Ok(())
}
