/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::iter::zip;
use std::sync::Arc;

use allocative::Allocative;
use async_recursion::async_recursion;
use async_trait::async_trait;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_artifact::artifact::artifact_type::ArtifactKind;
use buck2_artifact::artifact::artifact_type::BaseArtifactKind;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_artifact::artifact::source_artifact::SourceArtifact;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::metadata::RawPathMetadata;
use buck2_common::file_ops::metadata::RawSymlink;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::package_listing::dice::DicePackageListingResolver;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
use buck2_core::package::PackageLabel;
use buck2_directory::directory::directory_data::DirectoryData;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::directory::ActionDirectoryBuilder;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionSharedDirectory;
use buck2_execute::directory::INTERNER;
use buck2_execute::directory::extract_artifact_value;
use buck2_execute::directory::insert_artifact;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_fs::paths::relative_path::Component;
use buck2_fs::paths::relative_path::RelativePath;
use buck2_util::size_assert;
use buck2_util::time_span::TimeSpan;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::Future;
use futures::FutureExt;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;
use ref_cast::RefCast;
use smallvec::SmallVec;
use sorted_vector_map::SortedVectorMap;

use crate::actions::artifact::get_artifact_fs::GetArtifactFs;
use crate::actions::calculation::ActionCalculation;
use crate::actions::execute::action_executor::ActionOutputs;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::artifact_groups::ResolvedArtifactGroup;
use crate::artifact_groups::TransitiveSetProjectionKey;
use crate::keep_going::KeepGoing;

#[async_trait]
pub trait ArtifactGroupCalculation {
    /// Makes an 'Artifact' available to be accessed
    async fn ensure_artifact_group(
        &mut self,
        input: &ArtifactGroup,
    ) -> buck2_error::Result<ArtifactGroupValues>;
}

#[async_trait]
impl ArtifactGroupCalculation for DiceComputations<'_> {
    /// makes the 'Artifact' available to be accessed
    async fn ensure_artifact_group(
        &mut self,
        input: &ArtifactGroup,
    ) -> buck2_error::Result<ArtifactGroupValues> {
        // TODO consider if we need to cache this
        let resolved_artifacts = input.resolved_artifact(self).await?;
        ensure_artifact_group_staged(self, resolved_artifacts)
            .await?
            .into_group_values(resolved_artifacts)
    }
}

/// A large build may have many artifact dependency edges and so may have many of the
/// `ensure_build_artifact_*()` futures live at any time. To support this efficiently
/// we provide these `*_staged()` functions that provide an optimized Future implementation
/// for waiting on the dependency edge and an optimized form to represent the result (as
/// we also may have many of those results alive across await points as things need to wait
/// on all dependencies).
///
/// Performance sensitive things should use these staged functions, wait for all their results
/// and then synchronously process them and drop any intermediate data structures before their
/// next yield point.
///
/// Some of the optimizations this provides:
///  - The staged future is kept to a minimum size (which we track in an assertion below).
///  - The result of the staged future is kept to a minimum size (also tracked below).
///  - For the single Artifact case from ensure_artifact_group_staged, we defer allocation
///    of the ArtifactGroupValues until `into_group_values()` is called. For callers waiting
///    on many inputs, this allows them to only allocate those large values only after all
///    inputs are ready.
pub(crate) fn ensure_artifact_group_staged<'a, 'd>(
    ctx: &'a mut DiceComputations<'d>,
    input: ResolvedArtifactGroup<'a>,
) -> impl Future<Output = buck2_error::Result<EnsureArtifactGroupReady>> + use<'a, 'd> {
    match input {
        ResolvedArtifactGroup::Artifact(artifact) => {
            ensure_artifact_staged(ctx, artifact).left_future()
        }
        ResolvedArtifactGroup::TransitiveSetProjection(key) => ctx
            .compute(EnsureTransitiveSetProjectionKey::ref_cast(key))
            .map(|v| Ok(EnsureArtifactGroupReady::TransitiveSet(v??)))
            .right_future(),
    }
}

