

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

CREATE SCHEMA hge_tests;

CREATE EXTENSION IF NOT EXISTS citext WITH SCHEMA public;

COMMENT ON EXTENSION citext IS 'data type for case-insensitive character strings';

CREATE EXTENSION IF NOT EXISTS ltree WITH SCHEMA public;

COMMENT ON EXTENSION ltree IS 'data type for hierarchical tree-like structures';

CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public;

COMMENT ON EXTENSION pgcrypto IS 'cryptographic functions';

CREATE EXTENSION IF NOT EXISTS postgis WITH SCHEMA public;

COMMENT ON EXTENSION postgis IS 'PostGIS geometry and geography spatial types and functions';

SET default_tablespace = '';


CREATE TABLE public.test_table (
    first_name text,
    last_name text,
    created_at date,
    point public.geometry,
    id uuid DEFAULT gen_random_uuid() NOT NULL
);

INSERT INTO public.test_table VALUES
	('Foo', 'Bar', NULL, NULL, '5ab555c1-ed39-4ebb-89f7-02d65356cc46'),
	('Baz', 'Qux', NULL, NULL, '0ace2bdd-43a3-4d6d-b935-d1e0770cef01'),
	('X%20Y', 'Test', NULL, NULL, '2e9587c9-19de-42e3-bd67-3ec067ee8e00');


