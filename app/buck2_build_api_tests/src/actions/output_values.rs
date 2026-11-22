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
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use buck2_analysis::analysis::calculation::AnalysisKey;
use buck2_build_api::actions::RegisteredAction;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_build_api::actions::calculation::ActionCalculation;
use buck2_build_api::actions::execute::action_executor::HasActionExecutor;
use buck2_build_api::actions::impls::run_action_knobs::RunActionKnobs;
use buck2_build_api::analysis::AnalysisResult;
use buck2_build_api::analysis::registry::AnalysisRegistry;
use buck2_build_api::artifact_groups::calculation::ArtifactGroupCalculation;
use buck2_build_api::interpreter::rule_defs::context::AnalysisContext;
use buck2_build_api::interpreter::rule_defs::provider::collection::ProviderCollection;
use buck2_build_api::interpreter::rule_defs::register_rule_defs;
use buck2_common::cas_digest::DigestAlgorithm;
use buck2_common::file_ops::metadata::FileMetadata;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::configuration::compatibility::ResultMaybeCompatible;
use buck2_core::configuration::pair::ConfigurationNoExec;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::execution_types::execution::ExecutionPlatform;
use buck2_core::execution_types::execution::ExecutionPlatformResolution;
use buck2_core::execution_types::executor_config::CommandExecutorConfig;
use buck2_core::fs::project::ProjectRootTemp;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::dispatch::with_dispatcher_async;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::digest_config::SetDigestConfig;
use buck2_execute::execute::manager::CommandExecutionManager;
use buck2_execute::execute::prepared::PreparedCommand;
use buck2_execute::execute::prepared::PreparedCommandOptionalExecutor;
use buck2_execute::execute::request::CommandExecutionOutput;
use buck2_execute::execute::result::CommandExecutionResult;
use buck2_execute::execute::testing_dry_run::DryRunEntry;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_hash::BuckFixedSortedMap;
use buck2_hash::BuckMutInsertionMap;
use buck2_interpreter::file_type::StarlarkFileType;
use buck2_interpreter::from_freeze::from_freeze_error;
use buck2_interpreter::testing::Buck2TestHeapName;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use dice::DiceTransaction;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use httptest::Expectation;
use httptest::matchers::request;
use httptest::responders::status_code;
use indoc::formatdoc;
use indoc::indoc;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::syntax::AstModule;

use crate::actions::calculation::create_test_configured_target_label;
use crate::actions::calculation::make_dice_state;

const CONTENT: &str = "hello world";
const CONTENT_SHA1: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
const CONTENT_SHA256: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

#[derive(Default)]
struct FakeActionCache {
    /// When `entry` is set, `look_up_digests_only` returns it as the value of each output.
    entry: Option<ArtifactValue>,
    /// `lookups` counts the calls of `look_up_digests_only`.
    lookups: AtomicUsize,
}

#[async_trait]
impl PreparedCommandOptionalExecutor for FakeActionCache {
    async fn maybe_execute(
        &self,
        _command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        _cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        ControlFlow::Continue(manager)
    }

    fn lookups_can_hit(&self) -> bool {
        true
    }

    async fn look_up_digests_only(
        &self,
        command: &PreparedCommand<'_, '_>,
    ) -> buck2_error::Result<Option<BuckMutInsertionMap<CommandExecutionOutput, ArtifactValue>>>
    {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        Ok(self.entry.as_ref().map(|value| {
            command
                .request
                .outputs()
                .map(|output| (output.cloned(), value.dupe()))
                .collect()
        }))
    }
}

fn file_value(content: &str) -> ArtifactValue {
    ArtifactValue::file(FileMetadata {
        digest: TrackedFileDigest::from_content(
            content.as_bytes(),
            DigestConfig::testing_default().cas_digest_config(),
        ),
        is_executable: false,
    })
}

/// Runs the function `test` in `content` as the analysis of `cell//pkg:foo`, with `cache` as the
/// action cache of the executors. The returned DICE state has `action_cache.digests_only_entries`
/// set, and the returned list records the commands that the executors run.
async fn analyze(
    content: &str,
    temp_fs: &ProjectRootTemp,
    cache: Arc<FakeActionCache>,
) -> buck2_error::Result<(
    DiceTransaction,
    Vec<Arc<RegisteredAction>>,
    Arc<Mutex<Vec<DryRunEntry>>>,
)> {
    analyze_with_digest_config(content, temp_fs, DigestConfig::testing_default(), cache).await
}

