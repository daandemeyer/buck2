/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use async_trait::async_trait;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::dice::data::HasIoProvider;
use buck2_core::cells::execution_name::CellExecutionNames;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
use dice::DiceComputations;
use dupe::Dupe;

use crate::context::HasBuildContextData;

#[async_trait]
pub trait GetArtifactFs {
    /// Get the configured ArtifactFs.
    async fn get_artifact_fs(&mut self) -> buck2_error::Result<ArtifactFs>;
}

#[async_trait]
impl GetArtifactFs for DiceComputations<'_> {
    async fn get_artifact_fs(&mut self) -> buck2_error::Result<ArtifactFs> {
        let buck_out_path_resolver = self.get_buck_out_path().await?;
        let project_filesystem = self.global_data().get_io_provider().project_root().dupe();
        let buck_path_resolver = self.get_cell_resolver().await?;
        let cell_source_path_mode = self.get_cell_source_path_mode().await?;
        // Execution names only name canonical paths, so a workspace's stale or invalid
        // `[cell_execution_names]` must not fail a `physical` build.
        let cell_execution_names = match cell_source_path_mode {
            CellSourcePathMode::Physical => Arc::new(CellExecutionNames::identity()),
            CellSourcePathMode::CanonicalV1 => self.get_cell_execution_names().await?,
        };
        let artifact_fs = ArtifactFs::new_with_execution_names(
            buck_path_resolver,
            buck_out_path_resolver,
            project_filesystem,
            cell_source_path_mode,
            cell_execution_names,
        );
        // Fail a misconfigured canonical workspace before any source value, Action or upload can
        // be constructed from this ArtifactFs.
        artifact_fs.validate_canonical_layout()?;
        Ok(artifact_fs)
    }
}
