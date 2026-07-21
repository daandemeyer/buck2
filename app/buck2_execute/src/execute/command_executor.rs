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
use std::time::Duration;

use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::execution_types::executor_config::CommandGenerationOptions;
use buck2_core::execution_types::executor_config::ExecutorNetworkAccess;
use buck2_core::execution_types::executor_config::OutputPathsBehavior;
use buck2_core::execution_types::executor_config::ReGangWorker;
use buck2_core::execution_types::executor_config::RemoteExecutorCafFbpkg;
use buck2_core::execution_types::executor_config::RemoteExecutorCustomImage;
use buck2_core::execution_types::executor_config::RemoteExecutorDependency;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
use buck2_core::fs::buck_out_path::BuckOutNamespace;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_data::NetworkAccess;
use buck2_directory::directory::fingerprinted_directory::FingerprintedDirectory;
use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use remote_execution as RE;
use remote_execution::TActionResult2;
use sorted_vector_map::SortedVectorMap;

use super::cache_uploader::CacheUploadResults;
use super::cell_execution_view::CellExecutionView;
use super::cell_execution_view::CellExecutionViewRequirements;
use super::cell_execution_view::prepare_materialized_cell_execution_view;
use crate::artifact::fs::ExecutorFs;
use crate::digest::CasDigestToReExt;
use crate::digest_config::DigestConfig;
use crate::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use crate::execute::action_digest_and_blobs::ActionDigestAndBlobsBuilder;
use crate::execute::cache_uploader::CacheUploadInfo;
use crate::execute::cache_uploader::IntoRemoteDepFile;
use crate::execute::cache_uploader::UploadCache;
use crate::execute::executor_stage;
use crate::execute::manager::CommandExecutionManager;
use crate::execute::prepared::PreparedAction;
use crate::execute::prepared::PreparedCommand;
use crate::execute::prepared::PreparedCommandExecutor;
use crate::execute::prepared::PreparedCommandOptionalExecutor;
use crate::execute::request::CommandExecutionRequest;
use crate::execute::request::ExecutorPreference;
use crate::execute::request::OutputType;
use crate::execute::request::RemoteWorkerSpec;
use crate::execute::result::CommandExecutionMetadata;
use crate::execute::result::CommandExecutionResult;

fn validate_canonical_working_directory(
    artifact_fs: &ArtifactFs,
    request: &CommandExecutionRequest,
) -> buck2_error::Result<()> {
    let working_directory = request.working_directory();
    if artifact_fs.cell_source_path_mode() == CellSourcePathMode::Physical
        || working_directory.is_empty()
    {
        return Ok(());
    }

    if let Some(cell_path) = artifact_fs.decode_source_execution_path(working_directory)? {
        if cell_path.path().is_empty() && request.executor_preference().requires_local() {
            return Ok(());
        }
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Canonical cell execution paths only support an exact cell-root working directory for a local-only request; `{}` is in cell `{}`",
            working_directory,
            cell_path.cell(),
        ));
    }

    if artifact_fs
        .buck_out_path_resolver()
        .classify_namespace(working_directory)
        .is_some_and(|namespace| {
            namespace.is_generated() && namespace != BuckOutNamespace::OfflineCache
        })
    {
        // A declared output whose parents are created before execution is the only proof that the
        // cwd will exist remotely too: elsewhere local resolution succeeds while the RE Merkle
        // tree lacks the directory.
        for output in request.outputs() {
            let resolved = output.resolve_configuration_hash_path(artifact_fs)?;
            if resolved
                .path_to_create()
                .is_some_and(|path| path.starts_with(working_directory))
            {
                return Ok(());
            }
        }
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Canonical cell execution paths support a generated working directory `{working_directory}` only when it contains a declared output of the request",
        ));
    }

    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "Canonical cell execution paths do not support physical source or arbitrary Buck-out working directory `{working_directory}`; use the project root, an exact canonical cell root for a local-only request, or a declared generated directory",
    ))
}