async fn analyze_with_digest_config(
    content: &str,
    temp_fs: &ProjectRootTemp,
    digest_config: DigestConfig,
    cache: Arc<FakeActionCache>,
) -> buck2_error::Result<(
    DiceTransaction,
    Vec<Arc<RegisteredAction>>,
    Arc<Mutex<Vec<DryRunEntry>>>,
)> {
    let label = create_test_configured_target_label();
    let values = Module::with_temp_heap(|env| {
        let globals = GlobalsBuilder::standard().with(register_rule_defs).build();
        let registry = AnalysisRegistry::new_from_owner(
            BaseDeferredKey::TargetLabel(label.dupe()),
            ExecutionPlatformResolution::new_for_testing(
                Some(ExecutionPlatform::legacy_execution_platform(
                    CommandExecutorConfig::testing_local(),
                    ConfigurationNoExec::testing_new(),
                )),
                Vec::new(),
            ),
        )?;
        let (ctx, providers) = {
            let mut eval = Evaluator::new(&env);
            let ast = AstModule::parse(
                "test.bzl",
                content.to_owned(),
                &StarlarkFileType::Bzl.dialect(false),
            )?;
            eval.eval_module(ast, &globals)?;
            let test = env.get("test").expect("the module defines `test`");
            let ctx = AnalysisContext::prepare(
                eval.heap(),
                None,
                Some(label.dupe()),
                None,
                registry,
                digest_config,
            );
            let providers = eval.eval_function(test, &[ctx.to_value()], &[])?;
            (ctx, providers)
        };
        let registry = ctx.take_state();
        registry.analysis_value_storage.set_result_value(
            env.heap()
                .alloc(ProviderCollection::try_from_value(providers)?)
                .get(),
        )?;
        let finalize = registry.finalize(&env)?;
        let frozen = env
            .freeze_named(Buck2TestHeapName::frozen_heap_name())
            .map_err(from_freeze_error)?;
        finalize(&frozen)
    })?;
    let actions = values
        .iter_actions()
        .map(|action| action.map(Arc::dupe))
        .collect::<buck2_error::Result<Vec<_>>>()?;
    let node = ConfiguredTargetNode::testing_new(
        label.dupe(),
        "foo_lib",
        ExecutionPlatformResolution::new_for_testing(None, Vec::new()),
        vec![],
        None,
    );
    let result = AnalysisResult::new(
        values,
        None,
        BuckFixedSortedMap::default(),
        0,
        0,
        None,
        Vec::new(),
        node.rule_type().dupe(),
    )?;
    let commands = Arc::new(Mutex::new(Vec::new()));
    let dice = make_dice_state(
        commands.dupe(),
        temp_fs,
        cache,
        RunActionKnobs {
            digests_only_cache: true,
            ..Default::default()
        },
        vec![Box::new(move |builder| {
            // `make_dice_state` sets the default digest config before it runs the mocks, so this
            // call replaces it.
            builder
                .set_data(|data| data.set_digest_config(digest_config))
                .mock_and_return(
                    AnalysisKey(label),
                    ResultMaybeCompatible::Compatible(result),
                )
        })],
    )
    .await?;
    Ok((dice, actions, commands))
}

/// Checks that the output values of each action in `actions` equal the outputs of a build of the
/// action. Returns the names of the actions that have output values.
async fn check_output_values(
    dice: &DiceTransaction,
    actions: &[Arc<RegisteredAction>],
) -> buck2_error::Result<Vec<String>> {
    let mut ctx = dice.ctx();
    let mut names = Vec::new();
    for action in actions {
        let mut inputs = BuckMutInsertionMap::default();
        for group in action.inputs()?.iter() {
            inputs.insert(group.dupe(), ctx.ensure_artifact_group(group).await?);
        }
        let executor = ctx
            .get_action_executor(&CommandExecutorConfig::testing_local())
            .await?;
        // The output values are computed before the build of the action, so they can't depend on
        // anything that `execute` writes.
        if let Some(outputs) = executor.output_values(inputs, action).await? {
            assert!(
                action.supports_output_values(),
                "`{}` has output values",
                action.name()
            );
            let executed = ActionCalculation::build_action(&mut ctx, action.key()).await?;
            assert_eq!(&outputs, executed, "outputs of `{}`", action.name());
            names.push(action.name());
        }
    }
    Ok(names)
}

