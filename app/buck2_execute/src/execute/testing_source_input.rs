/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! A minimal source artifact for tests which need a `CommandExecutionInput` that resolves through
//! the canonical cell namespace, without depending on `buck2_build_api`'s `Artifact`.

use buck2_core::cells::cell_path::CellPath;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;

use crate::artifact::artifact_dyn::ArtifactDyn;
use crate::artifact::group::artifact_group_values_dyn::ArtifactGroupValuesDyn;
use crate::artifact_value::ArtifactValue;
use crate::directory::LazyActionDirectoryBuilder;
use crate::directory::insert_artifact_lazy;
use crate::execute::request::CommandExecutionInput;

pub struct TestSourceArtifact(pub CellPath);

impl ArtifactDyn for TestSourceArtifact {
    fn resolve_path(
        &self,
        fs: &ArtifactFs,
        _content_hash: Option<&ContentBasedPathHash>,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        fs.resolve_cell_path(self.0.as_ref())
    }

    fn resolve_path_for_execution(
        &self,
        fs: &ArtifactFs,
        _content_hash: Option<&ContentBasedPathHash>,
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

pub struct TestSourceArtifactGroupValues {
    pub artifact: TestSourceArtifact,
    pub value: ArtifactValue,
}

impl ArtifactGroupValuesDyn for TestSourceArtifactGroupValues {
    fn iter(&self) -> Box<dyn Iterator<Item = (&dyn ArtifactDyn, &ArtifactValue)> + '_> {
        Box::new(std::iter::once((
            &self.artifact as &dyn ArtifactDyn,
            &self.value,
        )))
    }

    fn add_to_directory(
        &self,
        builder: &mut LazyActionDirectoryBuilder,
        artifact_fs: &ArtifactFs,
    ) -> buck2_error::Result<()> {
        insert_artifact_lazy(
            builder,
            self.artifact
                .resolve_path_for_execution(artifact_fs, None)?,
            &self.value,
        )
    }
}

pub fn testing_source_input(source: CellPath, value: ArtifactValue) -> CommandExecutionInput {
    CommandExecutionInput::Artifact(Box::new(TestSourceArtifactGroupValues {
        artifact: TestSourceArtifact(source),
        value,
    }))
}