/// See [ensure_artifact_group_staged].
pub(super) fn ensure_base_artifact_staged<'a, 'd>(
    dice: &'a mut DiceComputations<'d>,
    artifact: &'a BaseArtifactKind,
) -> impl Future<Output = buck2_error::Result<EnsureArtifactGroupReady>> + use<'a, 'd> {
    match artifact {
        BaseArtifactKind::Build(built) => ensure_build_artifact_staged(dice, built).left_future(),
        BaseArtifactKind::Source(source) => {
            ensure_source_artifact_staged(dice, source).right_future()
        }
    }
}

/// See [ensure_artifact_group_staged].
pub(super) fn ensure_artifact_staged<'a, 'd>(
    dice: &'a mut DiceComputations<'d>,
    artifact: &'a Artifact,
) -> impl Future<Output = buck2_error::Result<EnsureArtifactGroupReady>> + use<'a, 'd> {
    let ArtifactKind { base, path } = artifact.data();
    match path.is_empty() {
        true => ensure_base_artifact_staged(dice, base).left_future(),
        false => dice
            .compute(EnsureProjectedArtifactKey::ref_cast(artifact.data()))
            .map(|v| Ok(EnsureArtifactGroupReady::Single(v??)))
            .right_future(),
    }
}

fn ensure_build_artifact_staged<'a, 'd>(
    dice: &'a mut DiceComputations<'d>,
    built: &'a BuildArtifact,
) -> impl Future<Output = buck2_error::Result<EnsureArtifactGroupReady>> + use<'a, 'd> {
    ActionCalculation::build_action(dice, built.key()).map(move |action_outputs| {
        let action_outputs = action_outputs?;
        if let Some(value) = action_outputs.get(built.get_path()) {
            Ok(EnsureArtifactGroupReady::Single(value.dupe()))
        } else {
            Err(
                EnsureArtifactStagedError::BuildArtifactMissing(built.dupe(), action_outputs)
                    .into(),
            )
        }
    })
}

fn ensure_source_artifact_staged<'a>(
    dice: &'a mut DiceComputations,
    source: &'a SourceArtifact,
) -> impl Future<Output = buck2_error::Result<EnsureArtifactGroupReady>> + use<'a> {
    async move {
        Ok(EnsureArtifactGroupReady::Single(
            path_artifact_value(
                dice,
                Arc::new(source.get_path().to_cell_path()),
                Some(source.get_path().package()),
            )
            .await?,
        ))
    }
    .boxed()
}

// These errors should be unreachable, they indicate misuse of the staged ensure artifact (or other buck
// invariant violations), but it's still better to propagate them as Error than to panic!().
#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
pub enum EnsureArtifactStagedError {
    #[error("Tried to unpack single artifact, but got transitive set")]
    UnpackSingleTransitiveSet,
    #[error("Expected a transitive set, got a single artifact")]
    ExpectedTransitiveSet,
    // This one could probably be a panic! if DICE didn't eagerly re-evaluate all deps.
    #[error("Building an artifact didn't produce it. Expected `{}` but only have `{}`", .0.get_path(), display_outputs(.1))]
    BuildArtifactMissing(BuildArtifact, ActionOutputs),
}

fn display_outputs(outputs: &ActionOutputs) -> String {
    format!(
        "({})",
        outputs
            .iter()
            .map(|(path, _)| path.path())
            .sorted()
            .join(", ")
    )
}

/// Represents the "ready" stage of an ensure_artifact_*() call. At this point the
/// ArtifactValue/ArtifactGroupValues can be synchronously accessed/constructed.
pub(crate) enum EnsureArtifactGroupReady {
    Single(ArtifactValue),
    TransitiveSet(ArtifactGroupValues),
}

