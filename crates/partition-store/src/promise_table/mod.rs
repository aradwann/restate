// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use crate::keys::{KeyKind, TableKey, define_table_key};
use crate::owned_iter::OwnedIterator;
use crate::protobuf_types::PartitionStoreProtobufValue;
use crate::scan::TableScan;
use crate::{PartitionStore, TableKind, TableScanIterationDecision};
use crate::{PartitionStoreTransaction, StorageAccess};
use anyhow::anyhow;
use bytes::Bytes;
use bytestring::ByteString;
use futures::Stream;
use futures_util::stream;
use restate_rocksdb::RocksDbPerfGuard;
use restate_storage_api::Result;
use restate_storage_api::promise_table::{
    OwnedPromiseRow, Promise, PromiseTable, ReadOnlyPromiseTable,
};
use restate_types::identifiers::{PartitionKey, ServiceId, WithPartitionKey};
use std::ops::RangeInclusive;

define_table_key!(
    TableKind::Promise,
    KeyKind::Promise,
    PromiseKey(
        partition_key: PartitionKey,
        service_name: ByteString,
        service_key: Bytes,
        key: ByteString
    )
);

impl PartitionStoreProtobufValue for Promise {
    type ProtobufType = crate::protobuf_types::v1::Promise;
}

fn create_key(service_id: &ServiceId, key: &ByteString) -> PromiseKey {
    PromiseKey::default()
        .partition_key(service_id.partition_key())
        .service_name(service_id.service_name.clone())
        .service_key(service_id.key.as_bytes().clone())
        .key(key.clone())
}

fn get_promise<S: StorageAccess>(
    storage: &mut S,
    service_id: &ServiceId,
    key: &ByteString,
) -> Result<Option<Promise>> {
    let _x = RocksDbPerfGuard::new("get-promise");
    storage.get_value(create_key(service_id, key))
}

fn all_promise<S: StorageAccess>(
    storage: &S,
    range: RangeInclusive<PartitionKey>,
) -> Result<impl Stream<Item = Result<OwnedPromiseRow>> + Send + use<'_, S>> {
    let iter = storage.iterator_from(TableScan::FullScanPartitionKeyRange::<PromiseKey>(range))?;
    Ok(stream::iter(OwnedIterator::new(iter).map(
        |(mut k, mut v)| {
            let key = PromiseKey::deserialize_from(&mut k)?;
            let metadata = Promise::decode(&mut v)?;

            let (partition_key, service_name, service_key, promise_key) = key.into_inner_ok_or()?;

            Ok(OwnedPromiseRow {
                service_id: ServiceId::with_partition_key(
                    partition_key,
                    service_name,
                    ByteString::try_from(service_key)
                        .map_err(|e| anyhow!("Cannot convert to string {e}"))?,
                ),
                key: promise_key,
                metadata,
            })
        },
    )))
}

fn put_promise<S: StorageAccess>(
    storage: &mut S,
    service_id: &ServiceId,
    key: &ByteString,
    metadata: &Promise,
) -> Result<()> {
    storage.put_kv(create_key(service_id, key), metadata)
}

fn delete_all_promises<S: StorageAccess>(storage: &mut S, service_id: &ServiceId) -> Result<()> {
    let prefix_key = PromiseKey::default()
        .partition_key(service_id.partition_key())
        .service_name(service_id.service_name.clone())
        .service_key(service_id.key.as_bytes().clone());

    let keys = storage.for_each_key_value_in_place(
        TableScan::SinglePartitionKeyPrefix(service_id.partition_key(), prefix_key),
        |k, _| TableScanIterationDecision::Emit(Ok(Bytes::copy_from_slice(k))),
    )?;

    for k in keys {
        let key = k?;
        storage.delete_cf(TableKind::Promise, key)?;
    }
    Ok(())
}

impl ReadOnlyPromiseTable for PartitionStore {
    async fn get_promise(
        &mut self,
        service_id: &ServiceId,
        key: &ByteString,
    ) -> Result<Option<Promise>> {
        self.assert_partition_key(service_id)?;
        get_promise(self, service_id, key)
    }

    fn all_promises(
        &self,
        range: RangeInclusive<PartitionKey>,
    ) -> Result<impl Stream<Item = Result<OwnedPromiseRow>> + Send> {
        all_promise(self, range)
    }
}

impl ReadOnlyPromiseTable for PartitionStoreTransaction<'_> {
    async fn get_promise(
        &mut self,
        service_id: &ServiceId,
        key: &ByteString,
    ) -> Result<Option<Promise>> {
        self.assert_partition_key(service_id)?;
        get_promise(self, service_id, key)
    }

    fn all_promises(
        &self,
        range: RangeInclusive<PartitionKey>,
    ) -> Result<impl Stream<Item = Result<OwnedPromiseRow>> + Send> {
        all_promise(self, range)
    }
}

impl PromiseTable for PartitionStoreTransaction<'_> {
    async fn put_promise(
        &mut self,
        service_id: &ServiceId,
        key: &ByteString,
        promise: &Promise,
    ) -> Result<()> {
        self.assert_partition_key(service_id)?;
        put_promise(self, service_id, key, promise)
    }

    async fn delete_all_promises(&mut self, service_id: &ServiceId) -> Result<()> {
        self.assert_partition_key(service_id)?;
        delete_all_promises(self, service_id)
    }
}
