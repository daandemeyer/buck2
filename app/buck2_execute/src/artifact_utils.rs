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

use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorOptionContext;
use buck2_fs::paths::RelativePathBuf;
use dupe::Dupe;

use crate::artifact_value::ArtifactValue;
use crate::digest_config::DigestConfig;
use crate::directory::ActionDirectoryBuilder;
use crate::directory::ActionDirectoryEntry;
use crate::directory::ActionDirectoryMember;
use crate::directory::ActionSharedDirectory;
use crate::directory::INTERNER;
use crate::directory::extract_artifact_value;
use crate::directory::insert_artifact;
use crate::directory::insert_entry;
use crate::directory::new_symlink;
use crate::directory::override_executable_bit;
use crate::directory::relativize_directory;

pub struct ArtifactValueBuilder<'a> {
    /// Only used to relativize paths; no disk operations performed!
    project_fs: &'a ProjectRoot,
    builder: ActionDirectoryBuilder,
    digest_config: DigestConfig,
}

impl<'a> ArtifactValueBuilder<'a> {
    pub fn new(project_fs: &'a ProjectRoot, digest_config: DigestConfig) -> Self {
        Self {
            project_fs,
            builder: ActionDirectoryBuilder::empty_non_exhaustive(),
            digest_config,
        }
    }

    pub fn add_entry(
        &mut self,
        path: ProjectRelativePathBuf,
        entry: ActionDirectoryEntry<ActionDirectoryBuilder>,
    ) -> buck2_error::Result<()> {
        insert_entry(&mut self.builder, path, entry)
    }

    /// Inserts an input to the tree, which will be required when following
    /// symlinks to calculate the `deps` of the `ArtifactValue`.
    pub fn add_input_value(
        &mut self,
        path: ProjectRelativePathBuf,
        value: &ArtifactValue,
    ) -> buck2_error::Result<()> {
        insert_artifact(&mut self.builder, path, value)
    }

    /// Takes an input `src_value`, adds it to the builder at `src`. Then
    /// creates a symlink to `src`, adds it to the builder at `dest` and
    /// returns it.
    pub fn add_symlinked(
        &mut self,
        src_value: &ArtifactValue,
        src: ProjectRelativePathBuf,
        dest: &ProjectRelativePath,
    ) -> buck2_error::Result<()> {
        let symlink = new_symlink(self.project_fs.relative_path(&src, dest))?;
        insert_artifact(&mut self.builder, src, src_value)?;
        let entry = DirectoryEntry::Leaf(symlink);
        self.builder.insert(dest, entry)?;
        Ok(())
    }