impl EnsureArtifactGroupReady {
    /// Converts the ensured artifact to an ArtifactGroupValues. The caller must ensure that the passed in artifact
    /// is the same one that was used to ensure this.
    pub(crate) fn into_group_values<'v>(
        self,
        resolved_artifact_group: ResolvedArtifactGroup<'v>,
    ) -> buck2_error::Result<ArtifactGroupValues> {
        match self {
            EnsureArtifactGroupReady::TransitiveSet(values) => Ok(values),
            EnsureArtifactGroupReady::Single(value) => match resolved_artifact_group {
                ResolvedArtifactGroup::Artifact(artifact) => {
                    Ok(ArtifactGroupValues::from_artifact(artifact.dupe(), value))
                }
                ResolvedArtifactGroup::TransitiveSetProjection(_) => {
                    Err(EnsureArtifactStagedError::ExpectedTransitiveSet.into())
                }
            },
        }
    }

    fn unpack_single(self) -> buck2_error::Result<ArtifactValue> {
        match self {
            EnsureArtifactGroupReady::Single(value) => Ok(value),
            EnsureArtifactGroupReady::TransitiveSet(..) => {
                Err(EnsureArtifactStagedError::UnpackSingleTransitiveSet.into())
            }
        }
    }
}

size_assert::words_of_type!(EnsureArtifactGroupReady, 4);

// Assert we don't unknowingly regress the size of these critical futures. The first two are the
// important ones to track and not regress; the rest are here to help understand how changes impact
// the important ones above, and regressing them is generally okay as long as the above don't.
size_assert::words_of_async_fn_future!(DiceComputations::ensure_artifact_group, (_, _), 2);
size_assert::words_of_async_fn_future!(ensure_artifact_group_staged, (_, _), 9);
size_assert::words_of_async_fn_future!(ensure_artifact_staged, (_, _), 9);
size_assert::words_of_async_fn_future!(ensure_base_artifact_staged, (_, _), 9);
size_assert::words_of_async_fn_future!(ensure_build_artifact_staged, (_, _), 9);
size_assert::words_of_async_fn_future!(ActionCalculation::build_action, (_, _), 8);
size_assert::words_of_async_fn_future!(ensure_source_artifact_staged, (_, _), 2);

