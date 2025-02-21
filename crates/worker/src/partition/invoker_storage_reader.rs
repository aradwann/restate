// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use restate_invoker_api::{EagerState, JournalMetadata};
use restate_storage_api::invocation_status_table::{
    InvocationStatus, ReadOnlyInvocationStatusTable,
};
use restate_storage_api::state_table::ReadOnlyStateTable;
use restate_storage_api::{journal_table as journal_table_v1, journal_table_v2};
use restate_types::identifiers::InvocationId;
use restate_types::identifiers::ServiceId;
use restate_types::service_protocol::ServiceProtocolVersion;
use std::vec::IntoIter;

#[derive(Debug, thiserror::Error)]
pub enum InvokerStorageReaderError {
    #[error("not invoked")]
    NotInvoked,
    #[error(transparent)]
    Storage(#[from] restate_storage_api::StorageError),
}

#[derive(Debug, Clone)]
pub(crate) struct InvokerStorageReader<Storage>(Storage);

impl<Storage> InvokerStorageReader<Storage> {
    pub(crate) fn new(storage: Storage) -> Self {
        InvokerStorageReader(storage)
    }
}

impl<Storage> restate_invoker_api::JournalReader for InvokerStorageReader<Storage>
where
    for<'a> Storage: journal_table_v1::ReadOnlyJournalTable
        + journal_table_v2::ReadOnlyJournalTable
        + ReadOnlyInvocationStatusTable
        + Send
        + 'a,
{
    type JournalStream = stream::Iter<IntoIter<restate_invoker_api::journal_reader::JournalEntry>>;
    type Error = InvokerStorageReaderError;

    async fn read_journal<'a>(
        &'a mut self,
        invocation_id: &'a InvocationId,
    ) -> Result<(JournalMetadata, Self::JournalStream), Self::Error> {
        let invocation_status = self.0.get_invocation_status(invocation_id).await?;

        if let InvocationStatus::Invoked(invoked_status) = invocation_status {
            let journal_metadata = JournalMetadata::new(
                invoked_status.journal_metadata.length,
                invoked_status.journal_metadata.span_context,
                invoked_status.pinned_deployment.clone(),
                // SAFETY: this value is used by the invoker, it's ok if it's not in sync
                unsafe { invoked_status.timestamps.modification_time() },
            );

            let journal_stream = if invoked_status
                .pinned_deployment
                .is_some_and(|p| p.service_protocol_version >= ServiceProtocolVersion::V4)
            {
                // If pinned service protocol version exists and >= V4, we need to read from Journal Table V2!
                journal_table_v2::ReadOnlyJournalTable::get_journal(
                    &mut self.0,
                    *invocation_id,
                    journal_metadata.length,
                )?
                .map(|entry| {
                    entry
                        .map_err(InvokerStorageReaderError::Storage)
                        .map(|(_, entry)| {
                            restate_invoker_api::journal_reader::JournalEntry::JournalV2(entry)
                        })
                })
                // TODO: Update invoker to maintain transaction while reading the journal stream: See https://github.com/restatedev/restate/issues/275
                // collecting the stream because we cannot keep the transaction open
                .try_collect::<Vec<_>>()
                .await?
            } else {
                journal_table_v1::ReadOnlyJournalTable::get_journal(
                    &mut self.0,
                    invocation_id,
                    journal_metadata.length,
                )?
                .map(|entry| {
                    entry
                        .map_err(InvokerStorageReaderError::Storage)
                        .map(|(_, journal_entry)| match journal_entry {
                            journal_table_v1::JournalEntry::Entry(entry) => {
                                restate_invoker_api::journal_reader::JournalEntry::JournalV1(
                                    entry.erase_enrichment(),
                                )
                            }
                            journal_table_v1::JournalEntry::Completion(_) => {
                                panic!("should only read entries when reading the journal")
                            }
                        })
                })
                // TODO: Update invoker to maintain transaction while reading the journal stream: See https://github.com/restatedev/restate/issues/275
                // collecting the stream because we cannot keep the transaction open
                .try_collect::<Vec<_>>()
                .await?
            };

            Ok((journal_metadata, stream::iter(journal_stream)))
        } else {
            Err(InvokerStorageReaderError::NotInvoked)
        }
    }
}

impl<Storage> restate_invoker_api::StateReader for InvokerStorageReader<Storage>
where
    for<'a> Storage: ReadOnlyStateTable + Send + 'a,
{
    type StateIter = IntoIter<(Bytes, Bytes)>;
    type Error = InvokerStorageReaderError;

    async fn read_state<'a>(
        &'a mut self,
        service_id: &'a ServiceId,
    ) -> Result<EagerState<Self::StateIter>, Self::Error> {
        let user_states = self
            .0
            .get_all_user_states_for_service(service_id)?
            .try_collect::<Vec<_>>()
            .await?;

        Ok(EagerState::new_complete(user_states.into_iter()))
    }
}
