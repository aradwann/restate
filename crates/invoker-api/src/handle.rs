// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use super::Effect;
use super::JournalMetadata;

use restate_errors::NotRunningError;
use restate_types::identifiers::PartitionKey;
use restate_types::identifiers::{EntryIndex, InvocationId, PartitionLeaderEpoch};
use restate_types::invocation::InvocationTarget;
use restate_types::journal::raw::PlainRawEntry;
use restate_types::journal::Completion;
use std::future::Future;
use std::ops::RangeInclusive;
use tokio::sync::mpsc;

#[derive(Debug, Eq, PartialEq, Default)]
pub enum InvokeInputJournal {
    #[default]
    NoCachedJournal,
    CachedJournal(JournalMetadata, Vec<PlainRawEntry>),
}

pub trait InvokerHandle<SR> {
    fn invoke(
        &mut self,
        partition: PartitionLeaderEpoch,
        invocation_id: InvocationId,
        invocation_target: InvocationTarget,
        journal: InvokeInputJournal,
    ) -> impl Future<Output = Result<(), NotRunningError>> + Send;

    fn notify_completion(
        &mut self,
        partition: PartitionLeaderEpoch,
        invocation_id: InvocationId,
        completion: Completion,
    ) -> impl Future<Output = Result<(), NotRunningError>> + Send;

    fn notify_stored_entry_ack(
        &mut self,
        partition: PartitionLeaderEpoch,
        invocation_id: InvocationId,
        entry_index: EntryIndex,
    ) -> impl Future<Output = Result<(), NotRunningError>> + Send;

    fn abort_all_partition(
        &mut self,
        partition: PartitionLeaderEpoch,
    ) -> impl Future<Output = Result<(), NotRunningError>> + Send;

    fn abort_invocation(
        &mut self,
        partition_leader_epoch: PartitionLeaderEpoch,
        invocation_id: InvocationId,
    ) -> impl Future<Output = Result<(), NotRunningError>> + Send;

    fn register_partition(
        &mut self,
        partition: PartitionLeaderEpoch,
        partition_key_range: RangeInclusive<PartitionKey>,
        storage_reader: SR,
        sender: mpsc::Sender<Effect>,
    ) -> impl Future<Output = Result<(), NotRunningError>> + Send;
}
