#!/usr/bin/env bash
set -euo pipefail

# Resolve the directory containing this script
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Locate pg_regress dynamically via pg_config --pgxs
# e.g. /usr/local/gpdb/lib/postgresql/pgxs/src/makefiles/pgxs.mk
#   -> /usr/local/gpdb/lib/postgresql/pgxs/src/test/regress/pg_regress
PG_CONFIG="${PGRX_PG_CONFIG_PATH:-$(which pg_config)}"
PGXS="$("$PG_CONFIG" --pgxs)"
PG_REGRESS="$(dirname "$PGXS")/../test/regress/pg_regress"
PG_REGRESS="$(cd "$(dirname "$PG_REGRESS")" && pwd)/$(basename "$PG_REGRESS")"

# Run pg_regress
env -u PGDATABASE -u PGHOST -u PGPORT -u PGUSER \
  "$PG_REGRESS" \
  --host "localhost" \
  --port "7000" \
  --use-existing \
  --dbname="gpadmin" \
  --inputdir="$SCRIPT_DIR" \
  --outputdir="$SCRIPT_DIR" \
  --ignore-plans \
  --init-file=./init_file \
  setup \
  ao_partitioned \
  aggregate-udf \
  aggregate \
  boost \
  custom_scan_is_numeric_fast_field_capable \
  example-test \
  exists_json \
  fast_fields_options \
  find_ctid \
  fuzzy \
  generated_subquery_proptest_failure \
  groupby_aggregate \
  index_config_errors \
  inet \
  issue_2528 \
  issue_2533 \
  issue_2564-parallel \
  issue_2564 \
  issue_2585 \
  issue_2688 \
  issue_2745 \
  issue_2753 \
  issue_2904 \
  issue_2932 \
  join_tests \
  json_operator \
  keys_snippet_score \
  keyword_defaults_fast \
  layer_size_config \
  misconfigured_configs \
  mixed_fast_fields_bug \
  mixedff_advanced_01_aggregation \
  mixedff_advanced_02_mixed_fast_non_fast \
  mixedff_advanced_03_limit_topn \
  mixedff_advanced_04_execution_method_selection \
  mixedff_advanced_05_union_window_functions \
  mixedff_advanced_06_score_function \
  mixedff_advanced_07_recursive_cte \
  mixedff_advanced_08_type_conversion \
  mixedff_advanced_09_multi_index_search \
  mixedff_basic_01_basic_mixed_fields \
  mixedff_basic_02_multiple_string_fields \
  mixedff_basic_03_multiple_numeric_fields \
  mixedff_basic_04_mixed_field_types \
  mixedff_basic_05_uuid \
  mixedff_edgecases_01_corner_cases \
  mixedff_edgecases_02_null_handling \
  mixedff_edgecases_03_string_edge_cases \
  mixedff_edgecases_04_complex_string_patterns \
  mixedff_edgecases_05_numeric_handling \
  mixedff_queries_01_complex_join \
  mixedff_queries_02_order_by \
  mixedff_queries_03_cte_test \
  mixedff_queries_04_subquery \
  mixedff_queries_05_join2 \
  not-paradedb_all \
  operators \
  parallel_build_empty \
  parallel_build_large \
  parallel_build_small \
  partial_index_score_fix \
  phrase_tokenization \
  proximity \
  pushdown_scalar_array_opexr \
  score_join_predicates \
  score_non_indexed_predicates \
  snippet_join_predicates \
  snippet_json_01_basic \
  snippet_json_02_advanced \
  snippet_position_01_advanced \
  snippet_position_01_basic \
  stopwords \
  string_id_limit \
  top_n_scan \
  unified_expression_comprehensive

