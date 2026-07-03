/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path::CellPathRef;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_fs::paths::RelativePathBuf;
use buck2_fs::paths::file_name::FileNameBuf;
use cmp_any::PartialEqAny;
use dice::DiceComputations;
use dice::testing::DiceBuilder;
use dupe::Dupe;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::cas_digest::CasDigestConfig;
use crate::external_symlink::ExternalSymlink;
use crate::file_ops::delegate::FileOpsDelegate;
use crate::file_ops::delegate::FileOpsDelegateWithIgnores;
use crate::file_ops::delegate::testing::FileOpsKey;
use crate::file_ops::delegate::testing::FileOpsValue;
use crate::file_ops::dice::CheckIgnores;
use crate::file_ops::metadata::FileMetadata;
use crate::file_ops::metadata::FileType;
use crate::file_ops::metadata::RawDirEntry;
use crate::file_ops::metadata::RawPathMetadata;
use crate::file_ops::metadata::RawSymlink;
use crate::file_ops::metadata::ReadDirOutput;
use crate::file_ops::metadata::SimpleDirEntry;
use crate::file_ops::metadata::Symlink;
use crate::file_ops::metadata::TrackedFileDigest;
use crate::file_ops::trait_::FileOps;
use crate::ignores::file_ignores::FileIgnoreResult;

#[derive(Pagable)]
enum TestFileOpsEntry {
    File(String /*data*/, FileMetadata),
    ExternalSymlink(Arc<ExternalSymlink>),
    /// The resolved target and the target as written in the link.
    Symlink(Arc<CellPath>, Arc<Symlink>),
    Directory(BTreeSet<SimpleDirEntry>),
}

#[derive(Allocative, Clone, Dupe, Pagable)]
pub struct TestFileOps {
    #[allocative(skip)]
    entries: Arc<BTreeMap<CellPath, TestFileOpsEntry>>,
}

impl TestFileOps {
    fn new(inputs: BTreeMap<CellPath, TestFileOpsEntry>) -> Self {
        let mut entries = BTreeMap::new();
        for (path, entry) in inputs {
            let mut file_type = match entry {
                TestFileOpsEntry::Directory(..) => FileType::Directory,
                TestFileOpsEntry::ExternalSymlink(..) | TestFileOpsEntry::Symlink(..) => {
                    FileType::Symlink
                }
                TestFileOpsEntry::File(..) => FileType::File,
            };
            // make sure the test setup is correct and concise
            assert!(
                entries.insert(path.to_owned(), entry).is_none(),
                "Adding `{path}`, it already exists."
            );

            let mut path = path.as_ref();

            // now add to / create the parent directories
            while let (Some(dir), Some(name)) = (path.parent(), path.path().file_name()) {
                let dir_entry = entries
                    .entry(dir.to_owned())
                    .or_insert_with(|| TestFileOpsEntry::Directory(BTreeSet::new()));
                match dir_entry {
                    TestFileOpsEntry::Directory(listing) => {
                        listing.insert(SimpleDirEntry {
                            file_type,
                            file_name: name.to_owned(),
                        });
                        file_type = FileType::Directory;
                        path = dir;
                    }
                    _ => panic!("Adding `{path}`, but `{dir}` exists and is not a dir"),
                };
            }
        }
        TestFileOps {
            entries: Arc::new(entries),
        }
    }

    pub fn new_with_files(files: BTreeMap<CellPath, String>) -> Self {
        Self::new_with_files_and_symlinks(files, BTreeMap::new())
    }

    /// Symlink targets are relative to the directory containing the link, as on disk.
    pub fn new_with_files_and_symlinks(
        files: BTreeMap<CellPath, String>,
        symlinks: BTreeMap<CellPath, RelativePathBuf>,
    ) -> Self {
        let cas_digest_config = CasDigestConfig::testing_default();

        let mut entries = files
            .into_iter()
            .map(|(path, data)| {
                let metadata = FileMetadata {
                    digest: TrackedFileDigest::from_content(data.as_bytes(), cas_digest_config),
                    is_executable: false,
                };
                (path, TestFileOpsEntry::File(data, metadata))
            })
            .collect::<BTreeMap<CellPath, TestFileOpsEntry>>();
        for (path, target) in symlinks {
            let dir = path.parent().expect("symlink at a cell root");
            let resolved = CellPath::new(
                dir.cell(),
                dir.path()
                    .as_forward_relative_path()
                    .join_normalized(&target)
                    .unwrap()
                    .into(),
            );
            entries.insert(
                path,
                TestFileOpsEntry::Symlink(Arc::new(resolved), Arc::new(Symlink::new(target))),
            );
        }
        Self::new(entries)
    }

    pub fn new_with_files_metadata(files: BTreeMap<CellPath, FileMetadata>) -> Self {
        Self::new(
            files
                .into_iter()
                .map(|(path, m)| (path, TestFileOpsEntry::File("".to_owned(), m)))
                .collect::<BTreeMap<CellPath, TestFileOpsEntry>>(),
        )
    }

