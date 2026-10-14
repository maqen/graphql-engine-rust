

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

SET default_tablespace = '';


CREATE TABLE public.test_table (
    first_name text,
    last_name text,
    id uuid DEFAULT gen_random_uuid() NOT NULL
);

CREATE TABLE public.test_table_recurse (
    random_data text,
    id uuid DEFAULT gen_random_uuid() NOT NULL
);

INSERT INTO public.test_table VALUES
	('Foo', 'Bar', 'c7bac169-17a0-4af1-bd11-5c658cd3b46c'),
	('Baz', 'Qux', 'e36efcb3-1941-40bc-87b9-7a2e08697361'),
	('X%20Y', 'Test', '697f4c40-28a0-46f2-adf8-af4681c643de');