async fn dir_artifact_value(
    ctx: &mut DiceComputations<'_>,
    cell_path: Arc<CellPath>,
) -> buck2_error::Result<ArtifactValue> {
    // We kept running into this performance footgun where a large directory is declared as a source
    // on a toolchain, and then every `BuildKey` using that toolchain ends up taking a DICE edge on
    // `PathMetadataKey` of every file inside that directory, blowing up Buck2's memory use.
    // `DirArtifactValueKey` is an intermediate DICE key to prevent that -  every `BuildKey` using
    // that directory now only depends on one `DirArtifactValueKey`, and that `DirArtifactValueKey`
    // depends on the `PathMetadataKey` of every member of the directory.
    #[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
    #[display("dir_artifact_value({})", _0)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct DirArtifactValueKey(Arc<CellPath>);

    #[async_trait]
    impl Key for DirArtifactValueKey {
        type Value = buck2_error::Result<ArtifactValue>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellation: &CancellationContext,
        ) -> Self::Value {
            let artifact_fs = ctx.get_artifact_fs().await?;
            artifact_fs.validate_canonical_cell(self.0.cell())?;
            let files = DiceFileComputations::read_dir(ctx, self.0.as_ref().as_ref())
                .await?
                .included;
            let files = if artifact_fs.cell_source_path_mode() == CellSourcePathMode::CanonicalV1
                && self.0.path().is_empty()
            {
                // Only the byte-exact entry is omitted. Case variants are reserved
                // host-dependently, so dropping them silently would diverge this fingerprint
                // across hosts for byte-identical checkouts; reject them instead.
                let mut included = Vec::with_capacity(files.len());
                for entry in files.iter() {
                    if entry.file_name.as_str() == "buck-out" {
                        continue;
                    }
                    if artifact_fs.is_reserved_buck_out_top_level_name(&entry.file_name) {
                        return Err(buck2_error::buck2_error!(
                            buck2_error::ErrorTag::Input,
                            "Source directory entry `{}` in cell `{}` is a case variant of the reserved top-level `buck-out` name in `canonical_v1`",
                            entry.file_name,
                            self.0.cell(),
                        ));
                    }
                    included.push(entry.clone());
                }
                included
            } else {
                files.to_vec()
            };

            let entry_values = ctx
                .try_compute_join(files, async |ctx, x| {
                    // TODO(scottcao): This current creates a `DirArtifactValueKey` for each subdir of a source directory.
                    // Instead, this should be 1 key for the entire top-level directory since there's almost
                    // no chance of getting cache hit with a sub-directory.
                    let value = path_artifact_value(
                        ctx,
                        Arc::new(self.0.as_ref().join(&x.file_name)),
                        None,
                    )
                    .await?;
                    buck2_error::Ok((x.file_name, value))
                })
                .await?;

            enum DepsMerger {
                None,
                One(ActionSharedDirectory),
                Multiple(ActionDirectoryBuilder),
            }

            let mut entries = SortedVectorMap::new();
            let mut deps_merger = DepsMerger::None;
            for (file_name, value) in entry_values {
                entries.insert(file_name, value.entry().dupe());
                if let Some(deps) = value.deps() {
                    deps_merger = match deps_merger {
                        DepsMerger::None => DepsMerger::One(deps.dupe()),
                        DepsMerger::One(first_deps) => {
                            let mut builder = first_deps.into_builder();
                            builder.merge(deps.dupe().into_builder())?;
                            DepsMerger::Multiple(builder)
                        }
                        DepsMerger::Multiple(mut builder) => {
                            builder.merge(deps.dupe().into_builder())?;
                            DepsMerger::Multiple(builder)
                        }
                    }
                }
            }
            let entries = entries.into_iter().collect();

            let digest_config = ctx.global_data().get_digest_config();
            let d: DirectoryData<_, _, _> =
                DirectoryData::new(entries, digest_config.as_directory_serializer());
            let d = INTERNER.intern(d);

            let deps = match deps_merger {
                DepsMerger::None => None,
                DepsMerger::One(deps) => Some(deps),
                DepsMerger::Multiple(builder) => Some(
                    builder
                        .fingerprint(digest_config.as_directory_serializer())
                        .shared(&*INTERNER),
                ),
            };

            Ok(ArtifactValue::new(ActionDirectoryEntry::Dir(d), deps))
        }

        fn equality(x: &Self::Value, y: &Self::Value) -> bool {
            match (x, y) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    ctx.compute(&DirArtifactValueKey(cell_path)).await?
}

#[async_recursion]
async fn path_artifact_value(
    ctx: &mut DiceComputations<'_>,
    cell_path: Arc<CellPath>,
    label: Option<PackageLabel>,
) -> buck2_error::Result<ArtifactValue> {
    let artifact_fs = ctx.get_artifact_fs().await?;
    artifact_fs.validate_canonical_cell(cell_path.cell())?;
    if artifact_fs.cell_source_path_mode() == CellSourcePathMode::CanonicalV1
        && artifact_fs.is_reserved_buck_out_source_path(cell_path.path())
    {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Source path `{cell_path}` is beneath the reserved top-level `buck-out` subtree in `canonical_v1`",
        ));
    }

    let raw = match DiceFileComputations::read_path_metadata(ctx, cell_path.as_ref().as_ref()).await
    {
        Ok(raw) => Ok(raw),
        Err(e) => {
            if let Some(label) = label {
                if let Ok(listing) = DicePackageListingResolver(ctx)
                    .resolve_package_listing(label.dupe())
                    .await
                {
                    return Err(e.with_package_context_information(
                        BuildFilePath::new(label, listing.buildfile().to_owned())
                            .path()
                            .path()
                            .to_string(),
                    ));
                }
            }

            // Suggestion is best effort, don't want it to override the actual error
            Err(e.without_package_context_information())
        }
    }?;

    match raw {
        RawPathMetadata::Symlink {
            at,
            to: RawSymlink::External(external_symlink),
        } => {
            if artifact_fs.cell_source_path_mode() == CellSourcePathMode::CanonicalV1 {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Source symlink `{at}` has an absolute or external target; canonical_v1 only supports relative source symlinks that remain inside one cell",
                ));
            }
            Ok(ArtifactValue::new(
                ActionDirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(
                    external_symlink,
                )),
                None,
            ))
        }
        RawPathMetadata::File(metadata) => Ok(ArtifactValue::new(
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(metadata)),
            None,
        )),
        RawPathMetadata::Directory => dir_artifact_value(ctx, cell_path).await,
        RawPathMetadata::Symlink {
            at,
            to: RawSymlink::Relative(target, target_rel),
        } => {
            if artifact_fs.cell_source_path_mode() == CellSourcePathMode::CanonicalV1 {
                validate_canonical_source_symlink(
                    &artifact_fs,
                    at.as_ref(),
                    target.as_ref(),
                    target_rel.target(),
                )?;
            }
            // TODO (T126181780): This should have a limit on recursion.
            let target_artifact_value = path_artifact_value(ctx, target.dupe(), label).await?;
            let root_cell = ctx.get_cell_resolver().await?.root_cell();
            let use_correct_source_symlink_reading = ctx
                .parse_legacy_config_property(
                    root_cell,
                    BuckconfigKeyRef {
                        section: "buck2",
                        property: "use_correct_source_symlink_reading",
                    },
                )
                .await?
                .unwrap_or(true);
            // In the case where this is a source artifact like `dir/link/foo`, where the symlink is
            // actually at `link`, `ArtifactValue` doesn't have a representation for the kind of
            // thing that'd require, so we read through the symlink instead. We could enhance
            // `ArtifactValue` to make that possible, but Jakob isn't sure that's a good idea
            let dont_read_through_symlink = use_correct_source_symlink_reading && at == cell_path;
            let canonical = artifact_fs.cell_source_path_mode() == CellSourcePathMode::CanonicalV1;
            if !dont_read_through_symlink && !canonical {
                return Ok(target_artifact_value);
            }

            // Reading through a symlink still has to name the target as a dependency, so that the
            // canonical view publishes it and the Merkle tree contains its bytes.
            let deps = canonical_source_symlink_deps(
                artifact_fs.resolve_cell_path_for_execution((*target).as_ref())?,
                &target_artifact_value,
                ctx.global_data().get_digest_config(),
            )?;
            let entry = if dont_read_through_symlink {
                ActionDirectoryEntry::Leaf(ActionDirectoryMember::Symlink(target_rel.dupe()))
            } else {
                target_artifact_value.entry().dupe()
            };
            Ok(ArtifactValue::new(entry, Some(deps)))
        }
    }
}