    pub fn new_with_symlinks(symlinks: BTreeMap<CellPath, Arc<ExternalSymlink>>) -> Self {
        Self::new(
            symlinks
                .into_iter()
                .map(|(path, s)| (path, TestFileOpsEntry::ExternalSymlink(s)))
                .collect::<BTreeMap<CellPath, TestFileOpsEntry>>(),
        )
    }

    pub fn mock_in_cell(&self, cell: CellName, builder: DiceBuilder) -> DiceBuilder {
        let data = Ok(FileOpsValue(FileOpsDelegateWithIgnores::new(
            None,
            Arc::new(TestCellFileOps(
                cell,
                Self {
                    entries: Arc::clone(&self.entries),
                },
            )),
        )));
        builder
            .mock_and_return(
                FileOpsKey {
                    cell,
                    check_ignores: CheckIgnores::Yes,
                },
                data.dupe(),
            )
            .mock_and_return(
                FileOpsKey {
                    cell,
                    check_ignores: CheckIgnores::No,
                },
                data,
            )
    }
}

#[async_trait]
impl FileOps for TestFileOps {
    async fn read_file_if_exists(
        &self,
        path: CellPathRef<'async_trait>,
    ) -> buck2_error::Result<Option<String>> {
        Ok(self.entries.get(&path.to_owned()).and_then(|e| match e {
            TestFileOpsEntry::File(data, ..) => Some(data.clone()),
            _ => None,
        }))
    }

    async fn read_dir(
        &self,
        path: CellPathRef<'async_trait>,
    ) -> buck2_error::Result<ReadDirOutput> {
        let included = self
            .entries
            .get(&path.to_owned())
            .and_then(|e| match e {
                TestFileOpsEntry::Directory(listing) => {
                    Some(listing.iter().cloned().sorted().collect::<Vec<_>>().into())
                }
                _ => None,
            })
            .ok_or_else(|| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Environment,
                    "couldn't find dir {:?}",
                    path
                )
            })?;
        Ok(ReadDirOutput { included })
    }

    async fn read_path_metadata_if_exists(
        &self,
        path: CellPathRef<'async_trait>,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        // As on disk, a symlink at a prefix of the path redirects the rest of the path.
        let ancestors: Vec<_> = path.ancestors().skip(1).collect();
        for prefix in ancestors.into_iter().rev() {
            if let Some(TestFileOpsEntry::Symlink(target, symlink)) =
                self.entries.get(&prefix.to_owned())
            {
                let rest = path.strip_prefix(prefix)?;
                return Ok(Some(RawPathMetadata::Symlink {
                    at: Arc::new(prefix.to_owned()),
                    to: RawSymlink::Relative(
                        Arc::new(target.join(rest)),
                        Arc::new(Symlink::new(symlink.target().join(rest.as_str()))),
                    ),
                }));
            }
        }

        self.entries.get(&path.to_owned()).map_or(Ok(None), |e| {
            match e {
                TestFileOpsEntry::File(_data, metadata) => {
                    Ok(RawPathMetadata::File(metadata.to_owned()))
                }
                TestFileOpsEntry::ExternalSymlink(sym) => Ok(RawPathMetadata::Symlink {
                    at: Arc::new(path.to_owned()),
                    to: RawSymlink::External(sym.dupe()),
                }),
                TestFileOpsEntry::Symlink(target, symlink) => Ok(RawPathMetadata::Symlink {
                    at: Arc::new(path.to_owned()),
                    to: RawSymlink::Relative(target.dupe(), symlink.dupe()),
                }),
                TestFileOpsEntry::Directory(..) => Ok(RawPathMetadata::Directory),
            }
            .map(Some)
        })
    }

    async fn is_ignored(
        &self,
        _path: CellPathRef<'async_trait>,
    ) -> buck2_error::Result<FileIgnoreResult> {
        Ok(FileIgnoreResult::Ok)
    }

    async fn buildfiles<'a>(&self, _cell: CellName) -> buck2_error::Result<Arc<[FileNameBuf]>> {
        Ok(Arc::from_iter([FileNameBuf::unchecked_new("BUCK")]))
    }
}

#[derive(Pagable, Allocative)]
pub struct TestCellFileOps(CellName, TestFileOps);

#[pagable_typetag]
#[async_trait]
impl FileOpsDelegate for TestCellFileOps {
    async fn read_file_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<String>> {
        let cell_path = CellPath::new(self.0, path.to_owned());
        FileOps::read_file_if_exists(&self.1, cell_path.as_ref()).await
    }

    /// Return the list of file outputs, sorted.
    async fn read_dir(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let path = CellPath::new(self.0, path.to_owned());
        let simple_entries = FileOps::read_dir(&self.1, path.as_ref()).await?.included;
        Ok(simple_entries
            .iter()
            .map(|e| RawDirEntry {
                file_name: e.file_name.clone().into_inner(),
                file_type: e.file_type,
            })
            .collect())
    }

    async fn read_path_metadata_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        let path = CellPath::new(self.0, path.to_owned());
        FileOps::read_path_metadata_if_exists(&self.1, path.as_ref()).await
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}
