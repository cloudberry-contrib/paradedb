// Copyright (c) 2023-2025 ParadeDB, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::error::Error;

use pgrx::{default, pg_extern, Json, JsonB, PgRelation, pg_sys};
use pgrx::cdb::dispatch::{dispatch_if_coordinator, DispatchFlags};

use crate::aggregate::execute_aggregate;
use crate::aggregate::{mvcc_collector::MVCCFilterCollector, vischeck::TSVisibilityChecker};
use crate::index::mvcc::MvccSatisfies;
use crate::index::reader::index::SearchIndexReader;
use crate::postgres::rel::PgSearchRelation;
use crate::query::SearchQueryInput;
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use tantivy::aggregation::{AggregationLimitsGuard, DistributedAggregationCollector};
use tantivy::collector::Collector;

/// Internal function to get local aggregation results on a segment
#[pg_extern]
pub fn get_local_aggregate(
    index: pg_sys::Oid,
    query_json: String,
    agg_json: String,
    solve_mvcc: bool,
    memory_limit: i64,
    bucket_limit: i64,
) -> Result<JsonB, Box<dyn Error>> {
    // This function should only be called on segments, not coordinator
    if crate::gucs::is_gp_coordinator() {
        return Ok(JsonB(serde_json::Value::Null));
    }

    let indexrel = PgSearchRelation::with_lock(index, pg_sys::AccessShareLock as _);
    let query = serde_json::from_str::<SearchQueryInput>(&query_json)?;
    let agg_req = serde_json::from_value::<Aggregations>(
        serde_json::from_str(&agg_json)?
    )?;
    
    let reader = SearchIndexReader::open(&indexrel, query, false, MvccSatisfies::Snapshot)?;
    
    let base_collector = DistributedAggregationCollector::from_aggs(
        agg_req,
        AggregationLimitsGuard::new(
            Some(memory_limit as u64),
            Some(bucket_limit as u32)
        )
    );
    
    // Execute collection and get intermediate results
    let intermediate_results = if solve_mvcc {
        let heaprel = indexrel
            .heap_relation()
            .expect("index should belong to a heap relation");
        let mvcc_collector = MVCCFilterCollector::new(
            base_collector,
            TSVisibilityChecker::with_rel_and_snap(heaprel.as_ptr(), unsafe {
                pg_sys::GetActiveSnapshot()
            })
        );
        reader.collect(mvcc_collector)
    } else {
        reader.collect(base_collector)
    };
    
    // Return intermediate aggregation results as JsonB
    Ok(JsonB(serde_json::to_value(intermediate_results)?))
}

#[pg_extern]
pub fn aggregate(
    index: PgRelation,
    query: SearchQueryInput,
    agg: Json,
    solve_mvcc: default!(bool, true),
    memory_limit: default!(i64, 500000000),
    bucket_limit: default!(i64, 65000),
) -> Result<JsonB, Box<dyn Error>> {
    let index_oid = index.oid();
    
    // Try dispatch if on coordinator
    if let Some(dispatch_result) = dispatch_if_coordinator(
        &format!(
            "SELECT paradedb.get_local_aggregate({}, '{}', '{}', {}, {}, {})",
            index_oid.to_u32(),
            serde_json::to_string(&query)?.replace("'", "''"),
            serde_json::to_string(&agg.0)?.replace("'", "''"),
            solve_mvcc,
            memory_limit,
            bucket_limit
        ),
        DispatchFlags::WITH_SNAPSHOT,
    ) {
        match dispatch_result {
            Ok(result_set) => {
                let mut intermediate_results = Vec::new();
                
                // Collect intermediate results from all segments
                for result in result_set.iter_results() {
                    if let Ok(pg_result) = result {
                        let ntuples = unsafe { pg_sys::PQntuples(pg_result) };
                        if ntuples > 0 {
                            if let Ok(Some(result_str)) = unsafe { 
                                pgrx::cdb::dispatch::CdbPgResults::get_field_value(pg_result, 0, 0) 
                            } {
                                if let Ok(intermediate_value) = serde_json::from_str::<serde_json::Value>(&result_str) {
                                    if !intermediate_value.is_null() {
                                        // Parse back to IntermediateAggregationResults
                                        if let Ok(intermediate) = serde_json::from_value::<IntermediateAggregationResults>(intermediate_value) {
                                            intermediate_results.push(Ok(intermediate));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Merge results using Tantivy's merge_fruits
                if !intermediate_results.is_empty() {
                    let agg_req = serde_json::from_value::<Aggregations>(agg.0.clone())?;
                    let collector = DistributedAggregationCollector::from_aggs(
                        agg_req.clone(),
                        AggregationLimitsGuard::new(
                            Some(memory_limit as u64), 
                            Some(bucket_limit as u32)
                        ),
                    );
                    
                    // Use merge_fruits to combine intermediate results
                    let merged = collector.merge_fruits(intermediate_results)?
                        .into_final_result(
                            agg_req,
                            AggregationLimitsGuard::new(
                                Some(memory_limit as u64),
                                Some(bucket_limit as u32)
                            ),
                        )?;
                    
                    return Ok(JsonB(serde_json::to_value(merged)?));
                }
                
                return Ok(JsonB(serde_json::Value::Null));
            }
            Err(_) => {
                // Fallback to empty result if dispatch fails
                return Ok(JsonB(serde_json::Value::Null));
            }
        }
    }
    
    // Execute locally (on segment or non-distributed environment)
    let relation = unsafe { PgSearchRelation::from_pg(index.as_ptr()) };
    Ok(JsonB(execute_aggregate(
        &relation,
        query,
        agg.0,
        solve_mvcc,
        memory_limit.try_into()?,
        bucket_limit.try_into()?,
    )?))
}
