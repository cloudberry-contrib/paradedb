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

use crate::postgres::rel::PgSearchRelation;
use crate::postgres::utils;
use pgrx::pg_sys;

/// Helper to manage the information necessary to validate that a "ctid" is currently visible to
/// a snapshot
pub struct VisibilityChecker {
    scan: *mut pg_sys::IndexFetchTableData,
    snapshot: pg_sys::Snapshot,
    tid: pg_sys::ItemPointerData,

    // we hold onto this b/c `scan` points to the relation this does
    _heaprel: PgSearchRelation,
}

impl Drop for VisibilityChecker {
    fn drop(&mut self) {
        unsafe {
            if !crate::postgres::utils::IsTransactionState() {
                // we are not in a transaction, so we can't do things like release buffers and close relations
                return;
            }

            pg_sys::table_index_fetch_end(self.scan);
        }
    }
}

impl VisibilityChecker {
    /// Construct a new [`VisibilityChecker`] that can validate ctid visibility against the specified
    /// `relation` and `snapshot`
    pub fn with_rel_and_snap(heaprel: &PgSearchRelation, snapshot: pg_sys::Snapshot) -> Self {
        unsafe {
            Self {
                scan: pg_sys::table_index_fetch_begin(heaprel.as_ptr()),
                snapshot,
                tid: pg_sys::ItemPointerData::default(),
                _heaprel: Clone::clone(heaprel),
            }
        }
    }

    /// If the specified `ctid` is visible in the heap, run the provided closure and return its
    /// result as `Some(T)`.  If it's not visible, return `None` without running the provided
    /// closure
    pub fn exec_if_visible<T, F: FnMut(pg_sys::Relation) -> T>(
        &mut self,
        ctid: u64,
        slot: *mut pg_sys::TupleTableSlot,
        mut func: F,
    ) -> Option<T> {
        unsafe {
            utils::u64_to_item_pointer(ctid, &mut self.tid);

            let mut call_again = false;
            let mut all_dead = false;
            let found = pg_sys::table_index_fetch_tuple(
                self.scan,
                &mut self.tid,
                self.snapshot,
                slot,
                &mut call_again,
                &mut all_dead,
            );

            if found {
                Some(func((*self.scan).rel))
            } else {
                None
            }
        }
    }
}

/// A wrapper for an owned scan and slot for repeated use with `table_index_fetch_tuple`.
///
/// Similar to [`VisibilityChecker`], but uses an owned slot in the shape of the table, rather
/// than borrowing a slot in the shape of the custom scan.
#[derive(Debug)]
pub struct HeapFetchState {
    pub scan: *mut pg_sys::IndexFetchTableData,
    pub slot: *mut pg_sys::TupleTableSlot,
    /// Snapshot captured at creation time.
    ///
    /// AO tables assert that the same snapshot pointer is used across all calls
    /// on a reused scan (`aoscan->aofetch->snapshot == snapshot` in
    /// `appendonlyam_handler.c`). Capturing once and reusing avoids the assertion
    /// failure that would occur if `GetActiveSnapshot()` returns a different
    /// pointer between calls (e.g. due to CBDB internal snapshot stack changes
    /// during AO metadata operations).
    pub snapshot: pg_sys::Snapshot,
}

// SAFETY: we don't execute within threads, despite Tantivy expecting that to be the case
unsafe impl Send for HeapFetchState {}
unsafe impl Sync for HeapFetchState {}

impl HeapFetchState {
    /// Create a [`HeapFetchState`] which will fetch the entire content of the given relation.
    /// The active snapshot is captured once and reused for all subsequent fetches.
    pub fn new(heaprel: &PgSearchRelation) -> Self {
        unsafe {
            let scan = pg_sys::table_index_fetch_begin(heaprel.as_ptr());
            // Use the AM-appropriate slot type (TTSOpsBufferHeapTuple for heap,
            // TTSOpsVirtual for AO/AOCO tables)
            let slot_ops = pg_sys::table_slot_callbacks(heaprel.as_ptr());
            let slot = pg_sys::MakeTupleTableSlot(heaprel.rd_att, slot_ops);
            let snapshot = pg_sys::GetActiveSnapshot();
            Self { scan, slot, snapshot }
        }
    }
}

impl Drop for HeapFetchState {
    fn drop(&mut self) {
        unsafe {
            if !crate::postgres::utils::IsTransactionState() {
                // we are not in a transaction, so we can't do things like release buffers
                return;
            }

            pg_sys::ExecDropSingleTupleTableSlot(self.slot);
            pg_sys::table_index_fetch_end(self.scan);
        }
    }
}
