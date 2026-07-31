/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Core dice computations relating to cells

use std::collections::BTreeMap;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::cells::execution_name::CellExecutionNames;
use buck2_core::cells::name::CellName;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_error::BuckErrorContext;
use derive_more::Display;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::DiceTransactionUpdater;
use dice::InjectedKey;
use dice::InvalidationSourcePriority;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::PagableValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::legacy_configs::cells::BuckConfigBasedCells;
use crate::legacy_configs::dice::HasLegacyConfigs;

#[async_trait]
pub trait HasCellResolver {
    async fn get_cell_resolver(&mut self) -> buck2_error::Result<CellResolver>;

    async fn is_cell_resolver_key_set(&mut self) -> buck2_error::Result<bool>;

    async fn get_cell_execution_names(&mut self) -> buck2_error::Result<Arc<CellExecutionNames>>;

    async fn get_cell_alias_resolver(
        &mut self,
        cell: CellName,
    ) -> buck2_error::Result<CellAliasResolver>;

    async fn get_cell_alias_resolver_for_dir(
        &mut self,
        dir: &ProjectRelativePath,
    ) -> buck2_error::Result<CellAliasResolver>;
}

pub trait SetCellResolver {
    fn set_cell_resolver(&mut self, cell_resolver: CellResolver) -> buck2_error::Result<()>;

    fn set_none_cell_resolver(&mut self) -> buck2_error::Result<()>;
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("{:?}", self)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct CellResolverKey;

impl InjectedKey for CellResolverKey {
    type Value = Option<CellResolver>;

    /// Lazy rebinding of `canonical_v1` execution views assumes any cell topology change makes the
    /// next command's DICE state non-equivalent, so this must stay a structural comparison.
    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Some(x), Some(y)) => x == y,
            (None, None) => true,
            (_, _) => false,
        }
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::Ignored
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        PagableValueSerialize::<Self::Value>::new()
    }
}

#[async_trait]
impl HasCellResolver for DiceComputations<'_> {
    async fn get_cell_resolver(&mut self) -> buck2_error::Result<CellResolver> {
        self.compute(&CellResolverKey).await?.ok_or_else(|| {
            panic!("Tried to retrieve CellResolverKey from the graph, but key has None value")
        })
    }

    async fn is_cell_resolver_key_set(&mut self) -> buck2_error::Result<bool> {
        Ok(self.compute(&CellResolverKey).await?.is_some())
    }

    async fn get_cell_execution_names(&mut self) -> buck2_error::Result<Arc<CellExecutionNames>> {
        self.compute(&CellExecutionNamesKey).await?
    }

    async fn get_cell_alias_resolver(
        &mut self,
        cell: CellName,
    ) -> buck2_error::Result<CellAliasResolver> {
        Ok(self.compute(&CellAliasResolverKey(cell)).await??)
    }

    async fn get_cell_alias_resolver_for_dir(
        &mut self,
        dir: &ProjectRelativePath,
    ) -> buck2_error::Result<CellAliasResolver> {
        let cell = self.get_cell_resolver().await?.find(dir);
        self.get_cell_alias_resolver(cell).await
    }
}

/// Execution names come from exactly one place: the root cell's `[cell_execution_names]`. Letting a
/// cell declare its own would mean reading every cell's `.buckconfig` from under `get_artifact_fs`,
/// which is on essentially every build path, and an external cell's is only readable once the cell
/// is materialized, so every declared cell would be fetched on every command.
#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("{:?}", self)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct CellExecutionNamesKey;

#[async_trait]
impl Key for CellExecutionNamesKey {
    type Value = buck2_error::Result<Arc<CellExecutionNames>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_config = ctx.get_legacy_config_for_cell(resolver.root_cell()).await?;
        let root_aliases = resolver.root_cell_cell_alias_resolver();
        let mut configured: BTreeMap<CellName, String> = BTreeMap::new();
        for (alias, name) in
            BuckConfigBasedCells::get_cell_execution_names_from_config(&root_config)?
        {
            let cell = root_aliases
                .resolve(alias.as_str())
                .with_buck_error_context(|| {
                    format!("`[cell_execution_names]` names `{alias}`, which is not a known cell")
                })?;
            configured.insert(cell, name);
        }