fn canonical_source_symlink_deps(
    target_path: buck2_core::fs::project_rel_path::ProjectRelativePathBuf,
    target_value: &ArtifactValue,
    digest_config: DigestConfig,
) -> buck2_error::Result<ActionSharedDirectory> {
    let mut builder = ActionDirectoryBuilder::empty();
    // `insert_artifact` also merges `target_value.deps()`, which is what makes the returned tree
    // transitively closed over read-through chains.
    insert_artifact(&mut builder, target_path, target_value)?;
    Ok(builder
        .fingerprint(digest_config.as_directory_serializer())
        .shared(&*INTERNER))
}

fn validate_canonical_source_symlink(
    artifact_fs: &buck2_core::fs::artifact_path_resolver::ArtifactFs,
    at: &CellPath,
    target: &CellPath,
    raw_target: &RelativePath,
) -> buck2_error::Result<()> {
    if at.cell() != target.cell() {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Relative source symlink `{at}` crosses from canonical cell `{}` into `{}`; canonical_v1 requires relative source symlinks to stay within one cell",
            at.cell(),
            target.cell(),
        ));
    }
    if artifact_fs.is_reserved_buck_out_source_path(target.path()) {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Relative source symlink `{at}` resolves beneath the reserved top-level `buck-out` subtree in `canonical_v1`",
        ));
    }

    // Only the depth matters: `..` must not underflow the cell root, and the component landing at
    // depth 1 must not be the reserved top-level name.
    let mut depth = at.path().parent().map_or(0, |parent| parent.iter().count());
    let mut saw_target_name = false;
    for component in raw_target.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if saw_target_name {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Relative source symlink `{at}` has non-canonical target `{raw_target}`: `..` may not cancel a component introduced by the symlink target",
                    ));
                }
                depth = depth.checked_sub(1).ok_or_else(|| {
                    buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Relative source symlink `{at}` target `{raw_target}` escapes canonical cell `{}`",
                        at.cell(),
                    )
                })?;
            }
            Component::Normal(name) => {
                saw_target_name = true;
                depth += 1;
                if depth == 1 && artifact_fs.is_reserved_buck_out_top_level_name(name) {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Relative source symlink `{at}` target `{raw_target}` traverses the reserved top-level `buck-out` subtree in `canonical_v1`",
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod canonical_source_symlink_tests {
    use buck2_core::fs::artifact_path_resolver::ArtifactFs;
    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;

    use super::*;

    fn artifact_fs() -> buck2_error::Result<ArtifactFs> {
        Ok(ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("sample", "")],
        ))
    }

    fn validate(at: &str, target: &str, raw: &str) -> buck2_error::Result<()> {
        validate_canonical_source_symlink(
            &artifact_fs()?,
            &CellPath::testing_new(at),
            &CellPath::testing_new(target),
            RelativePath::new(raw)?,
        )
    }

    #[test]
    fn permits_only_unambiguous_in_cell_relative_targets() -> buck2_error::Result<()> {
        validate("sample//dir/link", "sample//sibling", "../sibling")?;
        validate("sample//dir/link", "sample//dir/child", "child")?;

        assert!(
            validate(
                "sample//dir/link",
                "sample//dir/sibling",
                "child/../sibling"
            )
            .is_err()
        );
        assert!(validate("sample//link", "sample//outside", "../outside").is_err());
        assert!(validate("sample//link", "sample//buck-out/file", "buck-out/file").is_err());
        assert!(validate("sample//link", "other//file", "file").is_err());
        Ok(())
    }

    #[test]
    fn read_through_symlink_dependencies_are_transitively_closed() -> buck2_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let leaf = ArtifactValue::file(digest_config.empty_file());
        let leaf_path = ProjectRelativePathBuf::unchecked_new(
            "buck-out/v2/cell_sources/v1/c_73616d706c65/leaf".into(),
        );
        let inner_deps = canonical_source_symlink_deps(leaf_path.clone(), &leaf, digest_config)?;
        let inner = ArtifactValue::new(leaf.entry().dupe(), Some(inner_deps));
        let inner_path = ProjectRelativePathBuf::unchecked_new(
            "buck-out/v2/cell_sources/v1/c_73616d706c65/inner".into(),
        );
        let outer_deps = canonical_source_symlink_deps(inner_path.clone(), &inner, digest_config)?;
        let builder = outer_deps.into_builder();

        assert!(extract_artifact_value(&builder, &leaf_path, digest_config)?.is_some());
        assert!(extract_artifact_value(&builder, &inner_path, digest_config)?.is_some());
        Ok(())
    }
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum ProjectedArtifactError {
    #[error("The path `{0}` does not exist in the artifact `{1}`")]
    #[buck2(tag = buck2_error::ErrorTag::ProjectMissingPath)]
    MissingInProjectedArtifact(ForwardRelativePathBuf, BaseArtifactKind),
}

