// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;
use move_binary_format::{
    deserializer::DeserializerConfig,
    errors::{PartialVMError, PartialVMResult, VMResult},
    file_format_common::{IDENTIFIER_SIZE_MAX, VERSION_MAX},
    CompiledModule,
};
use move_bytecode_utils::compiled_module_viewer::CompiledModuleView;
use move_core_types::{
    account_address::AccountAddress,
    effects::{AccountChangeSet, ChangeSet, Op},
    identifier::{IdentStr, Identifier},
    language_storage::{ModuleId, StructTag},
    metadata::Metadata,
    value::MoveTypeLayout,
    vm_status::StatusCode,
};
use move_vm_types::{
    code::ModuleBytesStorage,
    resolver::{resource_size, ResourceResolver},
    sha3_256,
};
use std::{
    collections::{btree_map, BTreeMap},
    fmt::Debug,
};

use crate::state_view::{Checksum, ChecksumStorage};

/// A dummy storage containing no modules or resources.
#[derive(Debug, Clone)]
pub struct BlankStorage;

impl Default for BlankStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl BlankStorage {
    pub fn new() -> Self {
        Self
    }
}

impl ModuleBytesStorage for BlankStorage {
    fn fetch_module_bytes(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
    ) -> VMResult<Option<Bytes>> {
        Ok(None)
    }
}

impl ChecksumStorage for BlankStorage {
    fn fetch_checksum(
        &self,
        _address: &AccountAddress,
        _module_name: &IdentStr,
    ) -> VMResult<Option<Checksum>> {
        Ok(None)
    }
}

impl ResourceResolver for BlankStorage {
    fn get_resource_bytes_with_metadata_and_layout(
        &self,
        _address: &AccountAddress,
        _tag: &StructTag,
        _metadata: &[Metadata],
        _maybe_layout: Option<&MoveTypeLayout>,
    ) -> PartialVMResult<(Option<Bytes>, usize)> {
        Ok((None, 0))
    }
}

/// Simple in-memory storage for modules and resources under an account.
#[derive(Debug, Clone)]
struct InMemoryAccountStorage {
    resources: BTreeMap<StructTag, Bytes>,
    modules: BTreeMap<Identifier, Bytes>,
    checksums: BTreeMap<Identifier, Checksum>,
}

/// Simple in-memory storage that can be used as a Move VM storage backend for testing purposes.
#[derive(Debug, Clone)]
pub struct InMemoryStorage {
    accounts: BTreeMap<AccountAddress, InMemoryAccountStorage>,
}

impl CompiledModuleView for InMemoryStorage {
    type Item = CompiledModule;

    fn view_compiled_module(&self, id: &ModuleId) -> anyhow::Result<Option<Self::Item>> {
        Ok(match self.get_module(id)? {
            Some(bytes) => {
                let config = DeserializerConfig::new(VERSION_MAX, IDENTIFIER_SIZE_MAX);
                Some(CompiledModule::deserialize_with_config(&bytes, &config)?)
            }
            None => None,
        })
    }
}

fn apply_changes<K, V>(
    map: &mut BTreeMap<K, V>,
    changes: impl IntoIterator<Item = (K, Op<V>)>,
) -> PartialVMResult<()>
where
    K: Ord + Debug,
{
    use btree_map::Entry::*;
    use Op::*;

    for (k, op) in changes.into_iter() {
        match (map.entry(k), op) {
            (Occupied(entry), New(_)) => {
                return Err(
                    PartialVMError::new(StatusCode::STORAGE_ERROR).with_message(format!(
                        "Failed to apply changes -- key {:?} already exists",
                        entry.key()
                    )),
                )
            }
            (Occupied(entry), Delete) => {
                entry.remove();
            }
            (Occupied(entry), Modify(val)) => {
                *entry.into_mut() = val;
            }
            (Vacant(entry), New(val)) => {
                entry.insert(val);
            }
            (Vacant(entry), Delete | Modify(_)) => {
                return Err(
                    PartialVMError::new(StatusCode::STORAGE_ERROR).with_message(format!(
                        "Failed to apply changes -- key {:?} does not exist",
                        entry.key()
                    )),
                )
            }
        }
    }
    Ok(())
}

