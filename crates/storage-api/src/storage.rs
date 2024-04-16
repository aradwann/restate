// Copyright (c) 2024 - Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

pub mod v1 {
    #![allow(warnings)]
    #![allow(clippy::all)]
    #![allow(unknown_lints)]

    include!(concat!(
        env!("OUT_DIR"),
        "/dev.restate.storage.domain.v1.rs"
    ));

    pub mod pb_conversion {
        use std::collections::HashSet;
        use std::str::FromStr;

        use anyhow::anyhow;
        use bytes::{Buf, BufMut, Bytes};
        use bytestring::ByteString;
        use opentelemetry::trace::TraceState;
        use prost::Message;

        use restate_types::errors::{IdDecodeError, InvocationError};
        use restate_types::invocation::{InvocationTermination, TerminationFlavor};
        use restate_types::journal::enriched::AwakeableEnrichmentResult;
        use restate_types::storage::{
            StorageCodecKind, StorageDecode, StorageDecodeError, StorageEncode, StorageEncodeError,
        };
        use restate_types::time::MillisSinceEpoch;
        use restate_types::GenerationalNodeId;

        use crate::storage::v1::dedup_sequence_number::Variant;
        use crate::storage::v1::enriched_entry_header::{
            Awakeable, BackgroundCall, ClearAllState, ClearState, CompleteAwakeable, Custom,
            GetState, GetStateKeys, Input, Invoke, Output, SetState, SideEffect, Sleep,
        };
        use crate::storage::v1::invocation_status::{Completed, Free, Inboxed, Invoked, Suspended};
        use crate::storage::v1::journal_entry::completion_result::{Empty, Failure, Success};
        use crate::storage::v1::journal_entry::{completion_result, CompletionResult, Entry, Kind};
        use crate::storage::v1::outbox_message::{
            OutboxCancel, OutboxKill, OutboxServiceInvocation, OutboxServiceInvocationResponse,
        };
        use crate::storage::v1::service_invocation_response_sink::{
            Ingress, NewInvocation, PartitionProcessor, ResponseSink,
        };
        use crate::storage::v1::{
            enriched_entry_header, inbox_entry, invocation_resolution_result, invocation_status,
            invocation_target, outbox_message, response_result, service_status, source,
            span_relation, timer, BackgroundCallResolutionResult, DedupSequenceNumber, Duration,
            EnrichedEntryHeader, EpochSequenceNumber, FullInvocationId, Header,
            IdempotencyMetadata, IdempotentRequestMetadata, InboxEntry, InvocationResolutionResult,
            InvocationStatus, InvocationTarget, JournalEntry, JournalMeta, KvPair, OutboxMessage,
            ResponseResult, ServiceId, ServiceInvocation, ServiceInvocationResponseSink,
            ServiceStatus, Source, SpanContext, SpanRelation, StateMutation, Timer,
        };
        use crate::StorageError;

