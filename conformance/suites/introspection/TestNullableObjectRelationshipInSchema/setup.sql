

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


CREATE TABLE public.joint_foreign_key (
    id integer NOT NULL,
    table2_name text NOT NULL,
    table2_table1_id integer
);

CREATE SEQUENCE public.joint_foreign_key_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.joint_foreign_key_id_seq OWNED BY public.joint_foreign_key.id;

CREATE TABLE public.table1 (
    id integer NOT NULL,
    name text
);

CREATE SEQUENCE public.table1_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.table1_id_seq OWNED BY public.table1.id;

CREATE TABLE public.table2 (
    id integer NOT NULL,
    name text,
    table1_id integer,
    table1_id_not_null integer NOT NULL
);

CREATE SEQUENCE public.table2_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.table2_id_seq OWNED BY public.table2.id;

ALTER TABLE ONLY public.joint_foreign_key ALTER COLUMN id SET DEFAULT nextval('public.joint_foreign_key_id_seq'::regclass);

ALTER TABLE ONLY public.table1 ALTER COLUMN id SET DEFAULT nextval('public.table1_id_seq'::regclass);

ALTER TABLE ONLY public.table2 ALTER COLUMN id SET DEFAULT nextval('public.table2_id_seq'::regclass);

SELECT pg_catalog.setval('public.joint_foreign_key_id_seq', 1, false);

SELECT pg_catalog.setval('public.table1_id_seq', 1, false);

SELECT pg_catalog.setval('public.table2_id_seq', 1, false);

ALTER TABLE ONLY public.joint_foreign_key
    ADD CONSTRAINT joint_foreign_key_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.table1
    ADD CONSTRAINT table1_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.table2
    ADD CONSTRAINT table2_name_table1_id_key UNIQUE (name, table1_id);

ALTER TABLE ONLY public.table2
    ADD CONSTRAINT table2_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.table2
    ADD CONSTRAINT table2_table1_id_key UNIQUE (table1_id);

ALTER TABLE ONLY public.table2
    ADD CONSTRAINT table2_table1_id_not_null_key UNIQUE (table1_id_not_null);

ALTER TABLE ONLY public.joint_foreign_key
    ADD CONSTRAINT joint_foreign_key_table2_name_table2_table1_id_fkey FOREIGN KEY (table2_name, table2_table1_id) REFERENCES public.table2(name, table1_id);

ALTER TABLE ONLY public.table2
    ADD CONSTRAINT table2_table1_id_fkey FOREIGN KEY (table1_id) REFERENCES public.table1(id);

ALTER TABLE ONLY public.table2
    ADD CONSTRAINT table2_table1_id_not_null_fkey FOREIGN KEY (table1_id_not_null) REFERENCES public.table1(id);