#[derive(
    Clone, Dupe, Eq, PartialEq, Hash, Display, Debug, Allocative, RefCast, Pagable
)]
#[repr(transparent)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct EnsureProjectedArtifactKey(pub(crate) ArtifactKind);

#[async_trait]
impl Key for EnsureProjectedArtifactKey {
    type Value = buck2_error::Result<ArtifactValue>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let ArtifactKind { base, path } = &self.0;

        if path.is_empty() {
            return Err(internal_error!(
                "EnsureProjectedArtifactKey with non-empty projected path"
            ));
        }

        let base_value = ensure_base_artifact_staged(ctx, base)
            .await?
            .unpack_single()?;
        let base_content_based_path_hash = base_value.content_based_path_hash();

        let artifact_fs = ctx.get_artifact_fs().await?;
        let digest_config = ctx.global_data().get_digest_config();

        let base_path = match base {
            BaseArtifactKind::Build(built) => {
                artifact_fs.resolve_build(built.get_path(), Some(&base_content_based_path_hash))?
            }
            BaseArtifactKind::Source(source) => {
                artifact_fs.resolve_source_for_execution(source.get_path())?
            }
        };

        let projected_path = base_path.join(path);

        let mut builder = ActionDirectoryBuilder::empty();
        insert_artifact(&mut builder, base_path, &base_value)?;