#[derive(Copy, Dupe, Clone, Debug, PartialEq, Eq)]
pub struct ActionExecutionTimingData {
    pub wall_time: Duration,
}

impl Default for ActionExecutionTimingData {
    fn default() -> Self {
        Self {
            wall_time: Duration::ZERO,
        }
    }
}

impl From<CommandExecutionMetadata> for ActionExecutionTimingData {
    fn from(command: CommandExecutionMetadata) -> Self {
        Self {
            wall_time: command.time_span.duration(),
        }
    }
}

#[derive(Clone, Dupe)]
pub struct CommandExecutor(Arc<CommandExecutorData>);

struct CommandExecutorData {
    inner: Arc<dyn PreparedCommandExecutor>,
    action_cache_checker: Arc<dyn PreparedCommandOptionalExecutor>,
    remote_dep_file_cache_checker: Arc<dyn PreparedCommandOptionalExecutor>,
    artifact_fs: ArtifactFs,
    options: CommandGenerationOptions,
    re_platform: RE::Platform,
    cache_uploader: Arc<dyn UploadCache>,
    cell_execution_view: Option<Arc<dyn CellExecutionView>>,
}

impl CommandExecutor {
    pub fn new(
        inner: Arc<dyn PreparedCommandExecutor>,
        action_cache_checker: Arc<dyn PreparedCommandOptionalExecutor>,
        remote_dep_file_cache_checker: Arc<dyn PreparedCommandOptionalExecutor>,
        cache_uploader: Arc<dyn UploadCache>,
        artifact_fs: ArtifactFs,
        options: CommandGenerationOptions,
        re_platform: RE::Platform,
    ) -> Self {
        Self::new_with_cell_execution_view(
            inner,
            action_cache_checker,
            remote_dep_file_cache_checker,
            cache_uploader,
            artifact_fs,
            options,
            re_platform,
            None,
        )
    }

    pub fn new_with_cell_execution_view(
        inner: Arc<dyn PreparedCommandExecutor>,
        action_cache_checker: Arc<dyn PreparedCommandOptionalExecutor>,
        remote_dep_file_cache_checker: Arc<dyn PreparedCommandOptionalExecutor>,
        cache_uploader: Arc<dyn UploadCache>,
        artifact_fs: ArtifactFs,
        options: CommandGenerationOptions,
        re_platform: RE::Platform,
        cell_execution_view: Option<Arc<dyn CellExecutionView>>,
    ) -> Self {
        Self(Arc::new(CommandExecutorData {
            inner,
            action_cache_checker,
            remote_dep_file_cache_checker,
            artifact_fs,
            options,
            re_platform,
            cache_uploader,
            cell_execution_view,
        }))
    }

    pub fn fs(&self) -> &ArtifactFs {
        &self.0.artifact_fs
    }

