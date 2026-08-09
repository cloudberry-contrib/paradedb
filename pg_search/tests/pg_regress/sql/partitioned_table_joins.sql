-- Regression: issue #4643
-- Cross-table OR with BM25 predicates fails when one table is partitioned
-- with error: could not open file "pg_tblspc/0/.../0/0".
--
-- Root cause: search_with_query_input() opened the parent partitioned index
-- directly, but partitioned indexes have no physical storage and Tantivy
-- panicked trying to instantiate a SearchIndexReader on an empty directory.
--
-- Fix: route through IndexKind::for_index() + IndexKind::partitions() so the
-- search expands to each leaf partition's index and the matching primary keys
-- are unioned.
--
-- CBDB note: this test uses co-located distribution (join key = distribution
-- key on both sides) so the planner produces a local Hash Join without a
-- redistribution Motion. Non-co-located joins are caught earlier by the
-- motion guard in lib.rs and never reach the buggy code path.

DROP TABLE IF EXISTS items_4643 CASCADE;
DROP TABLE IF EXISTS orders_part_4643 CASCADE;

CREATE TABLE items_4643 (
    id bigint PRIMARY KEY,
    description text
) DISTRIBUTED BY (id);

INSERT INTO items_4643(id, description)
SELECT g,
       CASE g % 4
         WHEN 0 THEN 'mechanical keyboard'
         WHEN 1 THEN 'gaming mouse'
         WHEN 2 THEN 'usb keyboard'
         ELSE 'monitor'
       END
FROM generate_series(1, 40) g;

CREATE INDEX items_4643_idx ON items_4643
    USING bm25 (id, description)
    WITH (key_field='id');

CREATE TABLE orders_part_4643 (
    order_id bigint,
    product_id bigint NOT NULL,
    customer_name text,
    order_total numeric,
    part_key int NOT NULL,
    PRIMARY KEY (product_id, part_key)
) DISTRIBUTED BY (product_id)
PARTITION BY LIST (part_key) (
    PARTITION p0 VALUES (0),
    PARTITION p1 VALUES (1)
);

INSERT INTO orders_part_4643(order_id, product_id, customer_name, order_total, part_key)
SELECT g, g,
       CASE g % 3
         WHEN 0 THEN 'John Smith'
         WHEN 1 THEN 'Alice Wong'
         ELSE 'Bob Lee'
       END,
       (g * 7.5)::numeric,
       g % 2
FROM generate_series(1, 40) g;

CREATE INDEX orders_part_4643_idx ON orders_part_4643
    USING bm25 (order_id, part_key, product_id, order_total, customer_name)
    WITH (key_field='product_id');

-- Cross-table OR across a normal index and a partitioned index.
-- Pre-fix: ERROR: could not open file "pg_tblspc/0/.../0/0"
SELECT o.order_id, o.customer_name, i.description
FROM orders_part_4643 o
JOIN items_4643 i ON o.product_id = i.id
WHERE i.description @@@ 'keyboard' OR o.customer_name @@@ 'John'
ORDER BY o.order_total DESC LIMIT 5;

-- Direct search_with_query_input on the parent partitioned index — exercises
-- IndexKind::PartitionedIndex.partitions() unioning matches across all leaves.
SELECT product_id
FROM orders_part_4643
WHERE product_id @@@ paradedb.with_index(
    'orders_part_4643_idx',
    paradedb.parse('customer_name:John')
)
ORDER BY product_id LIMIT 5;

DROP TABLE orders_part_4643 CASCADE;
DROP TABLE items_4643 CASCADE;