        let value = extract_artifact_value(&builder, &projected_path, digest_config)
            .with_buck_error_context(|| {
                format!("The path `{path}` cannot be projected in the artifact `{base}`. Are you calling project() on a symlink?")
            })?
            .ok_or_else(|| {
                ProjectedArtifactError::MissingInProjectedArtifact(path.to_buf(), base.dupe())
            })?;

        // Projected artifacts are located in the same directory as the base artifact, so we
        // need to store the same content based path hash in order to find them in the correct place.
        let value = value.with_content_based_path_hash(base_content_based_path_hash);

        Ok(value)
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

/// Activation data for [`EnsureTransitiveSetProjectionKey`] DICE evaluations,
/// used to record duration in the critical path graph.
pub struct EnsureTransitiveSetProjectionKeyActivationData {
    pub time_span: TimeSpan,
}

#[derive(
    Clone, Dupe, Eq, PartialEq, Hash, Display, Debug, Allocative, RefCast, Pagable
)]
#[repr(transparent)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct EnsureTransitiveSetProjectionKey(pub TransitiveSetProjectionKey);

#[async_trait]
impl Key for EnsureTransitiveSetProjectionKey {
    type Value = buck2_error::Result<ArtifactGroupValues>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let set = self.0.key.lookup(ctx).await?;

        let projection_sub_inputs = set.get_projection_sub_inputs(self.0.projection)?;

        let sub_inputs: Vec<_> =
            KeepGoing::try_compute_join_all(ctx, projection_sub_inputs.iter(), async |ctx, a| {
                a.resolved_artifact(ctx).await
            })
            .await?;

        let (values, children) = {
            // Compute the new inputs. Note that ordering here (and below) is important to ensure
            // stability of the ArtifactGroupValues we produce across executions, which try_compute_join_all preserves.

            // FIXME(JakobDegen): The amount of stuff we're holding over this await point is extremely clowny
            let ready_inputs: Vec<_> =
                KeepGoing::try_compute_join_all(ctx, sub_inputs.iter(), async |ctx, v| {
                    ensure_artifact_group_staged(ctx, *v).await
                })
                .await?;

            // Partition our inputs in artifacts and projections.
            let mut values_count = 0;
            for input in sub_inputs.iter() {
                if let ResolvedArtifactGroup::Artifact(..) = input {
                    values_count += 1;
                }
            }

            let mut values = SmallVec::<[_; 1]>::with_capacity(values_count);
            let mut children = Vec::with_capacity(sub_inputs.len() - values_count);

            for (group, ready) in zip(sub_inputs.iter().copied(), ready_inputs) {
                match group {
                    ResolvedArtifactGroup::Artifact(artifact) => {
                        values.push((artifact.dupe(), ready.unpack_single()?))
                    }
                    ResolvedArtifactGroup::TransitiveSetProjection(..) => {
                        children.push(ready.into_group_values(group)?)
                    }
                }
            }
            (values, children)
        };

        let artifact_fs = ctx.get_artifact_fs().await?;

        // At this point we're holding a lot of data and want to ensure that we don't hold that across any
        // .await, so move into a little sync closure and call that
        (move || {
            let time_span = TimeSpan::start_now();
            let digest_config = ctx.global_data().get_digest_config();

            let values = ArtifactGroupValues::new(values, children, &artifact_fs, digest_config)
                .buck_error_context("Failed to construct ArtifactGroupValues")?;

            ctx.store_evaluation_data(EnsureTransitiveSetProjectionKeyActivationData {
                time_span: time_span.end_now(),
            })?;
            Ok(values)
        })()
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x.shallow_equals(y),
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}
