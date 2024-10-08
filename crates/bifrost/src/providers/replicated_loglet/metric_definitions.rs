// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

/// Optional to have but adds description/help message to the metrics emitted to
/// the metrics' sink.
use metrics::{describe_counter, Unit};

pub(crate) const BIFROST_REPLICATED_APPEND: &str = "restate.bifrost.replicatedloglet.appends.total";
pub(crate) const BIFROST_REPLICATED_READ_CACHE_HIT: &str =
    "restate.bifrost.replicatedloglet.read_record_cache_hit.total";
pub(crate) const BIFROST_REPLICATED_READ_CACHE_FILTERED: &str =
    "restate.bifrost.replicatedloglet.read_record_cache_filtered.total";
pub(crate) const BIFROST_REPLICATED_READ_TOTAL: &str =
    "restate.bifrost.replicatedloglet.read_record.total";

pub(crate) fn describe_metrics() {
    describe_counter!(
        BIFROST_REPLICATED_APPEND,
        Unit::Count,
        "Number of append requests to bifrost's replicated loglet"
    );

    describe_counter!(
        BIFROST_REPLICATED_READ_CACHE_HIT,
        Unit::Count,
        "Number of records read from RecordCache"
    );

    describe_counter!(
        BIFROST_REPLICATED_READ_CACHE_FILTERED,
        Unit::Count,
        "Number of records filtered out while reading from RecordCache"
    );

    describe_counter!(
        BIFROST_REPLICATED_READ_TOTAL,
        Unit::Count,
        "Number of records read"
    );
}