        /// Error type for conversion related problems (e.g. Rust <-> Protobuf)
        #[derive(Debug, thiserror::Error)]
        pub enum ConversionError {
            #[error("missing field '{0}'")]
            MissingField(&'static str),
            #[error("unexpected enum variant {1} for field '{0}'")]
            UnexpectedEnumVariant(&'static str, i32),
            #[error("invalid data: {0}")]
            InvalidData(anyhow::Error),
        }

        impl ConversionError {
            pub fn invalid_data(source: impl Into<anyhow::Error>) -> Self {
                ConversionError::InvalidData(source.into())
            }

            pub fn missing_field(field: &'static str) -> Self {
                ConversionError::MissingField(field)
            }

            pub fn unexpected_enum_variant(
                field: &'static str,
                enum_variant: impl Into<i32>,
            ) -> Self {
                ConversionError::UnexpectedEnumVariant(field, enum_variant.into())
            }
        }

        impl From<IdDecodeError> for ConversionError {
            fn from(value: IdDecodeError) -> Self {
                ConversionError::invalid_data(value)
            }
        }

        impl From<ConversionError> for StorageError {
            fn from(value: ConversionError) -> Self {
                StorageError::Conversion(value.into())
            }
        }

        impl TryFrom<ServiceStatus> for restate_types::identifiers::InvocationUuid {
            type Error = ConversionError;

            fn try_from(value: ServiceStatus) -> Result<Self, Self::Error> {
                Ok(
                    match value
                        .status
                        .ok_or(ConversionError::missing_field("status"))?
                    {
                        service_status::Status::Locked(locked) => {
                            try_bytes_into_invocation_uuid(locked.invocation_uuid)?
                        }
                    },
                )
            }
        }

        impl From<crate::service_status_table::VirtualObjectStatus> for ServiceStatus {
            fn from(value: crate::service_status_table::VirtualObjectStatus) -> Self {
                match value {
                    crate::service_status_table::VirtualObjectStatus::Locked(invocation_id) => {
                        ServiceStatus {
                            status: Some(service_status::Status::Locked(service_status::Locked {
                                invocation_uuid: invocation_id
                                    .invocation_uuid()
                                    .to_bytes()
                                    .to_vec()
                                    .into(),
                            })),
                        }
                    }
                    crate::service_status_table::VirtualObjectStatus::Unlocked => {
                        unreachable!("Nothing should be stored for unlocked")
                    }
                }
            }
        }

        impl TryFrom<InvocationStatus> for crate::invocation_status_table::InvocationStatus {
            type Error = ConversionError;

            fn try_from(value: InvocationStatus) -> Result<Self, Self::Error> {
                let result = match value
                    .status
                    .ok_or(ConversionError::missing_field("status"))?
                {
                    invocation_status::Status::Inboxed(inboxed) => {
                        let invocation_metadata =
                            crate::invocation_status_table::InboxedInvocation::try_from(inboxed)?;
                        crate::invocation_status_table::InvocationStatus::Inboxed(
                            invocation_metadata,
                        )
                    }
                    invocation_status::Status::Invoked(invoked) => {
                        let invocation_metadata =
                            crate::invocation_status_table::InFlightInvocationMetadata::try_from(
                                invoked,
                            )?;
                        crate::invocation_status_table::InvocationStatus::Invoked(
                            invocation_metadata,
                        )
                    }
                    invocation_status::Status::Suspended(suspended) => {
                        let (metadata, waiting_for_completed_entries) = suspended.try_into()?;
                        crate::invocation_status_table::InvocationStatus::Suspended {
                            metadata,
                            waiting_for_completed_entries,
                        }
                    }
                    invocation_status::Status::Completed(completed) => {
                        crate::invocation_status_table::InvocationStatus::Completed(
                            completed.try_into()?,
                        )
                    }
                    invocation_status::Status::Free(_) => {
                        crate::invocation_status_table::InvocationStatus::Free
                    }
                };

                Ok(result)
            }
        }

        impl From<crate::invocation_status_table::InvocationStatus> for InvocationStatus {
            fn from(value: crate::invocation_status_table::InvocationStatus) -> Self {
                let status = match value {
                    crate::invocation_status_table::InvocationStatus::Inboxed(inboxed_status) => {
                        invocation_status::Status::Inboxed(Inboxed::from(inboxed_status))
                    }
                    crate::invocation_status_table::InvocationStatus::Invoked(invoked_status) => {
                        invocation_status::Status::Invoked(Invoked::from(invoked_status))
                    }
                    crate::invocation_status_table::InvocationStatus::Suspended {
                        metadata,
                        waiting_for_completed_entries,
                    } => invocation_status::Status::Suspended(Suspended::from((
                        metadata,
                        waiting_for_completed_entries,
                    ))),
                    crate::invocation_status_table::InvocationStatus::Completed(completed) => {
                        invocation_status::Status::Completed(Completed::from(completed))
                    }
                    crate::invocation_status_table::InvocationStatus::Free => {
                        invocation_status::Status::Free(Free {})
                    }
                };

                InvocationStatus {
                    status: Some(status),
                }
            }
        }

        #[derive(derive_more::From)]
        pub struct InvocationStatusStorageSerde(
            pub crate::invocation_status_table::InvocationStatus,
        );

        impl InvocationStatusStorageSerde {
            pub fn into_inner(self) -> crate::invocation_status_table::InvocationStatus {
                self.0
            }
        }

        impl StorageEncode for InvocationStatusStorageSerde {
            const DEFAULT_CODEC: StorageCodecKind = StorageCodecKind::Protobuf;

            fn encode<B: BufMut>(&self, buf: &mut B) -> Result<(), StorageEncodeError> {
                InvocationStatus::from(self.0.clone())
                    .encode(buf)
                    .map_err(|err| StorageEncodeError::EncodeValue(err.into()))
            }
        }

        impl StorageDecode for InvocationStatusStorageSerde {
            fn decode(buf: &[u8], kind: StorageCodecKind) -> Result<Self, StorageDecodeError>
            where
                Self: Sized,
            {
                match kind {
                    StorageCodecKind::Protobuf => {
                        let invocation_status = InvocationStatus::decode(buf)
                            .map_err(|err| StorageDecodeError::DecodeValue(err.into()))?;
                        crate::invocation_status_table::InvocationStatus::try_from(
                            invocation_status,
                        )
                        .map_err(|err| StorageDecodeError::DecodeValue(err.into()))
                        .map(|value| InvocationStatusStorageSerde(value))
                    }
                    codec => Err(StorageDecodeError::UnsupportedCodecKind(codec)),
                }
            }
        }

        impl TryFrom<Invoked> for crate::invocation_status_table::InFlightInvocationMetadata {
            type Error = ConversionError;

            fn try_from(value: Invoked) -> Result<Self, Self::Error> {
                let invocation_target = restate_types::invocation::InvocationTarget::try_from(
                    value
                        .invocation_target
                        .ok_or(ConversionError::missing_field("invocation_target"))?,
                )?;

                let service_id = value
                    .service_id
                    .ok_or(ConversionError::missing_field("service_id"))?
                    .try_into()?;

                let method_name = value.method_name.try_into().map_err(|e| {
                    ConversionError::InvalidData(anyhow!("Cannot decode method_name string {e}"))
                })?;
                let deployment_id =
                    value.deployment_id.and_then(
                        |one_of_deployment_id| match one_of_deployment_id {
                            invocation_status::invoked::DeploymentId::None(_) => None,
                            invocation_status::invoked::DeploymentId::Value(id) => {
                                Some(id.parse().expect("valid deployment id"))
                            }
                        },
                    );

                let journal_metadata = crate::invocation_status_table::JournalMetadata::try_from(
                    value
                        .journal_meta
                        .ok_or(ConversionError::missing_field("journal_meta"))?,
                )?;
                let response_sinks = value
                    .response_sinks
                    .into_iter()
                    .map(|s| {
                        Ok::<_, ConversionError>(Option::<
                            restate_types::invocation::ServiceInvocationResponseSink,
                        >::try_from(s)
                            .transpose()
                            .ok_or(ConversionError::missing_field("response_sink"))??)
                    })
                    .collect::<Result<HashSet<_>, _>>()?;

                let source = restate_types::invocation::Source::try_from(
                    value
                        .source
                        .ok_or(ConversionError::missing_field("source"))?,
                )?;

                let completion_retention_time = std::time::Duration::try_from(
                    value.completion_retention_time.unwrap_or_default(),
                )?;

                let idempotency_key = match value
                    .idempotency_key
                    .ok_or(ConversionError::missing_field("idempotency_key"))?
                {
                    invocation_status::invoked::IdempotencyKey::IdempotencyKeyValue(key) => {
                        Some(ByteString::from(key))
                    }
                    invocation_status::invoked::IdempotencyKey::IdempotencyKeyNone(_) => None,
                };

                Ok(crate::invocation_status_table::InFlightInvocationMetadata {
                    service_id,
                    invocation_target,
                    journal_metadata,
                    deployment_id,
                    method: method_name,
                    response_sinks,
                    timestamps: crate::invocation_status_table::StatusTimestamps::new(
                        MillisSinceEpoch::new(value.creation_time),
                        MillisSinceEpoch::new(value.modification_time),
                    ),
                    source,
                    completion_retention_time,
                    idempotency_key,
                })
            }
        }

        impl From<crate::invocation_status_table::InFlightInvocationMetadata> for Invoked {
            fn from(value: crate::invocation_status_table::InFlightInvocationMetadata) -> Self {
                let crate::invocation_status_table::InFlightInvocationMetadata {
                    service_id,
                    invocation_target,
                    deployment_id,
                    method,
                    response_sinks,
                    journal_metadata,
                    timestamps,
                    source,
                    completion_retention_time,
                    idempotency_key,
                } = value;

                Invoked {
                    invocation_target: Some(invocation_target.into()),
                    service_id: Some(service_id.into()),
                    response_sinks: response_sinks
                        .into_iter()
                        .map(|s| ServiceInvocationResponseSink::from(Some(s)))
                        .collect(),
                    method_name: method.into_bytes(),
                    deployment_id: Some(match deployment_id {
                        None => invocation_status::invoked::DeploymentId::None(()),
                        Some(deployment_id) => invocation_status::invoked::DeploymentId::Value(
                            deployment_id.to_string(),
                        ),
                    }),
                    journal_meta: Some(JournalMeta::from(journal_metadata)),
                    creation_time: timestamps.creation_time().as_u64(),
                    modification_time: timestamps.modification_time().as_u64(),
                    source: Some(Source::from(source)),
                    completion_retention_time: Some(Duration::from(completion_retention_time)),
                    idempotency_key: Some(match idempotency_key {
                        Some(key) => {
                            invocation_status::invoked::IdempotencyKey::IdempotencyKeyValue(
                                key.to_string(),
                            )
                        }
                        _ => invocation_status::invoked::IdempotencyKey::IdempotencyKeyNone(()),
                    }),
                }
            }
        }

        impl TryFrom<Suspended>
            for (
                crate::invocation_status_table::InFlightInvocationMetadata,
                HashSet<restate_types::identifiers::EntryIndex>,
            )
        {
            type Error = ConversionError;

            fn try_from(value: Suspended) -> Result<Self, Self::Error> {
                let invocation_target = restate_types::invocation::InvocationTarget::try_from(
                    value
                        .invocation_target
                        .ok_or(ConversionError::missing_field("invocation_target"))?,
                )?;

                let service_id = value
                    .service_id
                    .ok_or(ConversionError::missing_field("service_id"))?
                    .try_into()?;

                let method_name = value.method_name.try_into().map_err(|e| {
                    ConversionError::InvalidData(anyhow!("Cannot decode method_name string {e}"))
                })?;
                let deployment_id =
                    value.deployment_id.and_then(
                        |one_of_deployment_id| match one_of_deployment_id {
                            invocation_status::suspended::DeploymentId::None(_) => None,
                            invocation_status::suspended::DeploymentId::Value(id) => Some(id),
                        },
                    );

                let journal_metadata = crate::invocation_status_table::JournalMetadata::try_from(
                    value
                        .journal_meta
                        .ok_or(ConversionError::missing_field("journal_meta"))?,
                )?;
                let response_sinks = value
                    .response_sinks
                    .into_iter()
                    .map(|s| {
                        Ok::<_, ConversionError>(Option::<
                            restate_types::invocation::ServiceInvocationResponseSink,
                        >::try_from(s)
                            .transpose()
                            .ok_or(ConversionError::missing_field("response_sink"))??)
                    })
                    .collect::<Result<HashSet<_>, _>>()?;

                let waiting_for_completed_entries =
                    value.waiting_for_completed_entries.into_iter().collect();

                let caller = restate_types::invocation::Source::try_from(
                    value
                        .source
                        .ok_or(ConversionError::missing_field("source"))?,
                )?;

                let completion_retention_time = std::time::Duration::try_from(
                    value.completion_retention_time.unwrap_or_default(),
                )?;

                let idempotency_key = match value
                    .idempotency_key
                    .ok_or(ConversionError::missing_field("idempotency_key"))?
                {
                    invocation_status::suspended::IdempotencyKey::IdempotencyKeyValue(key) => {
                        Some(ByteString::from(key))
                    }
                    invocation_status::suspended::IdempotencyKey::IdempotencyKeyNone(_) => None,
                };

                Ok((
                    crate::invocation_status_table::InFlightInvocationMetadata {
                        service_id,
                        invocation_target,
                        journal_metadata,
                        deployment_id: deployment_id
                            .map(|d| d.parse().expect("valid deployment id")),
                        method: method_name,
                        response_sinks,
                        timestamps: crate::invocation_status_table::StatusTimestamps::new(
                            MillisSinceEpoch::new(value.creation_time),
                            MillisSinceEpoch::new(value.modification_time),
                        ),
                        source: caller,
                        completion_retention_time,
                        idempotency_key,
                    },
                    waiting_for_completed_entries,
                ))
            }
        }

        impl
            From<(
                crate::invocation_status_table::InFlightInvocationMetadata,
                HashSet<restate_types::identifiers::EntryIndex>,
            )> for Suspended
        {
            fn from(
                (metadata, waiting_for_completed_entries): (
                    crate::invocation_status_table::InFlightInvocationMetadata,
                    HashSet<restate_types::identifiers::EntryIndex>,
                ),
            ) -> Self {
                let journal_meta = JournalMeta::from(metadata.journal_metadata);
                let waiting_for_completed_entries =
                    waiting_for_completed_entries.into_iter().collect();

                Suspended {
                    invocation_target: Some(metadata.invocation_target.into()),
                    service_id: Some(metadata.service_id.into()),
                    response_sinks: metadata
                        .response_sinks
                        .into_iter()
                        .map(|s| ServiceInvocationResponseSink::from(Some(s)))
                        .collect(),
                    journal_meta: Some(journal_meta),
                    method_name: metadata.method.into_bytes(),
                    deployment_id: Some(match metadata.deployment_id {
                        None => invocation_status::suspended::DeploymentId::None(()),
                        Some(deployment_id) => invocation_status::suspended::DeploymentId::Value(
                            deployment_id.to_string(),
                        ),
                    }),
                    creation_time: metadata.timestamps.creation_time().as_u64(),
                    modification_time: metadata.timestamps.modification_time().as_u64(),
                    waiting_for_completed_entries,
                    source: Some(Source::from(metadata.source)),
                    completion_retention_time: Some(Duration::from(
                        metadata.completion_retention_time,
                    )),
                    idempotency_key: Some(match metadata.idempotency_key {
                        Some(key) => {
                            invocation_status::suspended::IdempotencyKey::IdempotencyKeyValue(
                                key.to_string(),
                            )
                        }
                        _ => invocation_status::suspended::IdempotencyKey::IdempotencyKeyNone(()),
                    }),
                }
            }
        }

        impl TryFrom<Inboxed> for crate::invocation_status_table::InboxedInvocation {
            type Error = ConversionError;

            fn try_from(value: Inboxed) -> Result<Self, Self::Error> {
                let invocation_target = restate_types::invocation::InvocationTarget::try_from(
                    value
                        .invocation_target
                        .ok_or(ConversionError::missing_field("invocation_target"))?,
                )?;

                let service_id = value
                    .service_id
                    .ok_or(ConversionError::missing_field("service_id"))?
                    .try_into()?;

                let handler_name = value.handler.try_into().map_err(|e| {
                    ConversionError::InvalidData(anyhow!("Cannot decode method_name string {e}"))
                })?;
                let response_sinks = value
                    .response_sinks
                    .into_iter()
                    .map(|s| {
                        Ok::<_, ConversionError>(Option::<
                            restate_types::invocation::ServiceInvocationResponseSink,
                        >::try_from(s)
                            .transpose()
                            .ok_or(ConversionError::missing_field("response_sink"))??)
                    })
                    .collect::<Result<HashSet<_>, _>>()?;

                let source = restate_types::invocation::Source::try_from(
                    value
                        .source
                        .ok_or(ConversionError::missing_field("source"))?,
                )?;

                let span_context =
                    restate_types::invocation::ServiceInvocationSpanContext::try_from(
                        value
                            .span_context
                            .ok_or(ConversionError::missing_field("span_context"))?,
                    )?;
                let headers = value
                    .headers
                    .into_iter()
                    .map(|h| restate_types::invocation::Header::try_from(h))
                    .collect::<Result<Vec<_>, ConversionError>>()?;

                let execution_time = if value.execution_time == 0 {
                    None
                } else {
                    Some(MillisSinceEpoch::new(value.execution_time))
                };

                let idempotency = value
                    .idempotency
                    .map(restate_types::invocation::Idempotency::try_from)
                    .transpose()?;

                Ok(crate::invocation_status_table::InboxedInvocation {
                    inbox_sequence_number: value.inbox_sequence_number,
                    response_sinks,
                    timestamps: crate::invocation_status_table::StatusTimestamps::new(
                        MillisSinceEpoch::new(value.creation_time),
                        MillisSinceEpoch::new(value.modification_time),
                    ),
                    service_id,
                    handler_name,
                    source,
                    span_context,
                    headers,
                    argument: value.argument,
                    execution_time,
                    idempotency,
                    invocation_target,
                })
            }
        }

        impl From<crate::invocation_status_table::InboxedInvocation> for Inboxed {
            fn from(value: crate::invocation_status_table::InboxedInvocation) -> Self {
                let crate::invocation_status_table::InboxedInvocation {
                    invocation_target,
                    inbox_sequence_number,
                    service_id,
                    response_sinks,
                    timestamps,
                    handler_name,
                    argument,
                    source,
                    span_context,
                    headers,
                    execution_time,
                    idempotency,
                } = value;

                let headers = headers.into_iter().map(Into::into).collect();

                Inboxed {
                    invocation_target: Some(invocation_target.into()),
                    inbox_sequence_number,
                    service_id: Some(service_id.into()),
                    handler: handler_name.into_bytes(),
                    response_sinks: response_sinks
                        .into_iter()
                        .map(|s| ServiceInvocationResponseSink::from(Some(s)))
                        .collect(),
                    creation_time: timestamps.creation_time().as_u64(),
                    modification_time: timestamps.modification_time().as_u64(),
                    source: Some(Source::from(source)),
                    span_context: Some(SpanContext::from(span_context)),
                    headers,
                    argument,
                    execution_time: execution_time.map(|m| m.as_u64()).unwrap_or_default(),
                    idempotency: idempotency.map(Into::into),
                }
            }
        }

        impl TryFrom<Completed> for crate::invocation_status_table::CompletedInvocation {
            type Error = ConversionError;

            fn try_from(value: Completed) -> Result<Self, Self::Error> {
                let invocation_target = restate_types::invocation::InvocationTarget::try_from(
                    value
                        .invocation_target
                        .ok_or(ConversionError::missing_field("invocation_target"))?,
                )?;

                let handler_name = value.handler_name.try_into().map_err(|e| {
                    ConversionError::InvalidData(anyhow!("Cannot decode method_name string {e}"))
                })?;

                let idempotency_key = match value
                    .idempotency_key
                    .ok_or(ConversionError::missing_field("idempotency_key"))?
                {
                    invocation_status::completed::IdempotencyKey::IdempotencyKeyValue(key) => {
                        Some(ByteString::from(key))
                    }
                    invocation_status::completed::IdempotencyKey::IdempotencyKeyNone(_) => None,
                };

                Ok(crate::invocation_status_table::CompletedInvocation {
                    invocation_target,
                    service_id: value
                        .service_id
                        .ok_or(ConversionError::missing_field("service_id"))?
                        .try_into()?,
                    handler: handler_name,
                    response_result: value
                        .result
                        .ok_or(ConversionError::missing_field("result"))?
                        .try_into()?,
                    idempotency_key,
                })
            }
        }

        impl From<crate::invocation_status_table::CompletedInvocation> for Completed {
            fn from(value: crate::invocation_status_table::CompletedInvocation) -> Self {
                let crate::invocation_status_table::CompletedInvocation {
                    invocation_target,
                    service_id,
                    handler,
                    idempotency_key,
                    response_result,
                } = value;

                Completed {
                    invocation_target: Some(InvocationTarget::from(invocation_target)),
                    result: Some(ResponseResult::from(response_result)),
                    service_id: Some(service_id.into()),
                    handler_name: handler.into_bytes(),
                    idempotency_key: Some(match idempotency_key {
                        Some(key) => {
                            invocation_status::completed::IdempotencyKey::IdempotencyKeyValue(
                                key.to_string(),
                            )
                        }
                        _ => invocation_status::completed::IdempotencyKey::IdempotencyKeyNone(()),
                    }),
                }
            }
        }

        impl TryFrom<JournalMeta> for crate::invocation_status_table::JournalMetadata {
            type Error = ConversionError;

            fn try_from(value: JournalMeta) -> Result<Self, Self::Error> {
                let length = value.length;
                let span_context =
                    restate_types::invocation::ServiceInvocationSpanContext::try_from(
                        value
                            .span_context
                            .ok_or(ConversionError::missing_field("span_context"))?,
                    )?;
                Ok(crate::invocation_status_table::JournalMetadata {
                    length,
                    span_context,
                })
            }
        }

        impl From<crate::invocation_status_table::JournalMetadata> for JournalMeta {
            fn from(value: crate::invocation_status_table::JournalMetadata) -> Self {
                let crate::invocation_status_table::JournalMetadata {
                    span_context,
                    length,
                } = value;

                JournalMeta {
                    length,
                    span_context: Some(SpanContext::from(span_context)),
                }
            }
        }

        impl TryFrom<Source> for restate_types::invocation::Source {
            type Error = ConversionError;

            fn try_from(value: Source) -> Result<Self, Self::Error> {
                let source = match value
                    .source
                    .ok_or(ConversionError::missing_field("source"))?
                {
                    source::Source::Ingress(_) => restate_types::invocation::Source::Ingress,
                    source::Source::Service(fid) => restate_types::invocation::Source::Service(
                        restate_types::identifiers::FullInvocationId::try_from(fid)?,
                    ),
                    source::Source::Internal(_) => restate_types::invocation::Source::Internal,
                };

                Ok(source)
            }
        }

        impl From<restate_types::invocation::Source> for Source {
            fn from(value: restate_types::invocation::Source) -> Self {
                let source = match value {
                    restate_types::invocation::Source::Ingress => source::Source::Ingress(()),
                    restate_types::invocation::Source::Service(fid) => {
                        source::Source::Service(FullInvocationId::from(fid))
                    }
                    restate_types::invocation::Source::Internal => source::Source::Internal(()),
                };

                Source {
                    source: Some(source),
                }
            }
        }

        impl TryFrom<InboxEntry> for crate::inbox_table::InboxEntry {
            type Error = ConversionError;

            fn try_from(value: InboxEntry) -> Result<Self, Self::Error> {
                Ok(
                    match value.entry.ok_or(ConversionError::missing_field("entry"))? {
                        inbox_entry::Entry::InvocationId(fid) => {
                            crate::inbox_table::InboxEntry::Invocation(
                                restate_types::identifiers::FullInvocationId::try_from(fid)?,
                            )
                        }
                        inbox_entry::Entry::StateMutation(state_mutation) => {
                            crate::inbox_table::InboxEntry::StateMutation(
                                restate_types::state_mut::ExternalStateMutation::try_from(
                                    state_mutation,
                                )?,
                            )
                        }
                    },
                )
            }
        }

        impl From<crate::inbox_table::InboxEntry> for InboxEntry {
            fn from(inbox_entry: crate::inbox_table::InboxEntry) -> Self {
                let inbox_entry = match inbox_entry {
                    crate::inbox_table::InboxEntry::Invocation(fid) => {
                        inbox_entry::Entry::InvocationId(FullInvocationId::from(fid))
                    }
                    crate::inbox_table::InboxEntry::StateMutation(state_mutation) => {
                        inbox_entry::Entry::StateMutation(StateMutation::from(state_mutation))
                    }
                };

                InboxEntry {
                    entry: Some(inbox_entry),
                }
            }
        }

        impl TryFrom<ServiceInvocation> for restate_types::invocation::ServiceInvocation {
            type Error = ConversionError;

            fn try_from(value: ServiceInvocation) -> Result<Self, Self::Error> {
                let ServiceInvocation {
                    invocation_id,
                    invocation_target,
                    id,
                    method_name,
                    response_sink,
                    span_context,
                    argument,
                    source,
                    headers,
                    execution_time,
                    idempotency,
                } = value;

                let invocation_id =
                    restate_types::identifiers::InvocationId::from_slice(&invocation_id)?;

                let invocation_target = restate_types::invocation::InvocationTarget::try_from(
                    invocation_target.ok_or(ConversionError::missing_field("invocation_target"))?,
                )?;

                let id = restate_types::identifiers::FullInvocationId::try_from(
                    id.ok_or(ConversionError::missing_field("id"))?,
                )?;

                let span_context =
                    restate_types::invocation::ServiceInvocationSpanContext::try_from(
                        span_context.ok_or(ConversionError::missing_field("span_context"))?,
                    )?;

                let response_sink =
                    Option::<restate_types::invocation::ServiceInvocationResponseSink>::try_from(
                        response_sink.ok_or(ConversionError::missing_field("response_sink"))?,
                    )?;

                let method_name =
                    ByteString::try_from(method_name).map_err(ConversionError::invalid_data)?;

                let source = restate_types::invocation::Source::try_from(
                    source.ok_or(ConversionError::missing_field("source"))?,
                )?;

                let headers = headers
                    .into_iter()
                    .map(|h| restate_types::invocation::Header::try_from(h))
                    .collect::<Result<Vec<_>, ConversionError>>()?;

                let execution_time = if execution_time == 0 {
                    None
                } else {
                    Some(MillisSinceEpoch::new(execution_time))
                };

                let idempotency = idempotency
                    .map(restate_types::invocation::Idempotency::try_from)
                    .transpose()?;

                Ok(restate_types::invocation::ServiceInvocation {
                    invocation_id,
                    invocation_target,
                    fid: id,
                    method_name,
                    argument,
                    source,
                    response_sink,
                    span_context,
                    headers,
                    execution_time,
                    idempotency,
                })
            }
        }

        impl From<restate_types::invocation::ServiceInvocation> for ServiceInvocation {
            fn from(value: restate_types::invocation::ServiceInvocation) -> Self {
                let invocation_id = Bytes::copy_from_slice(&value.invocation_id.to_bytes());
                let invocation_target = InvocationTarget::from(value.invocation_target);
                let id = FullInvocationId::from(value.fid);
                let span_context = SpanContext::from(value.span_context);
                let response_sink = ServiceInvocationResponseSink::from(value.response_sink);
                let method_name = value.method_name.into_bytes();
                let source = Source::from(value.source);
                let headers = value.headers.into_iter().map(Into::into).collect();

                ServiceInvocation {
                    invocation_id,
                    invocation_target: Some(invocation_target),
                    id: Some(id),
                    span_context: Some(span_context),
                    response_sink: Some(response_sink),
                    method_name,
                    argument: value.argument,
                    source: Some(source),
                    headers,
                    execution_time: value.execution_time.map(|m| m.as_u64()).unwrap_or_default(),
                    idempotency: value.idempotency.map(Into::into),
                }
            }
        }

        impl TryFrom<IdempotentRequestMetadata> for restate_types::invocation::Idempotency {
            type Error = ConversionError;

            fn try_from(value: IdempotentRequestMetadata) -> Result<Self, Self::Error> {
                let retention: std::time::Duration = value
                    .retention
                    .ok_or(ConversionError::missing_field("retention"))?
                    .try_into()?;

                Ok(Self {
                    key: ByteString::from(value.key),
                    retention,
                })
            }
        }

        impl From<restate_types::invocation::Idempotency> for IdempotentRequestMetadata {
            fn from(value: restate_types::invocation::Idempotency) -> Self {
                Self {
                    key: value.key.to_string(),
                    retention: Some(value.retention.into()),
                }
            }
        }

        impl TryFrom<StateMutation> for restate_types::state_mut::ExternalStateMutation {
            type Error = ConversionError;

            fn try_from(state_mutation: StateMutation) -> Result<Self, Self::Error> {
                let service_id = restate_types::identifiers::ServiceId::try_from(
                    state_mutation
                        .service_id
                        .ok_or(ConversionError::missing_field("service_id"))?,
                )?;
                let state = state_mutation
                    .kv_pairs
                    .into_iter()
                    .map(|kv| (kv.key, kv.value))
                    .collect();

                Ok(restate_types::state_mut::ExternalStateMutation {
                    component_id: service_id,
                    version: state_mutation.version,
                    state,
                })
            }
        }

        impl From<restate_types::state_mut::ExternalStateMutation> for StateMutation {
            fn from(state_mutation: restate_types::state_mut::ExternalStateMutation) -> Self {
                let service_id = ServiceId::from(state_mutation.component_id);
                let kv_pairs = state_mutation
                    .state
                    .into_iter()
                    .map(|(key, value)| KvPair { key, value })
                    .collect();

                StateMutation {
                    service_id: Some(service_id),
                    version: state_mutation.version,
                    kv_pairs,
                }
            }
        }

        impl TryFrom<InvocationTarget> for restate_types::invocation::InvocationTarget {
            type Error = ConversionError;

            fn try_from(value: InvocationTarget) -> Result<Self, Self::Error> {
                match invocation_target::Ty::try_from(value.ty) {
                    Ok(invocation_target::Ty::Service) => {
                        Ok(restate_types::invocation::InvocationTarget::Service {
                            name: ByteString::try_from(value.name)
                                .map_err(ConversionError::invalid_data)?,
                            handler: ByteString::try_from(value.handler)
                                .map_err(ConversionError::invalid_data)?,
                        })
                    }
                    Ok(invocation_target::Ty::VirtualObject) => {
                        Ok(restate_types::invocation::InvocationTarget::VirtualObject {
                            name: ByteString::try_from(value.name)
                                .map_err(ConversionError::invalid_data)?,
                            handler: ByteString::try_from(value.handler)
                                .map_err(ConversionError::invalid_data)?,
                            key: ByteString::try_from(value.key)
                                .map_err(ConversionError::invalid_data)?,
                            handler_ty: match invocation_target::HandlerType::try_from(
                                value.handler_ty,
                            ) {
                                Ok(invocation_target::HandlerType::Exclusive) => {
                                    restate_types::invocation::HandlerType::Exclusive
                                }
                                Ok(invocation_target::HandlerType::Shared) => {
                                    restate_types::invocation::HandlerType::Shared
                                }
                                _ => {
                                    return Err(ConversionError::unexpected_enum_variant(
                                        "handler_ty",
                                        value.handler_ty,
                                    ))
                                }
                            },
                        })
                    }
                    _ => Err(ConversionError::unexpected_enum_variant("ty", value.ty)),
                }
            }
        }

        impl From<restate_types::invocation::InvocationTarget> for InvocationTarget {
            fn from(value: restate_types::invocation::InvocationTarget) -> Self {
                match value {
                    restate_types::invocation::InvocationTarget::Service { name, handler } => {
                        InvocationTarget {
                            ty: invocation_target::Ty::Service.into(),
                            name: name.into_bytes(),
                            handler: handler.into_bytes(),
                            ..InvocationTarget::default()
                        }
                    }
                    restate_types::invocation::InvocationTarget::VirtualObject {
                        name,
                        key,
                        handler,
                        handler_ty,
                    } => InvocationTarget {
                        ty: invocation_target::Ty::VirtualObject.into(),
                        name: name.into_bytes(),
                        handler: handler.into_bytes(),
                        key: key.into_bytes(),
                        handler_ty: match handler_ty {
                            restate_types::invocation::HandlerType::Shared => {
                                invocation_target::HandlerType::Shared
                            }
                            restate_types::invocation::HandlerType::Exclusive => {
                                invocation_target::HandlerType::Exclusive
                            }
                        }
                        .into(),
                    },
                }
            }
        }

        impl TryFrom<ServiceId> for restate_types::identifiers::ServiceId {
            type Error = ConversionError;

            fn try_from(service_id: ServiceId) -> Result<Self, Self::Error> {
                Ok(restate_types::identifiers::ServiceId::new(
                    ByteString::try_from(service_id.service_name)
                        .map_err(ConversionError::invalid_data)?,
                    service_id.service_key,
                ))
            }
        }

        impl From<restate_types::identifiers::ServiceId> for ServiceId {
            fn from(service_id: restate_types::identifiers::ServiceId) -> Self {
                ServiceId {
                    service_key: service_id.key,
                    service_name: service_id.service_name.into_bytes(),
                }
            }
        }

        impl TryFrom<FullInvocationId> for restate_types::identifiers::FullInvocationId {
            type Error = ConversionError;

            fn try_from(value: FullInvocationId) -> Result<Self, Self::Error> {
                let FullInvocationId {
                    service_name,
                    service_key,
                    invocation_uuid,
                } = value;

                let service_name =
                    ByteString::try_from(service_name).map_err(ConversionError::invalid_data)?;
                let invocation_uuid = try_bytes_into_invocation_uuid(invocation_uuid)?;

                Ok(restate_types::identifiers::FullInvocationId::new(
                    service_name,
                    service_key,
                    invocation_uuid,
                ))
            }
        }

        impl From<restate_types::identifiers::FullInvocationId> for FullInvocationId {
            fn from(value: restate_types::identifiers::FullInvocationId) -> Self {
                let service_key = value.service_id.key;
                let service_name = value.service_id.service_name.into_bytes();

                FullInvocationId {
                    invocation_uuid: value.invocation_uuid.into(),
                    service_key,
                    service_name,
                }
            }
        }

        fn try_bytes_into_invocation_uuid(
            bytes: Bytes,
        ) -> Result<restate_types::identifiers::InvocationUuid, ConversionError> {
            restate_types::identifiers::InvocationUuid::from_slice(bytes.as_ref())
                .map_err(ConversionError::invalid_data)
        }

        impl TryFrom<SpanContext> for restate_types::invocation::ServiceInvocationSpanContext {
            type Error = ConversionError;

            fn try_from(value: SpanContext) -> Result<Self, Self::Error> {
                let SpanContext {
                    trace_id,
                    span_id,
                    trace_flags,
                    is_remote,
                    trace_state,
                    span_relation,
                } = value;

                let trace_id = try_bytes_into_trace_id(trace_id)?;
                let span_id = opentelemetry::trace::SpanId::from_bytes(span_id.to_be_bytes());
                let trace_flags = opentelemetry::trace::TraceFlags::new(
                    u8::try_from(trace_flags).map_err(ConversionError::invalid_data)?,
                );

                let trace_state =
                    TraceState::from_str(&trace_state).map_err(ConversionError::invalid_data)?;

                let span_relation = span_relation
                    .map(|span_relation| span_relation.try_into())
                    .transpose()
                    .map_err(ConversionError::invalid_data)?;

                Ok(
                    restate_types::invocation::ServiceInvocationSpanContext::new(
                        opentelemetry::trace::SpanContext::new(
                            trace_id,
                            span_id,
                            trace_flags,
                            is_remote,
                            trace_state,
                        ),
                        span_relation,
                    ),
                )
            }
        }

        impl From<restate_types::invocation::ServiceInvocationSpanContext> for SpanContext {
            fn from(value: restate_types::invocation::ServiceInvocationSpanContext) -> Self {
                let span_context = value.span_context();
                let trace_state = span_context.trace_state().header();
                let span_id = u64::from_be_bytes(span_context.span_id().to_bytes());
                let trace_flags = u32::from(span_context.trace_flags().to_u8());
                let trace_id = Bytes::copy_from_slice(&span_context.trace_id().to_bytes());
                let is_remote = span_context.is_remote();
                let span_relation = value
                    .span_cause()
                    .map(|span_relation| SpanRelation::from(span_relation.clone()));

                SpanContext {
                    trace_state,
                    span_id,
                    trace_flags,
                    trace_id,
                    is_remote,
                    span_relation,
                }
            }
        }

        impl TryFrom<SpanRelation> for restate_types::invocation::SpanRelationCause {
            type Error = ConversionError;

            fn try_from(value: SpanRelation) -> Result<Self, Self::Error> {
                match value.kind.ok_or(ConversionError::missing_field("kind"))? {
                    span_relation::Kind::Parent(span_relation::Parent { span_id }) => {
                        let span_id =
                            opentelemetry::trace::SpanId::from_bytes(span_id.to_be_bytes());
                        Ok(Self::Parent(span_id))
                    }
                    span_relation::Kind::Linked(span_relation::Linked { trace_id, span_id }) => {
                        let trace_id = try_bytes_into_trace_id(trace_id)?;
                        let span_id =
                            opentelemetry::trace::SpanId::from_bytes(span_id.to_be_bytes());
                        Ok(Self::Linked(trace_id, span_id))
                    }
                }
            }
        }

        impl From<restate_types::invocation::SpanRelationCause> for SpanRelation {
            fn from(value: restate_types::invocation::SpanRelationCause) -> Self {
                let kind = match value {
                    restate_types::invocation::SpanRelationCause::Parent(span_id) => {
                        let span_id = u64::from_be_bytes(span_id.to_bytes());
                        span_relation::Kind::Parent(span_relation::Parent { span_id })
                    }
                    restate_types::invocation::SpanRelationCause::Linked(trace_id, span_id) => {
                        let span_id = u64::from_be_bytes(span_id.to_bytes());
                        let trace_id = Bytes::copy_from_slice(&trace_id.to_bytes());
                        span_relation::Kind::Linked(span_relation::Linked { trace_id, span_id })
                    }
                };

                Self { kind: Some(kind) }
            }
        }

        fn try_bytes_into_trace_id(
            mut bytes: Bytes,
        ) -> Result<opentelemetry::trace::TraceId, ConversionError> {
            if bytes.len() != 16 {
                return Err(ConversionError::InvalidData(anyhow!(
                    "trace id pb definition needs to contain exactly 16 bytes"
                )));
            }

            let mut bytes_array = [0; 16];
            bytes.copy_to_slice(&mut bytes_array);

            Ok(opentelemetry::trace::TraceId::from_bytes(bytes_array))
        }

        impl TryFrom<ServiceInvocationResponseSink>
            for Option<restate_types::invocation::ServiceInvocationResponseSink>
        {
            type Error = ConversionError;

            fn try_from(value: ServiceInvocationResponseSink) -> Result<Self, Self::Error> {
                let response_sink = match value
                    .response_sink
                    .ok_or(ConversionError::missing_field("response_sink"))?
                {
                    ResponseSink::PartitionProcessor(partition_processor) => {
                        Some(
                            restate_types::invocation::ServiceInvocationResponseSink::PartitionProcessor {
                                caller: restate_types::identifiers::InvocationId::from_slice(&partition_processor.caller)?,
                                entry_index: partition_processor.entry_index,
                            },
                        )
                    }
                    ResponseSink::Ingress(ingress) => {
                        let proto_id = ingress
                            .node_id
                            .ok_or(ConversionError::missing_field("node_id"))?;

                        Some(
                            restate_types::invocation::ServiceInvocationResponseSink::Ingress(
                                GenerationalNodeId::new(proto_id.id, proto_id.generation),
                            ),
                        )
                    }
                    ResponseSink::NewInvocation(new_invocation) => {
                        let target = restate_types::identifiers::FullInvocationId::try_from(
                            new_invocation
                                .target
                                .ok_or(ConversionError::missing_field("target"))?,
                        )?;
                        Some(
                            restate_types::invocation::ServiceInvocationResponseSink::NewInvocation {
                                target,
                                method: new_invocation.method,
                                caller_context: new_invocation.caller_context,
                            },
                        )
                    }
                    ResponseSink::None(_) => None,
                };

                Ok(response_sink)
            }
        }

        impl From<Option<restate_types::invocation::ServiceInvocationResponseSink>>
            for ServiceInvocationResponseSink
        {
            fn from(
                value: Option<restate_types::invocation::ServiceInvocationResponseSink>,
            ) -> Self {
                let response_sink = match value {
                    Some(
                        restate_types::invocation::ServiceInvocationResponseSink::PartitionProcessor {
                            caller,
                            entry_index,
                        },
                    ) => ResponseSink::PartitionProcessor(PartitionProcessor {
                        entry_index,
                        caller: caller.into(),
                    }),
                    Some(restate_types::invocation::ServiceInvocationResponseSink::Ingress(node_id)) => {
                        ResponseSink::Ingress(Ingress {
                            node_id: Some(super::GenerationalNodeId::from(node_id)),
                        })
                    },
                    Some(
                        restate_types::invocation::ServiceInvocationResponseSink::NewInvocation {
                            target, method, caller_context
                        },
                    ) => ResponseSink::NewInvocation(NewInvocation {
                        method,
                        target: Some(FullInvocationId::from(target)),
                        caller_context
                    }),
                    None => ResponseSink::None(Default::default()),
                };

                ServiceInvocationResponseSink {
                    response_sink: Some(response_sink),
                }
            }
        }

        impl TryFrom<Header> for restate_types::invocation::Header {
            type Error = ConversionError;

            fn try_from(value: Header) -> Result<Self, Self::Error> {
                let Header { name, value } = value;

                Ok(restate_types::invocation::Header::new(name, value))
            }
        }

        impl From<restate_types::invocation::Header> for Header {
            fn from(value: restate_types::invocation::Header) -> Self {
                Self {
                    name: value.name.to_string(),
                    value: value.value.to_string(),
                }
            }
        }

        impl From<GenerationalNodeId> for super::GenerationalNodeId {
            fn from(value: GenerationalNodeId) -> Self {
                super::GenerationalNodeId {
                    id: value.raw_id(),
                    generation: value.raw_generation(),
                }
            }
        }

        impl From<super::GenerationalNodeId> for GenerationalNodeId {
            fn from(value: super::GenerationalNodeId) -> Self {
                GenerationalNodeId::new(value.id, value.generation)
            }
        }

        impl TryFrom<JournalEntry> for crate::journal_table::JournalEntry {
            type Error = ConversionError;

            fn try_from(value: JournalEntry) -> Result<Self, Self::Error> {
                let journal_entry = match value
                    .kind
                    .ok_or(ConversionError::missing_field("kind"))?
                {
                    Kind::Entry(journal_entry) => crate::journal_table::JournalEntry::Entry(
                        restate_types::journal::enriched::EnrichedRawEntry::try_from(
                            journal_entry,
                        )?,
                    ),
                    Kind::CompletionResult(completion_result) => {
                        crate::journal_table::JournalEntry::Completion(
                            restate_types::journal::CompletionResult::try_from(completion_result)?,
                        )
                    }
                };

                Ok(journal_entry)
            }
        }

        impl From<crate::journal_table::JournalEntry> for JournalEntry {
            fn from(value: crate::journal_table::JournalEntry) -> Self {
                match value {
                    crate::journal_table::JournalEntry::Entry(entry) => JournalEntry::from(entry),
                    crate::journal_table::JournalEntry::Completion(completion) => {
                        JournalEntry::from(completion)
                    }
                }
            }
        }

        impl From<restate_types::journal::enriched::EnrichedRawEntry> for JournalEntry {
            fn from(value: restate_types::journal::enriched::EnrichedRawEntry) -> Self {
                let entry = Entry::from(value);

                JournalEntry {
                    kind: Some(Kind::Entry(entry)),
                }
            }
        }

        impl From<restate_types::journal::CompletionResult> for JournalEntry {
            fn from(value: restate_types::journal::CompletionResult) -> Self {
                let completion_result = CompletionResult::from(value);

                JournalEntry {
                    kind: Some(Kind::CompletionResult(completion_result)),
                }
            }
        }

        impl TryFrom<Entry> for restate_types::journal::enriched::EnrichedRawEntry {
            type Error = ConversionError;

            fn try_from(value: Entry) -> Result<Self, Self::Error> {
                let Entry { header, raw_entry } = value;

                let header = restate_types::journal::enriched::EnrichedEntryHeader::try_from(
                    header.ok_or(ConversionError::missing_field("header"))?,
                )?;

                Ok(restate_types::journal::enriched::EnrichedRawEntry::new(
                    header, raw_entry,
                ))
            }
        }

        impl From<restate_types::journal::enriched::EnrichedRawEntry> for Entry {
            fn from(value: restate_types::journal::enriched::EnrichedRawEntry) -> Self {
                let (header, entry) = value.into_inner();
                Entry {
                    header: Some(EnrichedEntryHeader::from(header)),
                    raw_entry: entry,
                }
            }
        }

        impl TryFrom<CompletionResult> for restate_types::journal::CompletionResult {
            type Error = ConversionError;

            fn try_from(value: CompletionResult) -> Result<Self, Self::Error> {
                let result = match value
                    .result
                    .ok_or(ConversionError::missing_field("result"))?
                {
                    completion_result::Result::Empty(_) => {
                        restate_types::journal::CompletionResult::Empty
                    }
                    completion_result::Result::Success(success) => {
                        restate_types::journal::CompletionResult::Success(success.value)
                    }
                    completion_result::Result::Failure(failure) => {
                        let failure_message = ByteString::try_from(failure.message)
                            .map_err(ConversionError::invalid_data);

                        restate_types::journal::CompletionResult::Failure(
                            failure.error_code.into(),
                            failure_message?,
                        )
                    }
                };

                Ok(result)
            }
        }

        impl From<restate_types::journal::CompletionResult> for CompletionResult {
            fn from(value: restate_types::journal::CompletionResult) -> Self {
                let result = match value {
                    restate_types::journal::CompletionResult::Empty => {
                        completion_result::Result::Empty(Empty {})
                    }
                    restate_types::journal::CompletionResult::Success(value) => {
                        completion_result::Result::Success(Success { value })
                    }
                    restate_types::journal::CompletionResult::Failure(error_code, message) => {
                        completion_result::Result::Failure(Failure {
                            error_code: error_code.into(),
                            message: message.into_bytes(),
                        })
                    }
                };

                CompletionResult {
                    result: Some(result),
                }
            }
        }

        impl TryFrom<EnrichedEntryHeader> for restate_types::journal::enriched::EnrichedEntryHeader {
            type Error = ConversionError;

            fn try_from(value: EnrichedEntryHeader) -> Result<Self, Self::Error> {
                // By definition of requires_ack, if it reached the journal storage then
                // either there is one in-flight stream that already got notified of this entry ack,
                // or there are no in-flight streams and the entry won't need any ack because it's in the replayed journal.

                let enriched_header = match value
                    .kind
                    .ok_or(ConversionError::missing_field("kind"))?
                {
                    enriched_entry_header::Kind::Input(_) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::Input {}
                    }
                    enriched_entry_header::Kind::Output(_) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::Output {}
                    }
                    enriched_entry_header::Kind::GetState(get_state) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::GetState {
                            is_completed: get_state.is_completed,
                        }
                    }
                    enriched_entry_header::Kind::SetState(_) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::SetState {}
                    }
                    enriched_entry_header::Kind::ClearState(_) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::ClearState {}
                    }
                    enriched_entry_header::Kind::ClearAllState(_) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::ClearAllState {}
                    }
                    enriched_entry_header::Kind::GetStateKeys(get_state_keys) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::GetStateKeys {
                            is_completed: get_state_keys.is_completed,
                        }
                    }
                    enriched_entry_header::Kind::Sleep(sleep) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::Sleep {
                            is_completed: sleep.is_completed,
                        }
                    }
                    enriched_entry_header::Kind::Invoke(invoke) => {
                        let enrichment_result = Option::<
                            restate_types::journal::enriched::InvokeEnrichmentResult,
                        >::try_from(
                            invoke
                                .resolution_result
                                .ok_or(ConversionError::missing_field("resolution_result"))?,
                        )?;

                        restate_types::journal::enriched::EnrichedEntryHeader::Invoke {
                            is_completed: invoke.is_completed,
                            enrichment_result,
                        }
                    }
                    enriched_entry_header::Kind::BackgroundCall(background_call) => {
                        let enrichment_result =
                            restate_types::journal::enriched::InvokeEnrichmentResult::try_from(
                                background_call
                                    .resolution_result
                                    .ok_or(ConversionError::missing_field("resolution_result"))?,
                            )?;

                        restate_types::journal::enriched::EnrichedEntryHeader::BackgroundInvoke {
                            enrichment_result,
                        }
                    }
                    enriched_entry_header::Kind::Awakeable(awakeable) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::Awakeable {
                            is_completed: awakeable.is_completed,
                        }
                    }
                    enriched_entry_header::Kind::CompleteAwakeable(CompleteAwakeable {
                        invocation_id,
                        entry_index,
                    }) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::CompleteAwakeable {
                            enrichment_result: AwakeableEnrichmentResult {
                                invocation_id:
                                    restate_types::identifiers::InvocationId::from_slice(
                                        &invocation_id,
                                    )
                                    .map_err(ConversionError::invalid_data)?,
                                entry_index,
                            },
                        }
                    }
                    enriched_entry_header::Kind::SideEffect(_) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::SideEffect {}
                    }
                    enriched_entry_header::Kind::Custom(custom) => {
                        restate_types::journal::enriched::EnrichedEntryHeader::Custom {
                            code: u16::try_from(custom.code)
                                .map_err(ConversionError::invalid_data)?,
                        }
                    }
                };

                Ok(enriched_header)
            }
        }

        impl From<restate_types::journal::enriched::EnrichedEntryHeader> for EnrichedEntryHeader {
            fn from(value: restate_types::journal::enriched::EnrichedEntryHeader) -> Self {
                // No need to write down the requires_ack field for any of the entries because
                // when reading an entry from storage, we never need to send the ack back for it.

                let kind = match value {
                    restate_types::journal::enriched::EnrichedEntryHeader::Input { .. } => {
                        enriched_entry_header::Kind::Input(Input {})
                    }
                    restate_types::journal::enriched::EnrichedEntryHeader::Output { .. } => {
                        enriched_entry_header::Kind::Output(Output {})
                    }
                    restate_types::journal::enriched::EnrichedEntryHeader::GetState {
                        is_completed,
                        ..
                    } => enriched_entry_header::Kind::GetState(GetState { is_completed }),
                    restate_types::journal::enriched::EnrichedEntryHeader::SetState { .. } => {
                        enriched_entry_header::Kind::SetState(SetState {})
                    }
                    restate_types::journal::enriched::EnrichedEntryHeader::ClearState {
                        ..
                    } => enriched_entry_header::Kind::ClearState(ClearState {}),
                    restate_types::journal::enriched::EnrichedEntryHeader::GetStateKeys {
                        is_completed,
                        ..
                    } => enriched_entry_header::Kind::GetStateKeys(GetStateKeys { is_completed }),
                    restate_types::journal::enriched::EnrichedEntryHeader::ClearAllState {
                        ..
                    } => enriched_entry_header::Kind::ClearAllState(ClearAllState {}),
                    restate_types::journal::enriched::EnrichedEntryHeader::Sleep {
                        is_completed,
                        ..
                    } => enriched_entry_header::Kind::Sleep(Sleep { is_completed }),
                    restate_types::journal::enriched::EnrichedEntryHeader::Invoke {
                        is_completed,
                        enrichment_result,
                        ..
                    } => enriched_entry_header::Kind::Invoke(Invoke {
                        is_completed,
                        resolution_result: Some(InvocationResolutionResult::from(
                            enrichment_result,
                        )),
                    }),
                    restate_types::journal::enriched::EnrichedEntryHeader::BackgroundInvoke {
                        enrichment_result,
                        ..
                    } => enriched_entry_header::Kind::BackgroundCall(BackgroundCall {
                        resolution_result: Some(BackgroundCallResolutionResult::from(
                            enrichment_result,
                        )),
                    }),
                    restate_types::journal::enriched::EnrichedEntryHeader::Awakeable {
                        is_completed,
                        ..
                    } => enriched_entry_header::Kind::Awakeable(Awakeable { is_completed }),
                    restate_types::journal::enriched::EnrichedEntryHeader::CompleteAwakeable {
                        enrichment_result,
                        ..
                    } => enriched_entry_header::Kind::CompleteAwakeable(CompleteAwakeable {
                        invocation_id: Bytes::copy_from_slice(
                            &enrichment_result.invocation_id.to_bytes(),
                        ),
                        entry_index: enrichment_result.entry_index,
                    }),
                    restate_types::journal::enriched::EnrichedEntryHeader::SideEffect {
                        ..
                    } => enriched_entry_header::Kind::SideEffect(SideEffect {}),
                    restate_types::journal::enriched::EnrichedEntryHeader::Custom {
                        code, ..
                    } => enriched_entry_header::Kind::Custom(Custom {
                        code: u32::from(code),
                    }),
                };

                EnrichedEntryHeader { kind: Some(kind) }
            }
        }

        impl TryFrom<InvocationResolutionResult>
            for Option<restate_types::journal::enriched::InvokeEnrichmentResult>
        {
            type Error = ConversionError;

            fn try_from(value: InvocationResolutionResult) -> Result<Self, Self::Error> {
                let result = match value
                    .result
                    .ok_or(ConversionError::missing_field("result"))?
                {
                    invocation_resolution_result::Result::None(_) => None,
                    invocation_resolution_result::Result::Success(success) => {
                        let invocation_id = restate_types::identifiers::InvocationId::from_slice(
                            &success.invocation_id,
                        )?;

                        let invocation_target =
                            restate_types::invocation::InvocationTarget::try_from(
                                success
                                    .invocation_target
                                    .ok_or(ConversionError::missing_field("invocation_target"))?,
                            )?;

                        let span_context =
                            restate_types::invocation::ServiceInvocationSpanContext::try_from(
                                success
                                    .span_context
                                    .ok_or(ConversionError::missing_field("span_context"))?,
                            )?;
                        let service_key = success.service_key;

                        Some(restate_types::journal::enriched::InvokeEnrichmentResult {
                            invocation_id,
                            invocation_target,
                            span_context,
                            service_key,
                        })
                    }
                };

                Ok(result)
            }
        }

        impl From<Option<restate_types::journal::enriched::InvokeEnrichmentResult>>
            for InvocationResolutionResult
        {
            fn from(
                value: Option<restate_types::journal::enriched::InvokeEnrichmentResult>,
            ) -> Self {
                let result = match value {
                    None => invocation_resolution_result::Result::None(Default::default()),
                    Some(resolution_result) => match resolution_result {
                        restate_types::journal::enriched::InvokeEnrichmentResult {
                            invocation_id,
                            invocation_target,
                            service_key,
                            span_context,
                        } => invocation_resolution_result::Result::Success(
                            invocation_resolution_result::Success {
                                invocation_id: invocation_id.into(),
                                invocation_target: Some(invocation_target.into()),
                                service_key,
                                span_context: Some(SpanContext::from(span_context)),
                            },
                        ),
                    },
                };

                InvocationResolutionResult {
                    result: Some(result),
                }
            }
        }

        impl TryFrom<BackgroundCallResolutionResult>
            for restate_types::journal::enriched::InvokeEnrichmentResult
        {
            type Error = ConversionError;

            fn try_from(value: BackgroundCallResolutionResult) -> Result<Self, Self::Error> {
                let invocation_id =
                    restate_types::identifiers::InvocationId::from_slice(&value.invocation_id)?;

                let invocation_target = restate_types::invocation::InvocationTarget::try_from(
                    value
                        .invocation_target
                        .ok_or(ConversionError::missing_field("invocation_target"))?,
                )?;
                let span_context =
                    restate_types::invocation::ServiceInvocationSpanContext::try_from(
                        value
                            .span_context
                            .ok_or(ConversionError::missing_field("span_context"))?,
                    )?;
                let service_key = value.service_key;

                Ok(restate_types::journal::enriched::InvokeEnrichmentResult {
                    invocation_id,
                    span_context,
                    service_key,
                    invocation_target,
                })
            }
        }

        impl From<restate_types::journal::enriched::InvokeEnrichmentResult>
            for BackgroundCallResolutionResult
        {
            fn from(value: restate_types::journal::enriched::InvokeEnrichmentResult) -> Self {
                BackgroundCallResolutionResult {
                    invocation_id: value.invocation_id.into(),
                    invocation_target: Some(value.invocation_target.into()),
                    service_key: value.service_key,
                    span_context: Some(SpanContext::from(value.span_context)),
                }
            }
        }

        impl TryFrom<OutboxMessage> for crate::outbox_table::OutboxMessage {
            type Error = ConversionError;

            fn try_from(value: OutboxMessage) -> Result<Self, Self::Error> {
                let result = match value
                    .outbox_message
                    .ok_or(ConversionError::missing_field("outbox_message"))?
                {
                    outbox_message::OutboxMessage::ServiceInvocationCase(service_invocation) => {
                        crate::outbox_table::OutboxMessage::ServiceInvocation(
                            restate_types::invocation::ServiceInvocation::try_from(
                                service_invocation
                                    .service_invocation
                                    .ok_or(ConversionError::missing_field("service_invocation"))?,
                            )?,
                        )
                    }
                    outbox_message::OutboxMessage::ServiceInvocationResponse(
                        invocation_response,
                    ) => crate::outbox_table::OutboxMessage::ServiceResponse(
                        restate_types::invocation::InvocationResponse {
                            entry_index: invocation_response.entry_index,
                            id: restate_types::identifiers::InvocationId::from_slice(
                                &invocation_response.invocation_id,
                            )?,
                            result: restate_types::invocation::ResponseResult::try_from(
                                invocation_response
                                    .response_result
                                    .ok_or(ConversionError::missing_field("response_result"))?,
                            )?,
                        },
                    ),
                    outbox_message::OutboxMessage::Kill(outbox_kill) => {
                        crate::outbox_table::OutboxMessage::InvocationTermination(
                            InvocationTermination::kill(
                                restate_types::identifiers::InvocationId::from_slice(
                                    &outbox_kill.invocation_id,
                                )?,
                            ),
                        )
                    }
                    outbox_message::OutboxMessage::Cancel(outbox_cancel) => {
                        crate::outbox_table::OutboxMessage::InvocationTermination(
                            InvocationTermination::cancel(
                                restate_types::identifiers::InvocationId::from_slice(
                                    &outbox_cancel.invocation_id,
                                )?,
                            ),
                        )
                    }
                };

                Ok(result)
            }
        }

        impl From<crate::outbox_table::OutboxMessage> for OutboxMessage {
            fn from(value: crate::outbox_table::OutboxMessage) -> Self {
                let outbox_message = match value {
                    crate::outbox_table::OutboxMessage::ServiceInvocation(service_invocation) => {
                        outbox_message::OutboxMessage::ServiceInvocationCase(
                            OutboxServiceInvocation {
                                service_invocation: Some(ServiceInvocation::from(
                                    service_invocation,
                                )),
                            },
                        )
                    }
                    crate::outbox_table::OutboxMessage::ServiceResponse(invocation_response) => {
                        outbox_message::OutboxMessage::ServiceInvocationResponse(
                            OutboxServiceInvocationResponse {
                                entry_index: invocation_response.entry_index,
                                invocation_id: invocation_response.id.into(),
                                response_result: Some(ResponseResult::from(
                                    invocation_response.result,
                                )),
                            },
                        )
                    }
                    crate::outbox_table::OutboxMessage::InvocationTermination(
                        invocation_termination,
                    ) => match invocation_termination.flavor {
                        TerminationFlavor::Kill => {
                            outbox_message::OutboxMessage::Kill(OutboxKill {
                                invocation_id: invocation_termination.invocation_id.into(),
                            })
                        }
                        TerminationFlavor::Cancel => {
                            outbox_message::OutboxMessage::Cancel(OutboxCancel {
                                invocation_id: invocation_termination.invocation_id.into(),
                            })
                        }
                    },
                };

                OutboxMessage {
                    outbox_message: Some(outbox_message),
                }
            }
        }

        impl TryFrom<ResponseResult> for restate_types::invocation::ResponseResult {
            type Error = ConversionError;

            fn try_from(value: ResponseResult) -> Result<Self, Self::Error> {
                let result = match value
                    .response_result
                    .ok_or(ConversionError::missing_field("response_result"))?
                {
                    response_result::ResponseResult::ResponseSuccess(success) => {
                        restate_types::invocation::ResponseResult::Success(success.value)
                    }
                    response_result::ResponseResult::ResponseFailure(failure) => {
                        restate_types::invocation::ResponseResult::Failure(InvocationError::new(
                            failure.failure_code,
                            ByteString::try_from(failure.failure_message)
                                .map_err(ConversionError::invalid_data)?,
                        ))
                    }
                };

                Ok(result)
            }
        }

        impl From<restate_types::invocation::ResponseResult> for ResponseResult {
            fn from(value: restate_types::invocation::ResponseResult) -> Self {
                let response_result = match value {
                    restate_types::invocation::ResponseResult::Success(value) => {
                        response_result::ResponseResult::ResponseSuccess(
                            response_result::ResponseSuccess { value },
                        )
                    }
                    restate_types::invocation::ResponseResult::Failure(err) => {
                        response_result::ResponseResult::ResponseFailure(
                            response_result::ResponseFailure {
                                failure_code: err.code().into(),
                                failure_message: Bytes::copy_from_slice(err.message().as_ref()),
                            },
                        )
                    }
                };

                ResponseResult {
                    response_result: Some(response_result),
                }
            }
        }

        impl TryFrom<Timer> for crate::timer_table::Timer {
            type Error = ConversionError;

            fn try_from(value: Timer) -> Result<Self, Self::Error> {
                Ok(
                    match value.value.ok_or(ConversionError::missing_field("value"))? {
                        timer::Value::CompleteSleepEntry(cse) => {
                            crate::timer_table::Timer::CompleteSleepEntry(cse.partition_key)
                        }
                        timer::Value::Invoke(si) => crate::timer_table::Timer::Invoke(
                            restate_types::invocation::ServiceInvocation::try_from(si)?,
                        ),
                        timer::Value::CleanInvocationStatus(clean_invocation_status) => {
                            crate::timer_table::Timer::CleanInvocationStatus(
                                restate_types::identifiers::InvocationId::from_slice(
                                    &clean_invocation_status.invocation_id,
                                )?,
                            )
                        }
                    },
                )
            }
        }

        impl From<crate::timer_table::Timer> for Timer {
            fn from(value: crate::timer_table::Timer) -> Self {
                Timer {
                    value: Some(match value {
                        crate::timer_table::Timer::CompleteSleepEntry(partition_key) => {
                            timer::Value::CompleteSleepEntry(timer::CompleteSleepEntry {
                                partition_key,
                            })
                        }

                        crate::timer_table::Timer::Invoke(si) => {
                            timer::Value::Invoke(ServiceInvocation::from(si))
                        }
                        crate::timer_table::Timer::CleanInvocationStatus(invocation_id) => {
                            timer::Value::CleanInvocationStatus(timer::CleanInvocationStatus {
                                invocation_id: Bytes::copy_from_slice(&invocation_id.to_bytes()),
                            })
                        }
                    }),
                }
            }
        }

        impl From<crate::deduplication_table::DedupSequenceNumber> for DedupSequenceNumber {
            fn from(value: crate::deduplication_table::DedupSequenceNumber) -> Self {
                match value {
                    crate::deduplication_table::DedupSequenceNumber::Sn(sn) => {
                        DedupSequenceNumber {
                            variant: Some(Variant::SequenceNumber(sn)),
                        }
                    }
                    crate::deduplication_table::DedupSequenceNumber::Esn(esn) => {
                        DedupSequenceNumber {
                            variant: Some(Variant::EpochSequenceNumber(EpochSequenceNumber::from(
                                esn,
                            ))),
                        }
                    }
                }
            }
        }

        impl TryFrom<DedupSequenceNumber> for crate::deduplication_table::DedupSequenceNumber {
            type Error = ConversionError;

            fn try_from(value: DedupSequenceNumber) -> Result<Self, Self::Error> {
                Ok(
                    match value
                        .variant
                        .ok_or(ConversionError::missing_field("variant"))?
                    {
                        Variant::SequenceNumber(sn) => {
                            crate::deduplication_table::DedupSequenceNumber::Sn(sn)
                        }
                        Variant::EpochSequenceNumber(esn) => {
                            crate::deduplication_table::DedupSequenceNumber::Esn(
                                crate::deduplication_table::EpochSequenceNumber::try_from(esn)?,
                            )
                        }
                    },
                )
            }
        }

        impl From<crate::deduplication_table::EpochSequenceNumber> for EpochSequenceNumber {
            fn from(value: crate::deduplication_table::EpochSequenceNumber) -> Self {
                EpochSequenceNumber {
                    leader_epoch: value.leader_epoch.into(),
                    sequence_number: value.sequence_number,
                }
            }
        }

        impl TryFrom<EpochSequenceNumber> for crate::deduplication_table::EpochSequenceNumber {
            type Error = ConversionError;

            fn try_from(value: EpochSequenceNumber) -> Result<Self, Self::Error> {
                Ok(crate::deduplication_table::EpochSequenceNumber {
                    leader_epoch: value.leader_epoch.into(),
                    sequence_number: value.sequence_number,
                })
            }
        }

        impl From<std::time::Duration> for Duration {
            fn from(value: std::time::Duration) -> Self {
                Duration {
                    secs: value.as_secs(),
                    nanos: value.subsec_nanos(),
                }
            }
        }

        impl TryFrom<Duration> for std::time::Duration {
            type Error = ConversionError;

            fn try_from(value: Duration) -> Result<Self, Self::Error> {
                Ok(std::time::Duration::new(value.secs, value.nanos))
            }
        }

        impl From<crate::idempotency_table::IdempotencyMetadata> for IdempotencyMetadata {
            fn from(value: crate::idempotency_table::IdempotencyMetadata) -> Self {
                IdempotencyMetadata {
                    invocation_id: Bytes::copy_from_slice(&value.invocation_id.to_bytes()),
                }
            }
        }

        impl TryFrom<IdempotencyMetadata> for crate::idempotency_table::IdempotencyMetadata {
            type Error = ConversionError;

            fn try_from(value: IdempotencyMetadata) -> Result<Self, Self::Error> {
                Ok(crate::idempotency_table::IdempotencyMetadata {
                    invocation_id: restate_types::identifiers::InvocationId::from_slice(
                        &value.invocation_id,
                    )
                    .map_err(|e| ConversionError::invalid_data(e))?,
                })
            }
        }
    }
}