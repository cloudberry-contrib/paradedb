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
#![recursion_limit = "512"]

mod aggregate;
mod api;
mod bootstrap;
mod index;
mod postgres;
mod query;
mod schema;

pub mod gucs;
pub mod parallel_worker;

use self::postgres::customscan;
use pgrx::*;

// 规划器钩子静态变量
static mut ORIGINAL_PLANNER_HOOK: Option<pg_sys::planner_hook_type> = None;

/// The prefix applied to tantivy fields that are actually Postgres expressions.
pub const PG_SEARCH_PREFIX: &str = "_pg_search_";

/// Postgres' value for a `norm_selec` that hasn't been assigned
const UNASSIGNED_SELECTIVITY: f64 = -1.0;

/// A hardcoded value when we can't figure out a good selectivity value
const UNKNOWN_SELECTIVITY: f64 = 0.00001;

/// A hardcoded value for parameterized plan queries
const PARAMETERIZED_SELECTIVITY: f64 = 0.10;

/// The selectivity value indicating the entire relation will be returned
const FULL_RELATION_SELECTIVITY: f64 = 1.0;

/// An arbitrary value for what it costs for a plan with one of our operators (@@@) to do whatever
/// initial work it needs to do (open tantivy index, start the query, etc).  The value is largely
/// meaningless but we should be honest that do _something_.
const DEFAULT_STARTUP_COST: f64 = 10.0;

pgrx::pg_module_magic!();

extension_sql!(
    r#"
        GRANT ALL ON SCHEMA paradedb TO PUBLIC;
        GRANT ALL ON SCHEMA pdb TO PUBLIC;
    "#,
    name = "paradedb_grant_all",
    finalize
);

/// Initializes option parsing
#[allow(clippy::missing_safety_doc)]
#[allow(non_snake_case)]
#[pg_guard]
pub unsafe extern "C-unwind" fn _PG_init() {
    // initialize environment logging (to stderr) for dependencies that do logging
    // we can't implement our own logger that sends messages to Postgres `ereport()` because
    // of threading concerns
    std::env::set_var("RUST_LOG", "warn");
    std::env::set_var("RUST_LOG_STYLE", "never");
    env_logger::init();

    if cfg!(not(feature = "pg17")) && !pg_sys::process_shared_preload_libraries_in_progress {
        error!("pg_search must be loaded via shared_preload_libraries. Add 'pg_search' to shared_preload_libraries in postgresql.conf and restart Postgres.");
    }

    postgres::options::init();
    gucs::init();

    #[cfg(not(feature = "pg17"))]
    postgres::fake_aminsertcleanup::register();

    #[allow(static_mut_refs)]
    #[allow(deprecated)]
    customscan::register_rel_pathlist(customscan::pdbscan::PdbScan);
    customscan::register_upper_path(customscan::aggregatescan::AggregateScan);

    // Install query planner hook to optimize cross-table OR conditions
    install_planner_hook();
}

#[pg_extern]
fn random_words(num_words: i32) -> String {
    use rand::Rng;

    let mut rng = rand::rng();
    let letters = "abcdefghijklmnopqrstuvwxyz";
    let mut result = String::new();

    for _ in 0..num_words {
        // Choose a random word length between 3 and 7.
        let word_length = rng.random_range(3..=7);
        let mut word = String::new();

        for _ in 0..word_length {
            // Pick a random letter from the letters string.
            let random_index = rng.random_range(0..letters.len());
            // Safe to use .unwrap() because the index is guaranteed to be valid.
            let letter = letters.chars().nth(random_index).unwrap();
            word.push(letter);
        }
        result.push_str(&word);
        result.push(' ');
    }
    result.trim_end().to_string()
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests

        let mut options: Vec<&'static str> = Vec::new();

        if cfg!(not(feature = "pg17")) {
            options.push("shared_preload_libraries='pg_search'");
            options.push("log_statement='all'");
        }

        options
    }
}

/// Install query planner hook
unsafe fn install_planner_hook() {
    #[allow(static_mut_refs)]
    if ORIGINAL_PLANNER_HOOK.is_none() {
        ORIGINAL_PLANNER_HOOK = Some(pg_sys::planner_hook);
        pg_sys::planner_hook = Some(pg_search_planner_hook);
    }
}

/// ParadeDB query planner hook for checking distributed query compatibility
#[pg_guard]
#[cfg(feature = "cbdb")]
unsafe extern "C-unwind" fn pg_search_planner_hook(
    parse: *mut pg_sys::Query,
    query_string: *const std::os::raw::c_char,
    cursor_options: i32,
    bound_params: pg_sys::ParamListInfo,
    optimizer_options: *mut pg_sys::OptimizerOptions,
) -> *mut pg_sys::PlannedStmt {
    // Call original planner
    #[allow(static_mut_refs)]
    let planned_stmt = if let Some(Some(original_hook)) = ORIGINAL_PLANNER_HOOK {
        original_hook(parse, query_string, cursor_options, bound_params, optimizer_options)
    } else {
        pg_sys::standard_planner(parse, query_string, cursor_options, bound_params, optimizer_options)
    };

    // Check for motion nodes and report error if found (skip only for EXPLAIN without ANALYZE)
    if !should_skip_motion_check(parse) {
        check_for_motion_nodes(planned_stmt);
    }

    planned_stmt
}

