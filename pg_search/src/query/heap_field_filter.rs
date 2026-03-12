use crate::postgres::rel::PgSearchRelation;
use crate::query::PostgresPointer;
use pgrx::FromDatum;
use pgrx::{pg_sys, PgMemoryContexts};
use serde::{Deserialize, Serialize};
use tantivy::{
    query::{EnableScoring, Explanation, Query, Scorer, Weight},
    DocId, DocSet, Score, SegmentReader, TERMINATED,
};

/// Core heap-based field filter using PostgreSQL expression evaluation
/// This approach stores a serialized representation of the PostgreSQL expression
/// and evaluates it directly against heap tuples, supporting any PostgreSQL operator or function
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeapFieldFilter {
    /// PostgreSQL expression node that can be serialized and reconstructed
    expr_node: PostgresPointer,
    /// Human-readable description of the expression
    pub description: String,

    #[serde(skip)]
    initialized_expression: Option<*mut pg_sys::ExprState>,
}

// SAFETY:  we don't execute within threads, despite Tantivy expecting that to be the case
unsafe impl Send for HeapFieldFilter {}
unsafe impl Sync for HeapFieldFilter {}

impl HeapFieldFilter {
    /// Create a new HeapFieldFilter from a PostgreSQL expression node
    pub unsafe fn new(expr_node: *mut pg_sys::Node, expr_desc: String) -> Self {
        Self {
            expr_node: PostgresPointer(expr_node.cast()),
            description: expr_desc,
            initialized_expression: None,
        }
    }

    /// Evaluate this filter against a heap tuple identified by ctid
    /// Uses PostgreSQL's expression evaluation system
    pub unsafe fn evaluate(
        &mut self,
        ctid: pg_sys::ItemPointer,
        heaprel: &PgSearchRelation,
    ) -> bool {
        // Get the expression node
        let expr_node = self.expr_node.0.cast::<pg_sys::Node>();
        if expr_node.is_null() {
            return true;
        }

        self.evaluate_expression_inner(ctid, heaprel, expr_node)
    }

    /// Inner expression evaluation method that can be wrapped in panic handling
    unsafe fn evaluate_expression_inner(
        &mut self,
        ctid: pg_sys::ItemPointer,
        relation: &PgSearchRelation,
        expr_node: *mut pg_sys::Node,
    ) -> bool {
        // Use table_index_fetch_tuple (table-AM-aware, works for heap and AO/AOCO tables)
        let scan = pg_sys::table_index_fetch_begin(relation.as_ptr());
        // Use the AM-appropriate slot type so AO/AOCO tables can store tuples into it
        let slot_ops = pg_sys::table_slot_callbacks(relation.as_ptr());
        let slot = pg_sys::MakeTupleTableSlot(relation.rd_att, slot_ops);

        let mut call_again = false;
        let mut all_dead = false;
        let found = pg_sys::table_index_fetch_tuple(
            scan,
            ctid,
            pg_sys::GetActiveSnapshot(),
            slot,
            &mut call_again,
            &mut all_dead,
        );

        if !found {
            pg_sys::ExecDropSingleTupleTableSlot(slot);
            pg_sys::table_index_fetch_end(scan);
            return false;
        }

        // Create an expression context for evaluation
        let econtext = pg_sys::CreateStandaloneExprContext();
        if econtext.is_null() {
            pg_sys::ExecDropSingleTupleTableSlot(slot);
            pg_sys::table_index_fetch_end(scan);
            return false;
        }

        // Set the tuple slot in the expression context
        (*econtext).ecxt_scantuple = slot;

        // Initialize the expression for execution
        let expr_state = *self.initialized_expression.get_or_insert_with(|| {
            PgMemoryContexts::TopTransactionContext
                .switch_to(|_| pg_sys::ExecInitExpr(expr_node.cast(), std::ptr::null_mut()))
        });
        if expr_state.is_null() {
            self.initialized_expression = None;
            pg_sys::FreeExprContext(econtext, false);
            pg_sys::ExecDropSingleTupleTableSlot(slot);
            pg_sys::table_index_fetch_end(scan);
            return false;
        }

        // Evaluate the expression
        let mut is_null = false;
        let result = pg_sys::ExecEvalExpr(expr_state, econtext, &mut is_null);
        let eval_result = bool::from_datum(result, is_null).unwrap_or(false);

        // Cleanup resources
        pg_sys::FreeExprContext(econtext, false);
        pg_sys::ExecDropSingleTupleTableSlot(slot);
        pg_sys::table_index_fetch_end(scan);

        eval_result
    }

    /// Get the PostgreSQL expression node
    pub unsafe fn get_expression_node(&self) -> *mut pg_sys::Node {
        self.expr_node.0.cast()
    }

    // The new expression-based approach handles evaluation directly
}

/// Tantivy query that combines indexed search with heap field filtering
#[derive(Debug)]
pub struct HeapFilterQuery {
    indexed_query: Box<dyn Query>,
    field_filters: Vec<HeapFieldFilter>,
    rel_oid: pg_sys::Oid,
}

