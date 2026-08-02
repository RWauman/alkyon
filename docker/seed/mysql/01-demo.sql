-- Demo schema for Alkyon, MySQL edition. Mirrors the PostgreSQL and SQL Server
-- seeds so the same dialect-agnostic tests run against all three.
--
-- The database *is* the schema here, which is why this file has no `CREATE
-- SCHEMA`: the compose file creates a database called `sales` and everything
-- below lands in it, making `sales.order_line` resolve exactly as it does on the
-- other two engines.

-- `generate_series` has no MySQL equivalent, so the rows come from a recursive
-- CTE — and the default recursion ceiling is 1000, below the 1500 order lines.
SET SESSION cte_max_recursion_depth = 2000;

CREATE TABLE customer (
    id          int           NOT NULL AUTO_INCREMENT PRIMARY KEY,
    -- MySQL has no uuid type; char(36) is what everyone uses.
    external_id char(36)      NOT NULL,
    name        varchar(120)  NOT NULL,
    country     char(2),
    credit      decimal(18,2) NOT NULL DEFAULT 0,
    -- No array type either, so the tags are a JSON array.
    tags        json,
    metadata    json,
    created_at  timestamp     NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE order_line (
    order_id    bigint        NOT NULL,
    line_no     smallint      NOT NULL,
    customer_id int           NOT NULL,
    sku         varchar(64)   NOT NULL,
    quantity    int           NOT NULL,
    unit_price  decimal(12,4) NOT NULL,
    shipped_on  date,
    signature   varbinary(64),
    PRIMARY KEY (order_id, line_no),
    CONSTRAINT fk_order_customer FOREIGN KEY (customer_id) REFERENCES customer (id)
);

CREATE VIEW order_value AS
SELECT l.order_id,
       c.name AS customer,
       SUM(l.quantity * l.unit_price) AS total
FROM order_line l
JOIN customer c ON c.id = l.customer_id
GROUP BY l.order_id, c.name;

INSERT INTO customer (external_id, name, country, credit, tags, metadata)
WITH RECURSIVE n(i) AS (
    SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 250
)
SELECT UUID(),
       CONCAT('Customer ', i),
       CASE i % 3 WHEN 0 THEN 'BE' WHEN 1 THEN 'FR' ELSE NULL END,
       ROUND(i * 13.37, 2),
       JSON_ARRAY(CONCAT('tier-', i % 4), 'seed'),
       JSON_OBJECT('seq', i, 'active', i % 5 <> 0)
FROM n;

INSERT INTO order_line (order_id, line_no, customer_id, sku, quantity, unit_price, shipped_on, signature)
WITH RECURSIVE n(i) AS (
    SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 1499
)
SELECT 1000 + FLOOR(i / 6),
       (i % 6),
       1 + (i % 250),
       CONCAT('SKU-', LPAD(i % 90, 4, '0')),
       1 + (i % 7),
       ROUND(9.99 + (i % 40), 4),
       CASE WHEN i % 4 = 0 THEN NULL ELSE DATE_ADD('2025-01-01', INTERVAL (i % 300) DAY) END,
       CASE WHEN i % 10 = 0 THEN UNHEX(MD5(i)) END
FROM n;

ANALYZE TABLE customer, order_line;