#[tokio::test]
async fn test_output_values_write_and_copy() -> buck2_error::Result<()> {
    // The output values of writes, copies and directories equal the outputs that `execute`
    // returns. Declare an executable write, writes with the path of the executable write in their
    // content, a copy, a symlink, a symlinked directory and a copied directory. Every output has a
    // content-based path. Expect output values for every action, equal to the outputs of the build.
    buck2_certs::certs::maybe_setup_cryptography();
    // `write_json` checks the stack depth against a range that buck2 records only on its own
    // threads, so the check fails on the test thread. The current-thread runtime of the test runs
    // the DICE tasks on the test thread too.
    let _stack_guard = buck2_util::threads::ignore_stack_overflow_checks_for_current_thread();
    let temp_fs = ProjectRootTemp::new()?;
    let content = indoc!(
        r#"
        def test(ctx):
            file = ctx.actions.write("file.txt", "hello", is_executable = True)
            ctx.actions.write("paths.txt", cmd_args(file))
            ctx.actions.write_json("paths.json", [file])
            ctx.actions.copy_file("copy.txt", file)
            link = ctx.actions.symlink_file("link", file)
            dir = ctx.actions.symlinked_dir("dir", {"file.txt": file, "link": link})
            ctx.actions.copied_dir("copied", {"dir": dir})
            return [DefaultInfo()]
        "#
    );

    with_dispatcher_async(EventDispatcher::null(), async {
        let (dice, actions, _) = analyze(content, &temp_fs, Arc::default()).await?;
        assert_eq!(
            check_output_values(&dice, &actions).await?,
            [
                "write file.txt",
                "write paths.txt",
                "write_json paths.json",
                "copy copy.txt",
                "copy link",
                "symlinked_dir dir",
                "symlinked_dir copied",
            ],
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn test_output_values_download_file() -> buck2_error::Result<()> {
    // A download has output values when it declares a size and a checksum in the algorithm that
    // `execute` picks: sha1 when the digest config allows sha1, and sha256 otherwise. Declare a
    // download with a sha1 and a size, an executable download with a sha256 and a size, and a
    // download with a sha1, a sha256 and no size. Analyze the downloads under a sha1-only and a
    // sha256-only digest config. Expect output values only for the download with the checksum of
    // the digest config, equal to the outputs of the build. The server fails the test on any
    // request other than the GET of each build, for example on a HEAD request for the size.
    buck2_certs::certs::maybe_setup_cryptography();
    let server = httptest::Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/file"))
            .times(2)
            .respond_with(status_code(200).body(CONTENT)),
    );
    let content = formatdoc!(
        r#"
        def test(ctx):
            ctx.actions.download_file("sha1", "{url}", sha1 = "{sha1}", size_bytes = {size})
            ctx.actions.download_file(
                "sha256",
                "{url}",
                sha256 = "{sha256}",
                size_bytes = {size},
                is_executable = True,
            )
            ctx.actions.download_file("no_size", "{url}", sha1 = "{sha1}", sha256 = "{sha256}")
            return [DefaultInfo()]
        "#,
        url = server.url_str("/file"),
        sha1 = CONTENT_SHA1,
        sha256 = CONTENT_SHA256,
        size = CONTENT.len(),
    );
    let sha256_only = DigestConfig::leak_new(vec![DigestAlgorithm::Sha256], None)?;

    with_dispatcher_async(EventDispatcher::null(), async {
        for (digest_config, downloaded) in [
            (DigestConfig::testing_default(), "download_file sha1"),
            (sha256_only, "download_file sha256"),
        ] {
            let temp_fs = ProjectRootTemp::new()?;
            let (dice, actions, _) =
                analyze_with_digest_config(&content, &temp_fs, digest_config, Arc::default())
                    .await?;
            assert_eq!(check_output_values(&dice, &actions).await?, [downloaded]);
        }
        Ok(())
    })
    .await
}

const PRODUCER_AND_CONSUMER: &str = indoc!(
    r#"
    def test(ctx):
        produced = ctx.actions.declare_output("produced", has_content_based_path = False)
        ctx.actions.run(["produce", produced.as_output()], category = "produce")
        consumed = ctx.actions.declare_output("consumed", has_content_based_path = False)
        ctx.actions.run(["consume", produced, consumed.as_output()], category = "consume")
        return [DefaultInfo()]
    "#
);

#[tokio::test]
async fn test_output_values_of_run_actions() -> buck2_error::Result<()> {
    // `OutputValuesKey` takes the outputs of run actions from their digests-only entries, so it
    // runs no command and declares nothing in the materializer. Declare the run actions of
    // `PRODUCER_AND_CONSUMER`, and give every action a digests-only entry with the same file for
    // each output. Expect the file of the entry as the output of the consumer, a lookup for each
    // action, no command and no artifact in the materializer at the output paths.
    buck2_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;

    with_dispatcher_async(EventDispatcher::null(), async {
        let entry = file_value("entry");
        let cache = Arc::new(FakeActionCache {
            entry: Some(entry.dupe()),
            ..Default::default()
        });
        let (dice, actions, commands) =
            analyze(PRODUCER_AND_CONSUMER, &temp_fs, cache.dupe()).await?;
        let consumer = &actions[1];
        let mut ctx = dice.ctx();
        assert_eq!(
            ActionCalculation::output_values(&mut ctx, consumer.key())
                .await?
                .get(consumer.first_output().get_path())?,
            Some(&entry),
        );
        assert_eq!(cache.lookups.load(Ordering::Relaxed), 2);
        assert!(commands.lock().unwrap().is_empty());
        let artifact_fs = ctx.get_artifact_fs().await?;
        let materializer = ctx.per_transaction_data().get_materializer();
        for action in &actions {
            let path = artifact_fs.resolve_build(action.first_output().get_path(), None)?;
            assert!(!materializer.has_artifact_at(path).await?);
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn test_output_values_projects_looked_up_values() -> buck2_error::Result<()> {
    // `OutputValuesKey` projects the looked-up value of a directory like a build projects the
    // built value, with the content-based path hash of the directory. Declare a copied directory
    // with a content-based path and a symlink to a file in the directory. The value of the symlink
    // contains the value and the path of the file, and the path contains the hash. Expect that
    // `OutputValuesKey` of the symlink doesn't build the symlink, and that the looked-up value of
    // the symlink equals its value after a build.
    buck2_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;
    let content = indoc!(
        r#"
        def test(ctx):
            src = ctx.actions.write("src.txt", "src")
            dir = ctx.actions.copied_dir("dir", {"sub/file": src})
            ctx.actions.symlink_file(
                "link",
                dir.project("sub/file"),
                has_content_based_path = False,
            )
            return [DefaultInfo()]
        "#
    );

    with_dispatcher_async(EventDispatcher::null(), async {
        let (dice, actions, _) = analyze(content, &temp_fs, Arc::default()).await?;
        let link = &actions[2];
        let output = link.first_output().get_path();
        let mut ctx = dice.ctx();
        let looked_up = ActionCalculation::output_values(&mut ctx, link.key())
            .await?
            .get(output)?;
        let path = ctx.get_artifact_fs().await?.resolve_build(output, None)?;
        let materializer = ctx.per_transaction_data().get_materializer();
        assert!(!materializer.has_artifact_at(path.clone()).await?);
        assert_eq!(
            looked_up,
            ActionCalculation::build_action(&mut ctx, link.key())
                .await?
                .get(output)
        );
        assert!(materializer.has_artifact_at(path).await?);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn test_output_values_falls_back_to_build() -> buck2_error::Result<()> {
    // Without a digests-only entry, `OutputValuesKey` of a run action builds the action. Declare
    // the run actions of `PRODUCER_AND_CONSUMER` and give the executors an action cache without
    // entries. Expect a lookup, a command, and the outputs of the build for the producer.
    buck2_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;

    with_dispatcher_async(EventDispatcher::null(), async {
        let cache = Arc::new(FakeActionCache::default());
        let (dice, actions, commands) =
            analyze(PRODUCER_AND_CONSUMER, &temp_fs, cache.dupe()).await?;
        let key = actions[0].key();
        let output = actions[0].first_output().get_path();
        let mut ctx = dice.ctx();
        let looked_up = ActionCalculation::output_values(&mut ctx, key)
            .await?
            .get(output)?;
        assert_eq!(cache.lookups.load(Ordering::Relaxed), 1);
        assert_eq!(commands.lock().unwrap().len(), 1);
        assert_eq!(
            looked_up,
            ActionCalculation::build_action(&mut ctx, key)
                .await?
                .get(output)
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn test_run_actions_without_output_values() -> buck2_error::Result<()> {
    // `supports_output_values` of a run action is false when the action digest doesn't determine
    // the outputs, when preparing the action writes a file, and with `allow_cache_upload = False`.
    // Declare a plain run action and run actions with `no_outputs_cleanup`, a worker,
    // `metadata_param` and `allow_cache_upload = False`. Expect it to be true only for the plain
    // run action.
    buck2_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;
    let content = indoc!(
        r#"
        def test(ctx):
            def run(category, **kwargs):
                out = ctx.actions.declare_output(category)
                ctx.actions.run(["run", out.as_output()], category = category, **kwargs)
            run("plain")
            run("incremental", no_outputs_cleanup = True)
            run("worker", exe = WorkerRunInfo(worker = WorkerInfo(exe = ["worker"])))
            run("metadata", metadata_env_var = "METADATA", metadata_path = "metadata.json")
            run("not_uploaded", allow_cache_upload = False)
            return [DefaultInfo()]
        "#
    );

    with_dispatcher_async(EventDispatcher::null(), async {
        let (_, actions, _) = analyze(content, &temp_fs, Arc::default()).await?;
        let supported: Vec<_> = actions
            .iter()
            .filter(|action| action.supports_output_values())
            .map(|action| action.name())
            .collect();
        assert_eq!(supported, ["plain"]);
        Ok(())
    })
    .await
}