fn get_or_insert<K, V, F>(map: &mut BTreeMap<K, V>, key: K, make_val: F) -> &mut V
where
    K: Ord,
    F: FnOnce() -> V,
{
    use btree_map::Entry::*;

    match map.entry(key) {
        Occupied(entry) => entry.into_mut(),
        Vacant(entry) => entry.insert(make_val()),
    }
}

impl InMemoryAccountStorage {
    fn apply(&mut self, account_changeset: AccountChangeSet) -> PartialVMResult<()> {
        let resources = account_changeset.into_resources();
        apply_changes(&mut self.resources, resources)?;
        Ok(())
    }

    fn new() -> Self {
        Self {
            modules: BTreeMap::new(),
            checksums: BTreeMap::new(),
            resources: BTreeMap::new(),
        }
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStorage {
    pub fn apply_extended(&mut self, changeset: ChangeSet) -> PartialVMResult<()> {
        for (addr, account_changeset) in changeset.into_inner() {
            self.accounts
                .entry(addr)
                .or_insert_with(InMemoryAccountStorage::new)
                .apply(account_changeset)?;
        }

        Ok(())
    }

    pub fn apply(&mut self, changeset: ChangeSet) -> PartialVMResult<()> {
        self.apply_extended(changeset)
    }

    pub fn new() -> Self {
        Self {
            accounts: BTreeMap::new(),
        }
    }

    /// Adds serialized module bytes to this storage.
    pub fn add_module_bytes(
        &mut self,
        address: &AccountAddress,
        module_name: &IdentStr,
        bytes: Bytes,
    ) -> Checksum {
        let checksum = sha3_256(&bytes);

        let account = get_or_insert(&mut self.accounts, *address, || {
            InMemoryAccountStorage::new()
        });
        account.modules.insert(module_name.to_owned(), bytes);
        account.checksums.insert(module_name.to_owned(), checksum);
        checksum
    }

    pub fn publish_or_overwrite_resource(
        &mut self,
        addr: AccountAddress,
        struct_tag: StructTag,
        blob: Vec<u8>,
    ) {
        let account = get_or_insert(&mut self.accounts, addr, InMemoryAccountStorage::new);
        account.resources.insert(struct_tag, blob.into());
    }

    fn get_module(&self, module_id: &ModuleId) -> PartialVMResult<Option<Bytes>> {
        Ok(self
            .accounts
            .get(module_id.address())
            .and_then(|account_storage| account_storage.modules.get(module_id.name()).cloned()))
    }
}

impl ModuleBytesStorage for InMemoryStorage {
    fn fetch_module_bytes(
        &self,
        address: &AccountAddress,
        module_name: &IdentStr,
    ) -> VMResult<Option<Bytes>> {
        Ok(self
            .accounts
            .get(address)
            .and_then(|account_storage| account_storage.modules.get(module_name).cloned()))
    }
}

impl ChecksumStorage for InMemoryStorage {
    fn fetch_checksum(
        &self,
        address: &AccountAddress,
        module_name: &IdentStr,
    ) -> VMResult<Option<Checksum>> {
        Ok(self
            .accounts
            .get(address)
            .and_then(|account_storage| account_storage.checksums.get(module_name))
            .cloned())
    }
}

impl ResourceResolver for InMemoryStorage {
    fn get_resource_bytes_with_metadata_and_layout(
        &self,
        address: &AccountAddress,
        tag: &StructTag,
        _metadata: &[Metadata],
        _maybe_layout: Option<&MoveTypeLayout>,
    ) -> PartialVMResult<(Option<Bytes>, usize)> {
        if let Some(account_storage) = self.accounts.get(address) {
            let buf = account_storage.resources.get(tag).cloned();
            let buf_size = resource_size(&buf);
            return Ok((buf, buf_size));
        }
        Ok((None, 0))
    }
}