    pub fn executor_fs(&self) -> ExecutorFs<'_> {
        ExecutorFs::new(&self.0.artifact_fs, self.0.options.path_separator)
    }

    pub fn re_platform(&self) -> &RE::Platform {
        &self.0.re_platform
    }

    /// The host-consumer handoff used outside the normal local executor, for inputs the caller has
    /// already materialized.
    pub fn prepare_materialized_execution_view(
        &self,
        request: &CommandExecutionRequest,
        requirements: CellExecutionViewRequirements,
    ) -> buck2_error::Result<()> {
        validate_canonical_working_directory(&self.0.artifact_fs, request)?;
        prepare_materialized_cell_execution_view(
            self.0.cell_execution_view.as_deref(),
            &self.0.artifact_fs,
            requirements,
            Some(request.working_directory()),
        )
    }

    /// Check if the action can be served by the action cache.
    pub async fn action_cache(
        &self,
        manager: CommandExecutionManager,
        prepared_command: &PreparedCommand<'_, '_>,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        self.0
            .action_cache_checker
            .maybe_execute(prepared_command, manager, cancellations)
            .await
    }

    pub async fn remote_dep_file_cache(
        &self,
        manager: CommandExecutionManager,
        prepared_command: &PreparedCommand<'_, '_>,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        self.0
            .remote_dep_file_cache_checker
            .maybe_execute(prepared_command, manager, cancellations)
            .await
    }

    pub async fn cache_upload(
        &self,
        info: &CacheUploadInfo<'_>,
        execution_result: &CommandExecutionResult,
        re_result: Option<TActionResult2>,
        dep_file_bundle: Option<&mut dyn IntoRemoteDepFile>,
        action_digest_and_blobs: &ActionDigestAndBlobs,
    ) -> buck2_error::Result<CacheUploadResults> {
        self.0
            .cache_uploader
            .upload(
                info,
                execution_result,
                re_result,
                dep_file_bundle,
                action_digest_and_blobs,
            )
            .await
    }

    /// Execute a command.
    ///
    /// This intentionally does not return a Result since we want to capture information about the
    /// execution even if there are errors. Any errors can be propagated by converting them
    /// to a result with CommandExecutionManager::error.
    pub async fn exec_cmd(
        &self,
        manager: CommandExecutionManager,
        prepared_command: &PreparedCommand<'_, '_>,
        cancellations: &CancellationContext,
    ) -> CommandExecutionResult {
        self.0
            .inner
            .exec_cmd(prepared_command, manager, cancellations)
            .await
    }

    pub fn is_local_execution_possible(&self, executor_preference: ExecutorPreference) -> bool {
        self.0
            .inner
            .is_local_execution_possible(executor_preference)
    }

    pub fn is_full_hybrid_enabled(&self) -> bool {
        self.0.inner.is_full_hybrid_enabled()
    }

    pub fn prepare_action(
        &self,
        request: &CommandExecutionRequest,
        digest_config: DigestConfig,
        re_outputs_required: bool,
    ) -> buck2_error::Result<PreparedAction> {
        executor_stage(buck2_data::PrepareAction {}, || {
            // Validate only: action construction and cache lookup must stay side-effect free, so
            // realizing the view is left to whichever consumer needs the paths.
            validate_canonical_working_directory(&self.0.artifact_fs, request)?;
            let input_digest = request.paths().input_directory().fingerprint();

            let mut platform = self.0.re_platform.clone();
            let all_args = if self.0.options.use_bazel_protocol_remote_persistent_workers
                && let Some(worker) = request.worker()
                && let Some(key) = worker.remote_key.as_ref()
            {
                platform.properties.push(RE::Property {
                    name: "persistentWorkerKey".to_owned(),
                    value: key.to_string(),
                });
                // TODO[AH] Ideally, Buck2 could generate an argfile on the fly.
                for arg in request.args() {
                    if !(arg.starts_with("@")
                        || arg.starts_with("-flagfile")
                        || arg.starts_with("--flagfile"))
                    {
                        return Err(buck2_error!(
                            buck2_error::ErrorTag::Input,
                            "Remote persistent worker arguments must be passed as `@argfile`, `-flagfile=argfile`, or `--flagfile=argfile`."
                        ));
                    }
                }
                worker
                    .exe
                    .iter()
                    .chain(request.args().iter())
                    .cloned()
                    .collect()
            } else {
                request.all_args_vec()
            };
            let network_access = request
                .network_access()
                .map(ExecutorNetworkAccess::from)
                .or(self.0.options.network_access);
            let action = re_create_action(
                request.args().to_vec(),
                all_args,
                request.paths().output_paths(),
                request.working_directory(),
                request.env(),
                input_digest,
                request.timeout(),
                platform,
                false,
                digest_config,
                self.0.options.output_paths_behavior,
                network_access,
                request.unique_input_inodes(),
                request.remote_execution_dependencies(),
                request.re_gang_workers(),
                request.remote_execution_custom_image(),
                &request
                    .meta_internal_extra_params()
                    .remote_execution_caf_fbpkgs,
                request.remote_worker(),
                re_outputs_required,
                request
                    .meta_internal_extra_params()
                    .allow_unsandboxed_action_cache_uploads,
            )?;

            buck2_error::Ok(action)
        })
    }
}