/// ParadeDB query planner hook for checking distributed query compatibility
#[pg_guard]
#[cfg(not(feature = "cbdb"))]
unsafe extern "C-unwind" fn pg_search_planner_hook(
    parse: *mut pg_sys::Query,
    query_string: *const std::os::raw::c_char,
    cursor_options: i32,
    bound_params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    // Call original planner
    #[allow(static_mut_refs)]
    let planned_stmt = if let Some(Some(original_hook)) = ORIGINAL_PLANNER_HOOK {
        original_hook(parse, query_string, cursor_options, bound_params)
    } else {
        pg_sys::standard_planner(parse, query_string, cursor_options, bound_params)
    };

    // Check for motion nodes and report error if found (skip only for EXPLAIN without ANALYZE)
    if !should_skip_motion_check(parse) {
        check_for_motion_nodes(planned_stmt);
    }

    planned_stmt
}

/// Check execution plan for motion nodes and report error if found
unsafe fn check_for_motion_nodes(planned_stmt: *mut pg_sys::PlannedStmt) {
    if planned_stmt.is_null() || (*planned_stmt).planTree.is_null() {
        return;
    }

    // Traverse execution plan tree to detect motion nodes
    walk_plan_tree((*planned_stmt).planTree, &mut |node| check_motion_node(node));
}

/// Helper function to traverse plan tree
unsafe fn walk_plan_tree(
    plan_node: *mut pg_sys::Plan,
    visitor: &mut dyn FnMut(*mut pg_sys::Plan)
) {
    if plan_node.is_null() {
        return;
    }

    visitor(plan_node);

    // Recursively visit child nodes
    walk_plan_tree((*plan_node).lefttree, visitor);
    walk_plan_tree((*plan_node).righttree, visitor);

    // Handle child nodes of other plan node types
    match (*plan_node).type_ {
        pg_sys::NodeTag::T_Append => {
            let append_plan = plan_node as *mut pg_sys::Append;
            let subplans = PgList::<pg_sys::Plan>::from_pg((*append_plan).appendplans);
            for subplan in subplans.iter_ptr() {
                walk_plan_tree(subplan, visitor);
            }
        }
        pg_sys::NodeTag::T_SubqueryScan => {
            let subquery_plan = plan_node as *mut pg_sys::SubqueryScan;
            walk_plan_tree((*subquery_plan).subplan, visitor);
        }
        _ => {
            // Other node types don't need special handling for now
        }
    }
}

/// Check if plan node is a motion node and report error if found
unsafe fn check_motion_node(plan_node: *mut pg_sys::Plan) {
    if plan_node.is_null() {
        return;
    }

    // Check for Motion node type (T_Motion = 70 in CBDB)
    if (*plan_node).type_ == pg_sys::NodeTag::T_Motion {
        let motion_node = plan_node as *mut pg_sys::Motion;
        let motion_type = (*motion_node).motionType;

        // Allow Gather Motion types (used to collect results), but block other Motion types
        match motion_type {
            pg_sys::MotionType::MOTIONTYPE_GATHER |
            pg_sys::MotionType::MOTIONTYPE_GATHER_SINGLE => {
                // Gather Motion is allowed - it just collects results from segments
            }
            _ => {
                error!("ParadeDB search queries are not supported in distributed environments with data redistribution motion nodes");
            }
        }
    }
}

/// Check if motion node checking should be skipped based on EXPLAIN analysis or non-SELECT statements
unsafe fn should_skip_motion_check(parse: *mut pg_sys::Query) -> bool {
    if parse.is_null() {
        return false;
    }

    // Skip motion check for non-SELECT DML operations - only SELECT needs motion node checking
    match (*parse).commandType {
        pg_sys::CmdType::CMD_INSERT |
        pg_sys::CmdType::CMD_UPDATE |
        pg_sys::CmdType::CMD_DELETE => {
            return true;
        }
        pg_sys::CmdType::CMD_SELECT => {
            // SELECT statements need motion node checking, continue with other checks
        }
        _ => {
            // Continue with other checks for non-DML commands
        }
    }

    // Check if this is a utility statement (EXPLAIN is a utility statement)
    let utility_stmt = (*parse).utilityStmt;
    if utility_stmt.is_null() || (*utility_stmt).type_ != pg_sys::NodeTag::T_ExplainStmt {
        return false; // Not an EXPLAIN command, don't skip
    }

    // Cast to ExplainStmt
    let explain_stmt = utility_stmt as *mut pg_sys::ExplainStmt;
    let options = (*explain_stmt).options;

    // Check if ANALYZE option is present
    if !options.is_null() {
        let option_list = PgList::<pg_sys::DefElem>::from_pg(options);
        for option in option_list.iter_ptr() {
            if !option.is_null() {
                let defname = (*option).defname;
                if !defname.is_null() {
                    let option_name = std::ffi::CStr::from_ptr(defname);
                    if let Ok(name_str) = option_name.to_str() {
                        if name_str.eq_ignore_ascii_case("analyze") {
                            return false; // EXPLAIN ANALYZE needs execution, don't skip motion check
                        }
                    }
                }
            }
        }
    }

    // This is EXPLAIN without ANALYZE, skip motion check
    true
}

