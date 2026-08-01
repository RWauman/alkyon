-- Mirror of the PostgreSQL demo schema, in T-SQL. Same purpose: exercise the
-- metadata endpoints and the type mapping, with enough rows to span batches.

IF DB_ID('alkyon_demo') IS NULL
    CREATE DATABASE alkyon_demo;
GO

USE alkyon_demo;
GO

IF SCHEMA_ID('sales') IS NULL
    EXEC('CREATE SCHEMA sales');
GO

DROP VIEW IF EXISTS sales.order_value;
DROP TABLE IF EXISTS sales.order_line;
DROP TABLE IF EXISTS sales.customer;
GO

CREATE TABLE sales.customer (
    id          int              IDENTITY(1,1) PRIMARY KEY,
    external_id uniqueidentifier NOT NULL CONSTRAINT df_customer_ext DEFAULT NEWID(),
    name        nvarchar(120)    NOT NULL,
    country     char(2)          NULL,
    credit      decimal(18,2)    NOT NULL CONSTRAINT df_customer_credit DEFAULT 0,
    metadata    nvarchar(max)    NULL,
    created_at  datetime2(3)     NOT NULL CONSTRAINT df_customer_created DEFAULT SYSUTCDATETIME()
);

CREATE TABLE sales.order_line (
    order_id    bigint        NOT NULL,
    line_no     smallint      NOT NULL,
    customer_id int           NOT NULL REFERENCES sales.customer (id),
    sku         varchar(20)   NOT NULL,
    quantity    int           NOT NULL,
    unit_price  decimal(12,4) NOT NULL,
    shipped_on  date          NULL,
    signature   varbinary(64) NULL,
    CONSTRAINT pk_order_line PRIMARY KEY (order_id, line_no)
);
GO

CREATE VIEW sales.order_value AS
SELECT l.order_id,
       c.name AS customer,
       SUM(l.quantity * l.unit_price) AS total
FROM sales.order_line l
JOIN sales.customer c ON c.id = l.customer_id
GROUP BY l.order_id, c.name;
GO

WITH numbers AS (
    SELECT TOP (250) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n
    FROM sys.all_objects
)
INSERT INTO sales.customer (name, country, credit, metadata)
SELECT CONCAT('Customer ', n),
       CASE n % 3 WHEN 0 THEN 'BE' WHEN 1 THEN 'FR' ELSE NULL END,
       CAST(n * 13.37 AS decimal(18,2)),
       CONCAT('{"seq":', n, ',"active":', IIF(n % 5 = 0, 'false', 'true'), '}')
FROM numbers;
GO

WITH numbers AS (
    SELECT TOP (1500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) - 1 AS n
    FROM sys.all_objects
)
INSERT INTO sales.order_line (order_id, line_no, customer_id, sku, quantity, unit_price, shipped_on, signature)
SELECT 1000 + (n / 6),
       CAST(n % 6 AS smallint),
       1 + (n % 250),
       CONCAT('SKU-', RIGHT(CONCAT('000', n % 90), 4)),
       1 + (n % 7),
       CAST(9.99 + (n % 40) AS decimal(12,4)),
       IIF(n % 4 = 0, NULL, DATEADD(day, n % 300, '2025-01-01')),
       IIF(n % 10 = 0, HASHBYTES('MD5', CAST(n AS varchar(10))), NULL)
FROM numbers;
GO

PRINT 'alkyon_demo seeded';
GO
