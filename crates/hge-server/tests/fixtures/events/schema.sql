DROP SCHEMA IF EXISTS ev CASCADE;
DROP SCHEMA IF EXISTS hdb_catalog CASCADE;
CREATE SCHEMA ev;
SET TIME ZONE 'UTC';

CREATE TABLE ev.item (
  id serial PRIMARY KEY,
  name text NOT NULL,
  price numeric,
  big bigint,
  info jsonb
);

CREATE TABLE ev.flaky (
  id serial PRIMARY KEY,
  name text NOT NULL
);

CREATE TABLE ev.broken (
  id serial PRIMARY KEY
);
