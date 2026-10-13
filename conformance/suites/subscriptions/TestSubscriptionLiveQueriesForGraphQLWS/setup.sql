

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


CREATE TABLE hge_tests.articles (
    id integer NOT NULL,
    content text,
    title text,
    is_public boolean DEFAULT false
);

CREATE SEQUENCE hge_tests.articles_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE hge_tests.articles_id_seq OWNED BY hge_tests.articles.id;

CREATE TABLE hge_tests.test_t2 (
    c1 integer,
    c2 text
);

ALTER TABLE ONLY hge_tests.articles ALTER COLUMN id SET DEFAULT nextval('hge_tests.articles_id_seq'::regclass);

SELECT pg_catalog.setval('hge_tests.articles_id_seq', 1, false);

ALTER TABLE ONLY hge_tests.articles
    ADD CONSTRAINT articles_pkey PRIMARY KEY (id);