    /// Takes an input `src_value`, adds it to the builder at `src`. Then
    /// creates a copy of `src_value`'s entry relativized as if it had been
    /// copied from `src` to `dest`, adds it to the builder at `dest` and
    /// returns it.
    pub fn add_copied(
        &mut self,
        src_value: &ArtifactValue,
        src: &ProjectRelativePath,
        dest: &ProjectRelativePath,
        executable_bit_override: Option<bool>,
        relative_symlinks: bool,
    ) -> buck2_error::Result<ActionDirectoryEntry<ActionSharedDirectory>> {
        insert_artifact(&mut self.builder, src.to_buf(), src_value)?;

        let entry = match src_value.entry() {
            DirectoryEntry::Dir(directory) => {
                let mut builder = directory.dupe().into_builder();
                relativize_directory(&mut builder, src, dest, relative_symlinks)?;
                if let Some(executable_bit_override) = executable_bit_override {
                    override_executable_bit(&mut builder, executable_bit_override)?;
                }
                DirectoryEntry::Dir(
                    builder.fingerprint(self.digest_config.as_directory_serializer()),
                )
            }
            DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(s)) => {
                // TODO: This seems like it normally shouldn't need to be normalizing anything.
                let src_parent = src.parent().internal_error("Symlink has no dir parent")?;
                let orig_dest = src_parent.join_normalized(s.target())?;
                if relative_symlinks && orig_dest.starts_with(src) {
                    DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(s.dupe()))
                } else {
                    let reldest = self.project_fs.relative_path(src_parent, dest);
                    // RelativePathBuf::from_system_path converts platform-specific path separators.
                    let reldest = RelativePathBuf::from_system_path(&reldest)?;
                    let s = s.relativized(reldest);
                    DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(s)))
                }
            }
            DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(s)) => {
                DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(
                    s.with_full_target()?,
                ))
            }
            DirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => {
                let file_metadata = if let Some(executable_bit_override) = executable_bit_override {
                    f.dupe().with_executable(executable_bit_override)
                } else {
                    f.dupe()
                };
                DirectoryEntry::Leaf(ActionDirectoryMember::File(file_metadata))
            }
        };

        let entry = entry.map_dir(|d| d.shared(&*INTERNER));

        self.builder
            .insert(dest, entry.dupe().map_dir(|d| d.into_builder()))?;

        Ok(entry)
    }

    /// Builds the `ArtifactValue`. Since `self.builder` is rooted at the
    /// project root, `output` must be passed to specify the path of the value
    /// being built.
    pub fn build(&self, output: &ProjectRelativePath) -> buck2_error::Result<ArtifactValue> {
        match extract_artifact_value(&self.builder, output, self.digest_config)? {
            Some(v) => Ok(v),
            None => {
                tracing::debug!("Extracting {} produces empty directory!", output);
                Ok(ArtifactValue::dir(self.digest_config.empty_directory()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use buck2_common::file_ops::metadata::FileMetadata;
    use buck2_common::file_ops::metadata::Symlink;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_directory::directory::directory::Directory;
    use buck2_directory::directory::find::find;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePath;

    use super::*;
    use crate::directory::insert_file;
    use crate::directory::insert_symlink;

    fn path(s: &str) -> &ProjectRelativePath {
        ProjectRelativePath::new(s).unwrap()
    }

    fn get_symlink(s: &str) -> Arc<Symlink> {
        Arc::new(Symlink::new(s.into()))
    }

    fn get_symlink_artifact_value(s: &str) -> ArtifactValue {
        let symlink = DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(get_symlink(s)));
        ArtifactValue::new(symlink, None)
    }

    fn get_symlink_directory_artifact_value() -> buck2_error::Result<ArtifactValue> {
        let digest_config = DigestConfig::testing_default();
        let mut builder = ActionDirectoryBuilder::empty_non_exhaustive();
        insert_file(
            &mut builder,
            path("dir/real").to_buf(),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        insert_symlink(&mut builder, path("dir/link").to_buf(), get_symlink("real"))?;
        insert_symlink(&mut builder, path("sub/up").to_buf(), get_symlink(".."))?;
        insert_symlink(
            &mut builder,
            path("escape").to_buf(),
            get_symlink("../outside/target"),
        )?;
        builder.mark_uniformly_exhaustive();

        Ok(ArtifactValue::dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        ))
    }

    fn copied_symlink_directory(
        relative_symlinks: bool,
    ) -> buck2_error::Result<ActionDirectoryEntry<ActionSharedDirectory>> {
        let fs = ProjectRootTemp::new().unwrap();
        let mut builder = ArtifactValueBuilder::new(fs.path(), DigestConfig::testing_default());
        builder.add_copied(
            &get_symlink_directory_artifact_value()?,
            path("source"),
            path("buck-out/copied"),
            None,
            relative_symlinks,
        )
    }

    fn symlink_target(
        entry: &ActionDirectoryEntry<ActionSharedDirectory>,
        path: &str,
    ) -> buck2_error::Result<String> {
        let directory = match entry {
            DirectoryEntry::Dir(directory) => directory,
            _ => panic!("Directory type is expected!"),
        };
        let path = ForwardRelativePath::new(path)?;
        let symlink = match find(directory.as_ref(), path)? {
            Some(DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink))) => symlink,
            _ => panic!("Symlink type is expected at `{path}`!"),
        };
        Ok(symlink.target().as_str().to_owned())
    }

    #[test]
    fn copy_directory_relativizes_all_symlinks_by_default() -> buck2_error::Result<()> {
        let entry = copied_symlink_directory(false)?;

        assert_eq!(
            symlink_target(&entry, "dir/link")?,
            "../../../source/dir/real"
        );
        assert_eq!(symlink_target(&entry, "sub/up")?, "../../../source");
        assert_eq!(symlink_target(&entry, "escape")?, "../../outside/target");

        Ok(())
    }

    #[test]
    fn copy_directory_preserves_internal_relative_symlinks() -> buck2_error::Result<()> {
        let entry = copied_symlink_directory(true)?;

        assert_eq!(symlink_target(&entry, "dir/link")?, "real");
        assert_eq!(symlink_target(&entry, "sub/up")?, "..");
        assert_eq!(symlink_target(&entry, "escape")?, "../../outside/target");

        Ok(())
    }

    #[test]
    fn copy_relativized_symlink() -> buck2_error::Result<()> {
        // /
        // |-d1/
        // | |-d2/
        // | | |-d3/
        // | | |  |-d4/
        // | | |  | |-link -> ../../../d6/target
        // | |-d5/
        // | | |-new_link
        // |-d6/
        // | |-target

        for relative_symlinks in [false, true] {
            let fs = ProjectRootTemp::new().unwrap();
            let mut builder = ArtifactValueBuilder::new(fs.path(), DigestConfig::testing_default());
            let entry = builder.add_copied(
                &get_symlink_artifact_value("../../../d6/target"),
                path("d1/d2/d3/d4/link"),
                path("d1/d5/new_link"),
                None,
                relative_symlinks,
            )?;

            let new_symlink = match entry.as_ref() {
                DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(s)) => s,
                _ => panic!("Symlink type is expected!"),
            };

            assert_eq!(
                new_symlink,
                &get_symlink("../d6/target"),
                "Symlinks are different"
            );
        }

        Ok(())
    }
}
