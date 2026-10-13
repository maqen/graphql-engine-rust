

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


CREATE TABLE public.dummy_table (
    id integer NOT NULL,
    name text NOT NULL
);

CREATE TABLE public.users (
    id integer NOT NULL,
    name text NOT NULL,
    restricted_field text
);

INSERT INTO public.dummy_table VALUES
	(1, 'Dummy 1'),
	(2, 'Dummy 2'),
	(3, 'Dummy 3');

INSERT INTO public.users VALUES
	(1, 'User 1', 'Allowed Data 1'),
	(2, 'User 2', 'Allowed Data 2'),
	(3, 'User 3', 'Allowed Data 3');

ALTER TABLE ONLY public.dummy_table
    ADD CONSTRAINT dummy_table_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


