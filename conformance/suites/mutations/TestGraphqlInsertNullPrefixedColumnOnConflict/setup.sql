

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


CREATE TABLE public."nullPrefixTestTable" (
    id integer NOT NULL,
    "nullName" text
);

CREATE SEQUENCE public."nullPrefixTestTable_id_seq"
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public."nullPrefixTestTable_id_seq" OWNED BY public."nullPrefixTestTable".id;

ALTER TABLE ONLY public."nullPrefixTestTable" ALTER COLUMN id SET DEFAULT nextval('public."nullPrefixTestTable_id_seq"'::regclass);

SELECT pg_catalog.setval('public."nullPrefixTestTable_id_seq"', 1, false);

ALTER TABLE ONLY public."nullPrefixTestTable"
    ADD CONSTRAINT "nullPrefixTestTable_pkey" PRIMARY KEY (id);