#[cfg(test)]
mod tests {

    use buck2_core::cells::cell_path::CellPath;
    use buck2_core::cells::name::CellName;
    use buck2_core::cells::paths::CellRelativePathBuf;
    use buck2_core::fs::buck_out_path::BuckOutTestPath;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
    use buck2_hash::BuckIndexSet;
    use sorted_vector_map::SortedVectorMap;

    use std::sync::Mutex;

    use super::*;
    use crate::artifact::artifact_dyn::ArtifactDyn;
    use crate::artifact::group::artifact_group_values_dyn::ArtifactGroupValuesDyn;
    use crate::artifact_value::ArtifactValue;
    use crate::execute::cache_uploader::NoOpCacheUploader;
    use crate::execute::cell_execution_view::collect_canonical_source_inputs;
    use crate::execute::prepared::NoOpCommandOptionalExecutor;
    use crate::execute::request::CommandExecutionInput;
    use crate::execute::request::CommandExecutionOutput;
    use crate::execute::request::CommandExecutionPaths;
    use crate::execute::request::OutputCreationBehavior;
    use crate::execute::request::WorkerId;
    use crate::execute::request::WorkerSpec;
    use crate::execute::testing_dry_run::DryRunExecutor;

    struct TestSourceArtifact(CellPath);

    impl ArtifactDyn for TestSourceArtifact {
        fn resolve_path(
            &self,
            fs: &ArtifactFs,
            _content_hash: Option<&buck2_core::content_hash::ContentBasedPathHash>,
        ) -> buck2_error::Result<ProjectRelativePathBuf> {
            fs.resolve_cell_path(self.0.as_ref())
        }

        fn resolve_path_for_execution(
            &self,
            fs: &ArtifactFs,
            _content_hash: Option<&buck2_core::content_hash::ContentBasedPathHash>,
        ) -> buck2_error::Result<ProjectRelativePathBuf> {
            fs.resolve_cell_path_for_execution(self.0.as_ref())
        }

        fn resolve_configuration_hash_path(
            &self,
            fs: &ArtifactFs,
        ) -> buck2_error::Result<ProjectRelativePathBuf> {
            self.resolve_path(fs, None)
        }

        fn requires_materialization(&self, _fs: &ArtifactFs) -> bool {
            false
        }

        fn has_content_based_path(&self) -> bool {
            false
        }

        fn is_projected(&self) -> bool {
            false
        }
    }

    struct TestArtifactGroupValues {
        artifact: TestSourceArtifact,
        value: ArtifactValue,
    }

    impl ArtifactGroupValuesDyn for TestArtifactGroupValues {
        fn iter(&self) -> Box<dyn Iterator<Item = (&dyn ArtifactDyn, &ArtifactValue)> + '_> {
            Box::new(std::iter::once((
                &self.artifact as &dyn ArtifactDyn,
                &self.value,
            )))
        }