impl HeapFilterQuery {
    pub fn new(
        indexed_query: Box<dyn Query>,
        field_filters: Vec<HeapFieldFilter>,
        rel_oid: pg_sys::Oid,
    ) -> Self {
        Self {
            indexed_query,
            field_filters,
            rel_oid,
        }
    }
}

impl tantivy::query::QueryClone for HeapFilterQuery {
    fn box_clone(&self) -> Box<dyn Query> {
        Box::new(Self {
            indexed_query: self.indexed_query.box_clone(),
            field_filters: self.field_filters.clone(),
            rel_oid: self.rel_oid,
        })
    }
}

impl Query for HeapFilterQuery {
    fn weight(&self, enable_scoring: EnableScoring) -> tantivy::Result<Box<dyn Weight>> {
        let indexed_weight = self.indexed_query.weight(enable_scoring)?;
        Ok(Box::new(HeapFilterWeight {
            indexed_weight,
            field_filters: self.field_filters.clone(),
            rel_oid: self.rel_oid,
        }))
    }
}

struct HeapFilterWeight {
    indexed_weight: Box<dyn Weight>,
    field_filters: Vec<HeapFieldFilter>,
    rel_oid: pg_sys::Oid,
}

impl Weight for HeapFilterWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let indexed_scorer = self.indexed_weight.scorer(reader, boost)?;

        // Get ctid fast field for heap access
        let fast_fields_reader = reader.fast_fields();
        let ctid_ff = crate::index::fast_fields_helper::FFType::new_ctid(fast_fields_reader);

        let scorer = HeapFilterScorer::new(
            indexed_scorer,
            self.field_filters.clone(),
            ctid_ff,
            self.rel_oid,
        );

        Ok(Box::new(scorer))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let indexed_explanation = self.indexed_weight.explain(reader, doc)?;
        Ok(Explanation::new("HeapFilter", indexed_explanation.value()))
    }
}

struct HeapFilterScorer {
    indexed_scorer: Box<dyn Scorer>,
    field_filters: Vec<HeapFieldFilter>,
    ctid_ff: crate::index::fast_fields_helper::FFType,
    heaprel: PgSearchRelation,
    current_doc: DocId,
}

// SAFETY:  we don't execute within threads, despite Tantivy expecting that to be the case
unsafe impl Send for HeapFilterScorer {}
unsafe impl Sync for HeapFilterScorer {}

impl HeapFilterScorer {
    fn new(
        indexed_scorer: Box<dyn Scorer>,
        field_filters: Vec<HeapFieldFilter>,
        ctid_ff: crate::index::fast_fields_helper::FFType,
        rel_oid: pg_sys::Oid,
    ) -> Self {
        let mut scorer = Self {
            indexed_scorer,
            field_filters,
            ctid_ff,
            heaprel: PgSearchRelation::open(rel_oid),
            current_doc: TERMINATED,
        };

        // Position at the first valid document
        // For initialization, we need to check the current document first, then advance if needed
        scorer.find_first_valid_document();

        scorer
    }

    fn find_first_valid_document(&mut self) {
        // For initialization, check the current document first
        self.current_doc = self.indexed_scorer.doc();

        if self.current_doc != TERMINATED && self.passes_heap_filters(self.current_doc) {
            return;
        }

        // If current document doesn't pass, advance to find the next valid one
        self.advance();
    }

    fn passes_heap_filters(&mut self, doc_id: DocId) -> bool {
        // Extract ctid from the current document
        let ctid_value = self.ctid_ff.as_u64(doc_id);
        if ctid_value.is_none() {
            panic!("Could not get ctid for doc_id: {doc_id}");
        }
        let ctid_value = ctid_value.unwrap();
        // Convert u64 ctid back to ItemPointer
        let mut item_pointer = pg_sys::ItemPointerData::default();
        crate::postgres::utils::u64_to_item_pointer(ctid_value, &mut item_pointer);

        // Evaluate all heap filters
        for filter in self.field_filters.iter_mut() {
            unsafe {
                let filter_result = filter.evaluate(
                    &mut item_pointer as *mut pg_sys::ItemPointerData,
                    &self.heaprel,
                );
                if !filter_result {
                    return false;
                }
            }
        }

        true
    }
}

impl Scorer for HeapFilterScorer {
    fn score(&mut self) -> Score {
        // Return the score from the indexed query (preserving BM25 scores)
        self.indexed_scorer.score()
    }
}

impl DocSet for HeapFilterScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.indexed_scorer.advance();

            if doc == TERMINATED {
                self.current_doc = TERMINATED;
                return TERMINATED;
            }

            if self.passes_heap_filters(doc) {
                self.current_doc = doc;
                return doc;
            }
        }
    }

    fn doc(&self) -> DocId {
        self.current_doc
    }

    fn size_hint(&self) -> u32 {
        self.indexed_scorer.size_hint()
    }
}
