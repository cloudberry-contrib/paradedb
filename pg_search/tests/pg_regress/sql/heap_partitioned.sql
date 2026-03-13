-- Test pg_search on heap (regular) partitioned tables in CBDB

DROP TABLE IF EXISTS products_hp CASCADE;

CREATE TABLE products_hp (
    id SERIAL,
    description TEXT,
    rating INTEGER,
    category TEXT,
    price NUMERIC,
    in_stock BOOLEAN,
    availability_date date
)
DISTRIBUTED BY (id)
PARTITION BY RANGE(availability_date) (
    PARTITION p201507 START ('2015-07-01'::date) END ('2015-08-01'::date),
    PARTITION p201508 START ('2015-08-01'::date) END ('2015-09-01'::date),
    DEFAULT PARTITION pothers
);

INSERT INTO products_hp (description, rating, category, price, in_stock, availability_date) VALUES
('Laptop with fast processor', 5, 'Electronics', 999.99,  true,  '2015-07-01'),
('Gaming laptop with RGB',     5, 'Electronics', 1299.99, true,  '2015-07-31'),
('Toy laptop for kids',        3, 'Toys',        499.99,  false, '2015-08-01'),
('Wireless keyboard and mouse',4, 'Electronics', 79.99,   true,  '2015-08-31'),
('Mechanical keyboard RGB',    5, 'Electronics', 149.99,  true,  '2015-08-11'),
('Running shoes for athletes', 5, 'Sports',      89.99,   true,  '2015-08-21'),
('Winter jacket warm',         4, 'Clothing',    129.99,  true,  '2015-07-11'),
('Summer jacket light',        3, 'Clothing',    59.99,   true,  '2015-08-21');

CREATE INDEX products_hp_idx ON products_hp
USING bm25 (id, description, rating, category, price)
WITH (
    key_field='id',
    text_fields='{"description": {}, "category": {"fast": true}}',
    numeric_fields='{"rating": {"fast": true}, "price": {"fast": true}}'
);

-- Basic search on heap partitioned table
SELECT id, description FROM products_hp WHERE description @@@ 'laptop' ORDER BY id;

-- Aggregate on heap partitioned table
SELECT COUNT(*), SUM(price) FROM products_hp WHERE description @@@ 'laptop';

-- Explain aggregate query
EXPLAIN (FORMAT TEXT, COSTS OFF, TIMING OFF, VERBOSE)
SELECT COUNT(*), SUM(price) FROM products_hp WHERE description @@@ 'laptop';

-- -----------------------------------------------------------------------
-- Heap field filter: non-BM25-indexed columns in AND conditions
-- (in_stock, availability_date are NOT in the BM25 index, so the filter
--  is applied against the heap after the BM25 scan via heap_field_filter)
-- -----------------------------------------------------------------------

-- Filter on boolean heap column (in_stock not indexed)
SELECT id, description, in_stock
FROM products_hp
WHERE description @@@ 'laptop' AND in_stock = true
ORDER BY id;

-- Filter on date heap column (availability_date not indexed)
SELECT id, description, availability_date
FROM products_hp
WHERE description @@@ 'laptop' AND availability_date < '2015-08-01'
ORDER BY id;

-- Combined: BM25 + two non-indexed heap filters
SELECT id, description, in_stock, availability_date
FROM products_hp
WHERE description @@@ 'laptop'
  AND in_stock = true
  AND availability_date < '2015-08-01'
ORDER BY id;

-- Aggregate with non-indexed heap filter
SELECT COUNT(*), SUM(price)
FROM products_hp
WHERE description @@@ 'laptop' AND in_stock = true;

-- -----------------------------------------------------------------------
-- Snippet generation on heap partitioned table
-- -----------------------------------------------------------------------
DROP TABLE IF EXISTS products_heap CASCADE;

CREATE TABLE products_heap (
    id SERIAL,
    description TEXT,
    category TEXT,
    created_date date
)
DISTRIBUTED BY (id)
PARTITION BY RANGE(created_date) (
    PARTITION p2015h1 START ('2015-01-01'::date) END ('2015-07-01'::date),
    PARTITION p2015h2 START ('2015-07-01'::date) END ('2016-01-01'::date),
    DEFAULT PARTITION pothers
);

INSERT INTO products_heap (description, category, created_date) VALUES
('Laptop with fast processor', 'Electronics', '2015-02-01'),
('Gaming laptop with RGB',     'Electronics', '2015-08-01'),
('Toy laptop for kids',        'Toys',        '2015-11-01'),
('Wireless keyboard and mouse','Electronics', '2015-03-01');

CREATE INDEX products_heap_idx ON products_heap
USING bm25 (id, description, category)
WITH (
    key_field='id',
    text_fields='{"description": {}, "category": {}}'
);

-- Snippet on heap partitioned table
SELECT id, paradedb.snippet(description)
FROM products_heap
WHERE description @@@ 'laptop'
ORDER BY id;

DROP TABLE IF EXISTS products_heap CASCADE;

DROP TABLE IF EXISTS products_hp CASCADE;
