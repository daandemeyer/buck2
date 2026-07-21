/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_common::file_ops::metadata::FileMetadata;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::directory_iterator::DirectoryIterator;
use buck2_directory::directory::directory_iterator::DirectoryIteratorPathStack;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorContext;
use dupe::Dupe;

use crate::digest_config::DigestConfig;
use crate::directory::ActionDirectoryBuilder;
use crate::directory::ActionDirectoryEntry;
use crate::directory::ActionDirectoryMember;
use crate::directory::ActionSharedDirectory;
use crate::directory::LazyActionDirectoryBuilder;
use crate::execute::request::CommandExecutionInput;

fn raw_canonical_input_error(path: &ProjectRelativePath) -> String {
    format!("Raw incremental input `{path}` occupies the reserved canonical cell-sources namespace")
}

fn reject_raw_canonical_input(
    fs: &ArtifactFs,
    base: &ProjectRelativePath,
    entry: &ActionDirectoryEntry<ActionSharedDirectory>,
) -> buck2_error::Result<()> {
    if fs.cell_source_path_mode() == CellSourcePathMode::Physical {
        return Ok(());
    }
    let reject = |path: &ProjectRelativePath| -> buck2_error::Result<()> {
        // The leaf decoder is used for directories too: unlike the directory decoder it errors on
        // the traversable intermediate names, which a raw input must not occupy even as an empty
        // directory.
        let decoded = fs
            .decode_source_execution_leaf_path(path)
            .with_buck_error_context(|| raw_canonical_input_error(path))?;
        if decoded.is_some() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "{}",
                raw_canonical_input_error(path),
            ));
        }
        Ok(())
    };

    reject(base)?;
    if let DirectoryEntry::Dir(directory) = entry {
        let mut walk = directory.unordered_walk();
        while let Some((path, _nested)) = walk.next() {
            let path = base.join(path.get());
            reject(&path)?;
        }
    }
    Ok(())
}

fn validate_finalized_input_tree(
    fs: &ArtifactFs,
    directory: &ActionDirectoryBuilder,
) -> buck2_error::Result<()> {
    if fs.cell_source_path_mode() == CellSourcePathMode::Physical {
        return Ok(());
    }
    let mut walk = directory.unordered_walk();
    while let Some((path, entry)) = walk.next() {
        let path = path.get();
        let path = ProjectRelativePath::new(path.as_str())?;
        if matches!(entry, DirectoryEntry::Dir(_)) {
            fs.decode_source_execution_path(path)?;
        } else {
            fs.decode_source_execution_leaf_path(path)?;
        }
    }
    Ok(())
}