        fn add_to_directory(
            &self,
            builder: &mut crate::directory::LazyActionDirectoryBuilder,
            artifact_fs: &ArtifactFs,
        ) -> buck2_error::Result<()> {
            crate::directory::insert_artifact_lazy(
                builder,
                self.artifact
                    .resolve_path_for_execution(artifact_fs, None)?,
                &self.value,
            )
        }
    }

    fn canonical_artifact_fs() -> buck2_error::Result<ArtifactFs> {
        Ok(ArtifactFs::testing_new_with_mode_and_external(
            CellSourcePathMode::CanonicalV1,
            &[
                ("root", ""),
                ("local", "workspace/local"),
                ("external", "declared/external"),
            ],
            &["external"],
        ))
    }

    #[test]
    fn canonical_working_directory_policy_is_local_and_lexical() -> buck2_error::Result<()> {
        let fs = canonical_artifact_fs()?;
        let request_with_outputs =
            |cwd: ProjectRelativePathBuf,
             preference: ExecutorPreference,
             outputs: BuckIndexSet<CommandExecutionOutput>| {
                Ok::<_, buck2_error::Error>(
                    CommandExecutionRequest::new(
                        vec!["tool".to_owned()],
                        Vec::new(),
                        CommandExecutionPaths::new(
                            Vec::new(),
                            outputs,
                            &fs,
                            DigestConfig::testing_default(),
                            None,
                        )?,
                        SortedVectorMap::default(),
                    )
                    .with_working_directory(cwd)
                    .with_executor_preference(preference),
                )
            };
        let request = |cwd: ProjectRelativePathBuf, preference: ExecutorPreference| {
            request_with_outputs(cwd, preference, Default::default())
        };

        let logical = fs.resolve_cell_path_for_execution(
            CellPath::new(
                CellName::testing_new("local"),
                CellRelativePathBuf::unchecked_new(String::new()),
            )
            .as_ref(),
        )?;
        let local_physical =
            fs.resolve_cell_source_root_physical(CellName::testing_new("local"))?;
        let external_physical =
            fs.resolve_cell_source_root_physical(CellName::testing_new("external"))?;

        validate_canonical_working_directory(
            &fs,
            &request(logical.clone(), ExecutorPreference::LocalRequired)?,
        )?;
        for path in [logical, local_physical, external_physical] {
            let error = validate_canonical_working_directory(
                &fs,
                &request(path, ExecutorPreference::Default)?,
            )
            .expect_err("remote-capable canonical or physical source cwd must be rejected");
            assert!(error.to_string().contains("working directory"), "{error:#}");
        }

        assert!(
            validate_canonical_working_directory(
                &fs,
                &request(
                    ProjectRelativePathBuf::unchecked_new("app/pkg".to_owned()),
                    ExecutorPreference::LocalRequired,
                )?,
            )
            .is_err()
        );
        // A generated-namespace cwd is accepted only when the request proves the
        // directory exists by declaring an output at or beneath it.
        let test_output = CommandExecutionOutput::TestPath {
            path: BuckOutTestPath::new(
                ForwardRelativePathBuf::unchecked_new("buck-out/v2/test/base".to_owned()),
                ForwardRelativePathBuf::unchecked_new("out.txt".to_owned()),
            ),
            create: OutputCreationBehavior::Parent,
        };
        let mut outputs = BuckIndexSet::default();
        outputs.insert(test_output);
        validate_canonical_working_directory(
            &fs,
            &request_with_outputs(
                ProjectRelativePathBuf::unchecked_new("buck-out/v2/test/base".to_owned()),
                ExecutorPreference::Default,
                outputs,
            )?,
        )?;
        let parent_only_output = CommandExecutionOutput::TestPath {
            path: BuckOutTestPath::new(
                ForwardRelativePathBuf::unchecked_new("buck-out/v2/test/base".to_owned()),
                ForwardRelativePathBuf::unchecked_new("out.txt".to_owned()),
            ),
            create: OutputCreationBehavior::Parent,
        };
        let mut outputs = BuckIndexSet::default();
        outputs.insert(parent_only_output);
        assert!(
            validate_canonical_working_directory(
                &fs,
                &request_with_outputs(
                    ProjectRelativePathBuf::unchecked_new(
                        "buck-out/v2/test/base/out.txt".to_owned()
                    ),
                    ExecutorPreference::Default,
                    outputs,
                )?,
            )
            .is_err(),
            "an output whose parent alone is created must not justify using the output as cwd"
        );
        assert!(
            validate_canonical_working_directory(
                &fs,
                &request(
                    ProjectRelativePathBuf::unchecked_new(
                        "buck-out/v2/gen/root/pkg/output".to_owned()
                    ),
                    ExecutorPreference::Default,
                )?,
            )
            .is_err(),
            "generated cwd without a contained declared output must be rejected"
        );
        assert!(
            validate_canonical_working_directory(
                &fs,
                &request(
                    ProjectRelativePathBuf::unchecked_new("buck-out/v2/offline-cache/x".to_owned()),
                    ExecutorPreference::Default,
                )?,
            )
            .is_err(),
            "offline-cache cwd must be rejected outright"
        );
        Ok(())
    }

    #[derive(Default)]
    struct RecordingView(Mutex<Vec<CellName>>);

    impl CellExecutionView for RecordingView {
        fn prepare(
            &self,
            _artifact_fs: &ArtifactFs,
            requirements: &CellExecutionViewRequirements,
        ) -> buck2_error::Result<()> {
            self.0.lock().unwrap().extend(requirements.cells());
            Ok(())
        }
    }

    #[test]
    fn execution_view_prepares_canonical_worker_inputs() -> buck2_error::Result<()> {
        let artifact_fs = canonical_artifact_fs()?;
        let digest_config = DigestConfig::testing_default();
        let worker_source = CellPath::new(
            CellName::testing_new("local"),
            CellRelativePathBuf::unchecked_new("worker/tool".to_owned()),
        );
        let worker_paths = CommandExecutionPaths::new(
            vec![CommandExecutionInput::Artifact(Box::new(
                TestArtifactGroupValues {
                    artifact: TestSourceArtifact(worker_source),
                    value: ArtifactValue::file(digest_config.empty_file()),
                },
            ))],
            Default::default(),
            &artifact_fs,
            digest_config,
            None,
        )?;
        let request_paths = CommandExecutionPaths::new(
            Vec::new(),
            Default::default(),
            &artifact_fs,
            digest_config,
            None,
        )?;
        let request = CommandExecutionRequest::new(
            vec!["tool".to_owned()],
            Vec::new(),
            request_paths,
            SortedVectorMap::default(),
        )
        .with_worker(Some(WorkerSpec {
            id: WorkerId(1),
            exe: vec!["worker".to_owned()],
            env: SortedVectorMap::default(),
            concurrency: None,
            streaming: false,
            remote_key: None,
            input_paths: worker_paths,
        }));

        let view = Arc::new(RecordingView::default());
        let executor = CommandExecutor::new_with_cell_execution_view(
            Arc::new(DryRunExecutor::new(
                Arc::new(Mutex::new(Vec::new())),
                artifact_fs.clone(),
            )),
            Arc::new(NoOpCommandOptionalExecutor {}),
            Arc::new(NoOpCommandOptionalExecutor {}),
            Arc::new(NoOpCacheUploader {}),
            artifact_fs,
            CommandGenerationOptions {
                path_separator:
                    buck2_core::execution_types::executor_config::PathSeparatorKind::Unix,
                output_paths_behavior: OutputPathsBehavior::Compatibility,
                use_bazel_protocol_remote_persistent_workers: false,
                network_access: None,
            },
            RE::Platform::default(),
            Some(view.clone()),
        );

        executor.prepare_action(&request, digest_config, false)?;
        assert!(
            view.0.lock().unwrap().is_empty(),
            "Action construction and cache selection must not publish the local view",
        );
        let mut requirements =
            collect_canonical_source_inputs(executor.fs(), request.paths().input_directory())?
                .view_requirements;
        if let Some(worker) = request.worker() {
            requirements.merge(
                collect_canonical_source_inputs(
                    executor.fs(),
                    worker.input_paths.input_directory(),
                )?
                .view_requirements,
            );
        }
        executor.prepare_materialized_execution_view(&request, requirements)?;
        assert_eq!(
            *view.0.lock().unwrap(),
            vec![CellName::testing_new("local")],
        );
        Ok(())
    }
}