        let names = resolver.cells().map(|(cell, _)| {
            let name = configured
                .get(&cell)
                .cloned()
                .unwrap_or_else(|| cell.as_str().to_owned());
            (cell, name)
        });
        Ok(Arc::new(CellExecutionNames::new(names)?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            (_, _) => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

/// Only used for cell alias resolvers parsed within dice, currently those for external cells
#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct CellAliasResolverKey(CellName);

#[async_trait]
impl Key for CellAliasResolverKey {
    type Value = buck2_error::Result<CellAliasResolver>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_aliases = resolver.root_cell_cell_alias_resolver();
        let config = ctx.get_legacy_config_for_cell(self.0).await?;
        // Cell alias resolvers that are parsed within dice differ from those outside of dice in
        // that they cannot create new cells, and so respect only their `cell_aliases` section, not
        // their `cells` section. This is the expected behavior for external cells, moving other
        // cell resolver parsing into dice would require this code to be adjusted.
        CellAliasResolver::new_for_non_root_cell(
            self.0,
            root_aliases,
            BuckConfigBasedCells::get_cell_aliases_from_config(&config)?,
        )
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            (_, _) => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

impl SetCellResolver for DiceTransactionUpdater {
    fn set_cell_resolver(&mut self, cell_resolver: CellResolver) -> buck2_error::Result<()> {
        Ok(self.changed_to(vec![(CellResolverKey, Some(cell_resolver))])?)
    }

    fn set_none_cell_resolver(&mut self) -> buck2_error::Result<()> {
        Ok(self.changed_to(vec![(CellResolverKey, None)])?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::cells::external::ExternalCellOrigin;
    use buck2_core::cells::external::GitCellSetup;
    use buck2_core::cells::instance::CellInstance;
    use buck2_core::cells::nested::NestedCells;
    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;

    use super::*;

    fn resolver(
        cell_root: &str,
        external: Option<ExternalCellOrigin>,
    ) -> buck2_error::Result<CellResolver> {
        let root_name = CellName::testing_new("root");
        let cell_name = CellName::testing_new("sample");
        let root_path = CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new(String::new()));
        let cell_path =
            CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new(cell_root.to_owned()));
        let roots = [
            (root_name, root_path.as_path()),
            (cell_name, cell_path.as_path()),
        ];
        CellResolver::new(
            vec![
                CellInstance::new(
                    root_name,
                    root_path.clone(),
                    None,
                    NestedCells::from_cell_roots(&roots, root_path.as_path()),
                )?,
                CellInstance::new(
                    cell_name,
                    cell_path.clone(),
                    external,
                    NestedCells::from_cell_roots(&roots, cell_path.as_path()),
                )?,
            ],
            CellAliasResolver::new(root_name, Default::default())?,
        )
    }

    fn git(commit: &str) -> Option<ExternalCellOrigin> {
        Some(ExternalCellOrigin::Git(GitCellSetup {
            git_origin: Arc::from("https://example.com/sample.git"),
            commit: Arc::from(commit),
            object_format: None,
        }))
    }

    /// Pins the invariant documented on [`CellResolverKey::equality`].
    #[test]
    fn cell_resolver_key_equality_tracks_topology() -> buck2_error::Result<()> {
        let eq = <CellResolverKey as InjectedKey>::equality;
        let base = resolver("third-party/sample", None)?;

        assert!(eq(
            &Some(base.clone()),
            &Some(resolver("third-party/sample", None)?)
        ));
        assert!(!eq(
            &Some(base.clone()),
            &Some(resolver("development/sample", None)?)
        ));

        let commit_a = "0123456789abcdef0123456789abcdef01234567";
        let with_origin = resolver("third-party/sample", git(commit_a))?;
        assert!(!eq(&Some(base.clone()), &Some(with_origin.clone())));
        assert!(eq(
            &Some(with_origin.clone()),
            &Some(resolver("third-party/sample", git(commit_a))?)
        ));
        assert!(!eq(
            &Some(with_origin.clone()),
            &Some(resolver(
                "third-party/sample",
                git("89abcdef0123456789abcdef0123456789abcdef")
            )?)
        ));
        // The origin *kind* must discriminate too, not just the git commit.
        assert!(!eq(
            &Some(with_origin),
            &Some(resolver(
                "third-party/sample",
                Some(ExternalCellOrigin::Bundled(CellName::testing_new("sample")))
            )?)
        ));

        assert!(!eq(&Some(base), &None));
        assert!(eq(&None, &None));
        Ok(())
    }
}
