/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_artifact::artifact::output_type::OutputType;
use buck2_build_api::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use buck2_build_api::interpreter::rule_defs::artifact::output_artifact_like::OutputArtifactArg;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use buck2_build_api::interpreter::rule_defs::context::AnalysisActions;
use buck2_hash::buck_indexset;
use starlark::environment::MethodsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::ValueTyped;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;

use crate::actions::impls::extract_archive::UnregisteredExtractArchiveAction;

#[starlark_module]
pub(crate) fn analysis_actions_methods_extract_archive(methods: &mut MethodsBuilder) {
    /// Extracts the `archive` artifact into the directory `output` (a string or an output
    /// `artifact`) and returns the output `artifact`.
    ///
    /// Buck2 extracts the archive itself, without running a command, so the action needs no tools
    /// on the host and always runs where Buck2 runs.
    ///
    /// * `type`: one of `tar`, `tar.gz`, `tar.bz2`, `tar.xz`, `tar.zst` or `zip`. When it is not
    ///   given, the type comes from the extension of the archive's file name.
    /// * `strip_prefix`: only the entries below this path in the archive are extracted, to the
    ///   root of the output directory.
    /// * `excludes`: regular expressions. An entry is skipped when one of the regular expressions
    ///   matches its name in the archive, before `strip_prefix` is removed. Zip entry names use
    ///   `/` as the separator here, even when the archive stores `\`. Skipping a directory entry
    ///   does not skip the entries below it.
    ///
    /// Only file contents, symlinks and the executable bit are kept. A hard link to a file becomes
    /// a separate file in the output artifact. Entry names with a `..` component fail the action,
    /// and so does an entry that would be written through a symlink from the same archive. Tar
    /// entries for FIFOs and devices fail the action too, and so do sparse files that GNU tar
    /// stored in the pax format.
    fn extract_archive<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] output: OutputArtifactArg<'v>,
        #[starlark(require = pos)] archive: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named, default = NoneOr::None)] r#type: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] strip_prefix: NoneOr<&str>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        excludes: UnpackListOrTuple<&str>,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        let action = UnregisteredExtractArchiveAction::new(
            archive.0.get_artifact_group()?,
            r#type.into_option(),
            strip_prefix.into_option(),
            excludes.items,
        )?;

        let mut this = this.state()?;
        let (declaration, output_artifact) = this.get_or_declare_output(
            eval,
            output,
            OutputType::Directory,
            has_content_based_path.into_option(),
        )?;
        this.register_action(buck_indexset![output_artifact], action, None, None)?;

        Ok(declaration.into_declared_artifact(AssociatedArtifacts::new()))
    }
}