pub fn inputs_directory(
    inputs: &[CommandExecutionInput],
    digest_config: DigestConfig,
    fs: &ArtifactFs,
) -> buck2_error::Result<ActionDirectoryBuilder> {
    let mut builder = LazyActionDirectoryBuilder::empty();
    for input in inputs {
        match input {
            CommandExecutionInput::Artifact(group) => {
                group.add_to_directory(&mut builder, fs)?;
            }
            CommandExecutionInput::ActionMetadata(metadata) => {
                let path = fs
                    .buck_out_path_resolver()
                    .resolve_gen(&metadata.path, Some(&metadata.content_hash))?;
                builder.insert(
                    path.into(),
                    DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                        digest: metadata.digest.dupe(),
                        is_executable: false,
                    })),
                )?;
            }
            CommandExecutionInput::ScratchPath(path) => {
                let path = fs.buck_out_path_resolver().resolve_scratch(path)?;
                builder.insert(
                    path.into(),
                    DirectoryEntry::Dir(digest_config.empty_directory()),
                )?;
            }
            CommandExecutionInput::IncrementalRemoteOutput(path, entry) => {
                reject_raw_canonical_input(fs, path, entry)?;
                match entry {
                    DirectoryEntry::Dir(d) => {
                        builder.insert(path.clone().into(), DirectoryEntry::Dir(d.dupe()))?;
                    }
                    DirectoryEntry::Leaf(m) => {
                        builder.insert(path.clone().into(), DirectoryEntry::Leaf(m.dupe()))?;
                    }
                }
            }
        };
    }
    let directory = builder.finalize()?;
    validate_finalized_input_tree(fs, &directory)?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;

    use super::*;
    use crate::directory::INTERNER;

    fn test_artifact_fs(mode: CellSourcePathMode) -> buck2_error::Result<ArtifactFs> {
        Ok(ArtifactFs::testing_new_with_mode(
            mode,
            &[("root", ""), ("sample", "sample")],
        ))
    }

    fn file_leaf(digest_config: DigestConfig) -> ActionDirectoryMember {
        ActionDirectoryMember::File(digest_config.empty_file())
    }

    fn shared_dir_with_leaf(
        relative_path: &str,
        digest_config: DigestConfig,
    ) -> buck2_error::Result<ActionSharedDirectory> {
        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            ProjectRelativePath::unchecked_new(relative_path),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        Ok(builder
            .fingerprint(digest_config.as_directory_serializer())
            .shared(&*INTERNER))
    }

    const CANONICAL_LEAF: &str = "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/raw.txt";
    const CANONICAL_DIR: &str = "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg";

    fn raw_leaf_input(digest_config: DigestConfig) -> CommandExecutionInput {
        CommandExecutionInput::IncrementalRemoteOutput(
            ProjectRelativePathBuf::unchecked_new(CANONICAL_LEAF.to_owned()),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )
    }

    fn raw_dir_input(digest_config: DigestConfig) -> buck2_error::Result<CommandExecutionInput> {
        Ok(CommandExecutionInput::IncrementalRemoteOutput(
            ProjectRelativePathBuf::unchecked_new(CANONICAL_DIR.to_owned()),
            DirectoryEntry::Dir(shared_dir_with_leaf("raw.txt", digest_config)?),
        ))
    }

    /// Rooted outside the reserved namespace, but its nested entries reach into it.
    fn raw_nested_dir_input(
        digest_config: DigestConfig,
    ) -> buck2_error::Result<CommandExecutionInput> {
        Ok(CommandExecutionInput::IncrementalRemoteOutput(
            ProjectRelativePathBuf::unchecked_new("buck-out/v2".to_owned()),
            DirectoryEntry::Dir(shared_dir_with_leaf(
                "cell_sources/v1/c_73616d706c65/pkg/raw.txt",
                digest_config,
            )?),
        ))
    }

    fn expect_reserved_namespace_error(
        input: CommandExecutionInput,
        fs: &ArtifactFs,
        digest_config: DigestConfig,
    ) {
        match inputs_directory(&[input], digest_config, fs) {
            Ok(_) => panic!("raw incremental input must not claim a canonical source identity"),
            Err(error) => assert!(
                error.to_string().contains("reserved canonical"),
                "unexpected error: {error:#}"
            ),
        }
    }

    /// These decode to `None` with the directory decoder, so squatting them can only be caught by
    /// the leaf-decoder check.
    fn raw_intermediate_name_inputs(digest_config: DigestConfig) -> Vec<CommandExecutionInput> {
        ["buck-out/v2/cell_sources", "buck-out/v2/cell_sources/v1"]
            .into_iter()
            .map(|path| {
                CommandExecutionInput::IncrementalRemoteOutput(
                    ProjectRelativePathBuf::unchecked_new(path.to_owned()),
                    DirectoryEntry::Dir(digest_config.empty_directory()),
                )
            })
            .collect()
    }

    fn raw_reserved_inputs(
        digest_config: DigestConfig,
    ) -> buck2_error::Result<Vec<CommandExecutionInput>> {
        let mut inputs = vec![
            raw_leaf_input(digest_config),
            raw_dir_input(digest_config)?,
            raw_nested_dir_input(digest_config)?,
        ];
        inputs.extend(raw_intermediate_name_inputs(digest_config));
        Ok(inputs)
    }

    #[test]
    fn canonical_rejects_raw_incremental_inputs_in_reserved_namespace() -> buck2_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let fs = test_artifact_fs(CellSourcePathMode::CanonicalV1)?;
        for input in raw_reserved_inputs(digest_config)? {
            expect_reserved_namespace_error(input, &fs, digest_config);
        }
        Ok(())
    }

    #[test]
    fn canonical_rejects_finalized_tree_with_reserved_namespace_entry() -> buck2_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let fs = test_artifact_fs(CellSourcePathMode::CanonicalV1)?;

        // A leaf on the reserved version directory, which the dir decoder would merely skip.
        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            ProjectRelativePath::unchecked_new("buck-out/v2/cell_sources/v1"),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        match validate_finalized_input_tree(&fs, &builder) {
            Ok(()) => panic!("leaf occupying the reserved namespace must be rejected"),
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("occupies the reserved cell-sources namespace"),
                "unexpected error: {error:#}"
            ),
        }

        // Malformed below the version directory, rather than silently generated output.
        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            ProjectRelativePath::unchecked_new("buck-out/v2/cell_sources/v1/nonsense/x.txt"),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        match validate_finalized_input_tree(&fs, &builder) {
            Ok(()) => panic!("malformed entry in the reserved namespace must be rejected"),
            Err(error) => assert!(
                error.to_string().contains("expected a `c_` cell component"),
                "unexpected error: {error:#}"
            ),
        }
        Ok(())
    }

    #[test]
    fn physical_mode_accepts_reserved_namespace_trees() -> buck2_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let fs = test_artifact_fs(CellSourcePathMode::Physical)?;

        for input in raw_reserved_inputs(digest_config)? {
            inputs_directory(&[input], digest_config, &fs)?;
        }

        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            ProjectRelativePath::unchecked_new("buck-out/v2/cell_sources/v1"),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        validate_finalized_input_tree(&fs, &builder)?;

        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            ProjectRelativePath::unchecked_new("buck-out/v2/cell_sources/v1/nonsense/x.txt"),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        validate_finalized_input_tree(&fs, &builder)?;
        Ok(())
    }

    #[test]
    fn canonical_accepts_ordinary_trees() -> buck2_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let fs = test_artifact_fs(CellSourcePathMode::CanonicalV1)?;

        let directory = inputs_directory(
            &[
                CommandExecutionInput::IncrementalRemoteOutput(
                    ProjectRelativePathBuf::unchecked_new(
                        "buck-out/v2/gen/root/1234/pkg/__t__/out.txt".to_owned(),
                    ),
                    DirectoryEntry::Leaf(file_leaf(digest_config)),
                ),
                CommandExecutionInput::IncrementalRemoteOutput(
                    ProjectRelativePathBuf::unchecked_new(
                        "buck-out/v2/gen/root/1234/pkg/__t__/incremental_state".to_owned(),
                    ),
                    DirectoryEntry::Dir(shared_dir_with_leaf("state.json", digest_config)?),
                ),
            ],
            digest_config,
            &fs,
        )?;
        validate_finalized_input_tree(&fs, &directory)?;

        // Generated outputs mixed with a canonical source entry: the shape typed source
        // artifacts produce.
        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            ProjectRelativePath::unchecked_new("buck-out/v2/gen/root/1234/pkg/__t__/out.txt"),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        builder.insert(
            ProjectRelativePath::unchecked_new(CANONICAL_LEAF),
            DirectoryEntry::Leaf(file_leaf(digest_config)),
        )?;
        validate_finalized_input_tree(&fs, &builder)?;
        Ok(())
    }
}