fn re_create_action(
    args: Vec<String>,
    all_args: Vec<String>,
    outputs: &[(ProjectRelativePathBuf, OutputType)],
    working_directory: &ProjectRelativePath,
    environment: &SortedVectorMap<String, String>,
    input_digest: &TrackedFileDigest,
    timeout: Option<Duration>,
    platform: RE::Platform,
    do_not_cache: bool,
    digest_config: DigestConfig,
    output_paths_behavior: OutputPathsBehavior,
    network_access: Option<ExecutorNetworkAccess>,
    unique_input_inodes: bool,
    remote_execution_dependencies: &Vec<RemoteExecutorDependency>,
    re_gang_workers: &Vec<ReGangWorker>,
    remote_execution_custom_image: &Option<RemoteExecutorCustomImage>,
    remote_execution_caf_fbpkgs: &[RemoteExecutorCafFbpkg],
    worker: &Option<RemoteWorkerSpec>,
    re_outputs_required: bool,
    allow_unsandboxed_action_cache_uploads: bool,
) -> buck2_error::Result<PreparedAction> {
    let (worker_tool_init_action, command_args) = if let Some(worker) = worker {
        let mut action_and_blobs = ActionDigestAndBlobsBuilder::new(digest_config);
        let command = RE::Command {
            arguments: worker.init.clone(),
            #[allow(deprecated)]
            platform: Some(platform.clone()),
            working_directory: working_directory.as_str().to_owned(),
            environment_variables: worker
                .env
                .iter()
                .map(|(k, v)| RE::EnvironmentVariable {
                    name: (*k).clone(),
                    value: (*v).clone(),
                })
                .collect(),
            ..Default::default()
        };
        let input_digest = worker.input_paths.input_directory().fingerprint();

        #[allow(unused_mut)]
        let mut action = RE::Action {
            input_root_digest: Some(input_digest.to_grpc()),
            command_digest: Some(action_and_blobs.add_command(&command).to_grpc()),
            timeout: timeout
                .map(|t| t.try_into())
                .transpose()
                .buck_error_context("Cannot convert timeout to GRPC")?,
            do_not_cache,
            ..Default::default()
        };
        #[cfg(fbcode_build)]
        set_action_network_access(&mut action, network_access);
        let action_and_blobs = action_and_blobs.build(&action);
        (Some(action_and_blobs), args)
    } else {
        (None, all_args)
    };

    let mut command = RE::Command {
        arguments: command_args,
        #[allow(deprecated)]
        platform: Some(platform),
        working_directory: working_directory.as_str().to_owned(),
        environment_variables: environment
            .iter()
            .map(|(k, v)| RE::EnvironmentVariable {
                name: (*k).clone(),
                value: (*v).clone(),
            })
            .collect(),
        ..Default::default()
    };

    match output_paths_behavior {
        OutputPathsBehavior::Compatibility => {
            for (output, output_type) in outputs {
                let path = output.as_str().to_owned();

                #[allow(deprecated)]
                match output_type {
                    OutputType::FileOrDirectory => {
                        command.output_files.push(path.clone());
                        command.output_directories.push(path);
                    }
                    OutputType::File => command.output_files.push(path),
                    OutputType::Directory => command.output_directories.push(path),
                }
            }
        }
        OutputPathsBehavior::Strict => {
            for (output, output_type) in outputs {
                let path = output.as_str().to_owned();

                #[allow(deprecated)]
                match output_type {
                    OutputType::FileOrDirectory => {
                        command.output_files.push(path);
                    }
                    OutputType::File => command.output_files.push(path),
                    OutputType::Directory => command.output_directories.push(path),
                }
            }
        }
        OutputPathsBehavior::OutputPaths => {
            #[cfg(fbcode_build)]
            {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "output_paths is not supported in fbcode_build"
                ));
            }

            #[cfg(not(fbcode_build))]
            {
                for (output, _output_type) in outputs {
                    command.output_paths.push(output.as_str().to_owned());
                }
            }
        }
    }

    let mut action_and_blobs = ActionDigestAndBlobsBuilder::new(digest_config);

    let mut action = RE::Action {
        input_root_digest: Some(input_digest.to_grpc()),
        command_digest: Some(action_and_blobs.add_command(&command).to_grpc()),
        timeout: timeout
            .map(|t| t.try_into())
            .transpose()
            .buck_error_context("Cannot convert timeout to GRPC")?,
        do_not_cache,
        #[cfg(fbcode_build)]
        allow_unsandboxed_action_cache_uploads,
        #[cfg(fbcode_build)]
        worker_tool_action_digest: worker_tool_init_action.clone().map(|a| a.action.to_grpc()),
        ..Default::default()
    };

    #[cfg(fbcode_build)]
    if let Some(custom_image) = remote_execution_custom_image {
        action.caf_image_fbpkg = Some(RE::CafImageFbpkg {
            id: Some(RE::CafFbpkgIdentifier {
                name: custom_image.identifier.name.clone(),
                uuid: custom_image.identifier.uuid.clone(),
                ..Default::default()
            }),
            drop_host_mount_globs: custom_image.drop_host_mount_globs.clone(),
            ..Default::default()
        });
    }

    #[cfg(not(fbcode_build))]
    {
        let _unused = remote_execution_custom_image;
    }

    #[cfg(fbcode_build)]
    {
        action.caf_fbpkgs = remote_execution_caf_fbpkgs
            .iter()
            .map(|caf_fbpkg| RE::CafFbpkg {
                id: Some(RE::CafFbpkgIdentifier {
                    name: caf_fbpkg.name.clone(),
                    uuid: caf_fbpkg.uuid.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();
    }

    #[cfg(not(fbcode_build))]
    {
        let _unused = remote_execution_caf_fbpkgs;
    }

    if unique_input_inodes {
        #[cfg(fbcode_build)]
        {
            action.copy_policy_resolver = RE::CopyPolicyResolver::SingleHardLinking.into();
        }
    }

    #[cfg(fbcode_build)]
    set_action_network_access(&mut action, network_access);

    #[cfg(fbcode_build)]
    {
        action.respect_exec_bit = true;
    }

    #[cfg(fbcode_build)]
    {
        action.outputs_required = re_outputs_required;
    }

    #[cfg(not(fbcode_build))]
    {
        let _unused = &mut action;
        let _unused = re_outputs_required;
        let _unused = allow_unsandboxed_action_cache_uploads;
        let _unused = network_access;
    }

    let action_and_blobs = action_and_blobs.build(&action);

    Ok(PreparedAction {
        action_and_blobs,
        #[allow(deprecated)]
        platform: command
            .platform
            .expect("We did put a platform a few lines up"),
        remote_execution_dependencies: remote_execution_dependencies.to_owned(),
        re_gang_workers: re_gang_workers.to_owned(),
        worker_tool_init_action,
        network_access: network_access.map(NetworkAccess::from),
    })
}

#[cfg(fbcode_build)]
fn set_action_network_access(
    action: &mut RE::Action,
    network_access: Option<ExecutorNetworkAccess>,
) {
    let Some(network_access) = network_access else {
        return;
    };

    action.network_isolation = match network_access {
        ExecutorNetworkAccess::All => RE::NetworkIsolationType::None,
        ExecutorNetworkAccess::None | ExecutorNetworkAccess::Strict => {
            RE::NetworkIsolationType::NetworkStrict
        }
        ExecutorNetworkAccess::Loopback => RE::NetworkIsolationType::Loopback,
        ExecutorNetworkAccess::Private => RE::NetworkIsolationType::Private,
    } as i32;
}
