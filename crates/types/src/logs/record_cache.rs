// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use moka::{
    ops::compute::Op,
    policy::EvictionPolicy,
    sync::{Cache, CacheBuilder},
};

use super::{LogletId, LogletOffset, Record, SequenceNumber};

/// Unique record key across different loglets.
type RecordKey = (LogletId, LogletOffset);

/// A a simple LRU-based record cache.
///
/// This can be safely shared between all ReplicatedLoglet(s) and the LocalSequencers or the
/// RemoteSequencers
#[derive(Clone)]
pub struct RecordCache {
    inner: Option<Cache<RecordKey, Record, ahash::RandomState>>,
}

impl RecordCache {
    /// Creates a new instance of RecordCache. If memory budget is 0
    /// cache will be disabled
    pub fn new(memory_budget_bytes: usize) -> Self {
        let inner = if memory_budget_bytes > 0 {
            Some(
                CacheBuilder::default()
                    .name("ReplicatedLogRecordCache")
                    .weigher(|_, record: &Record| {
                        record
                            .estimated_encode_size()
                            .try_into()
                            .unwrap_or(u32::MAX)
                    })
                    .max_capacity(memory_budget_bytes.try_into().unwrap_or(u64::MAX))
                    .eviction_policy(EvictionPolicy::lru())
                    .build_with_hasher(ahash::RandomState::default()),
            )
        } else {
            None
        };

        Self { inner }
    }

    fn insert(&self, loglet_id: LogletId, offset: LogletOffset, record: &Record) {
        let Some(ref inner) = self.inner else {
            return;
        };

        inner
            .entry((loglet_id, offset))
            .and_compute_with(|existing| {
                let Some(existing) = existing else {
                    return Op::Put(record.clone());
                };
                match (
                    existing.value().body().is_encoded(),
                    record.body().is_encoded(),
                ) {
                    // both are encoded, we don't want to replace the existing value.
                    (true, true) | (false, false) | (false, true) => Op::Nop,
                    // replace the existing value if the new one is deserialized.
                    (true, false) => Op::Put(record.clone()),
                }
            });
    }

    /// Writes a record to cache externally
    pub fn add(&self, loglet_id: LogletId, offset: LogletOffset, record: &Record) {
        self.insert(loglet_id, offset, record);
    }

    /// Removes the record from cache if it exists
    pub fn invalidate_record(&self, loglet_id: LogletId, offset: LogletOffset) {
        let Some(ref inner) = self.inner else {
            return;
        };
        inner.invalidate(&(loglet_id, offset));
    }

    /// Extend cache with records
    pub fn extend<I: AsRef<[Record]>>(
        &self,
        loglet_id: LogletId,
        mut first_offset: LogletOffset,
        records: I,
    ) {
        if self.inner.is_none() {
            return;
        };

        for record in records.as_ref() {
            self.insert(loglet_id, first_offset, record);
            first_offset = first_offset.next();
        }
    }

    /// Get a for given loglet id and offset.
    pub fn get(&self, loglet_id: LogletId, offset: LogletOffset) -> Option<Record> {
        let inner = self.inner.as_ref()?;

        inner.get(&(loglet_id, offset))
    }
}
