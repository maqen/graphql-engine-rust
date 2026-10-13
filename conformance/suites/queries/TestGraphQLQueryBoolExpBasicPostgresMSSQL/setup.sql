

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


CREATE TABLE public.account (
    id integer NOT NULL,
    account_no integer NOT NULL
);

CREATE SEQUENCE public.account_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.account_id_seq OWNED BY public.account.id;

CREATE TABLE public.article (
    id integer NOT NULL,
    title text,
    content text,
    author_id integer,
    is_published boolean,
    published_on timestamp without time zone
);

CREATE SEQUENCE public.article_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.article_id_seq OWNED BY public.article.id;

CREATE TABLE public.author (
    id integer NOT NULL,
    name text,
    is_registered boolean DEFAULT false NOT NULL
);

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE TABLE public.city (
    id integer NOT NULL,
    name text NOT NULL,
    country text NOT NULL
);

CREATE SEQUENCE public.city_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.city_id_seq OWNED BY public.city.id;

CREATE TABLE public.message (
    id integer NOT NULL,
    content text NOT NULL,
    parent_id integer
);

CREATE TABLE public.orders (
    id integer NOT NULL,
    received_at timestamp without time zone NOT NULL,
    delivered_at timestamp without time zone
);

CREATE SEQUENCE public.orders_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.orders_id_seq OWNED BY public.orders.id;

CREATE TABLE public.table_with_sql_identifier (
    id integer NOT NULL,
    sql_id information_schema.sql_identifier
);

CREATE SEQUENCE public.table_with_sql_identifier_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.table_with_sql_identifier_id_seq OWNED BY public.table_with_sql_identifier.id;

CREATE TABLE public."user" (
    id integer NOT NULL,
    name text NOT NULL,
    is_admin boolean DEFAULT false
);

CREATE SEQUENCE public.user_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.user_id_seq OWNED BY public."user".id;

CREATE TABLE public.uuid_test (
    id integer NOT NULL,
    uuid_col uuid NOT NULL
);

CREATE SEQUENCE public.uuid_test_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.uuid_test_id_seq OWNED BY public.uuid_test.id;

ALTER TABLE ONLY public.account ALTER COLUMN id SET DEFAULT nextval('public.account_id_seq'::regclass);

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.city ALTER COLUMN id SET DEFAULT nextval('public.city_id_seq'::regclass);

ALTER TABLE ONLY public.orders ALTER COLUMN id SET DEFAULT nextval('public.orders_id_seq'::regclass);

ALTER TABLE ONLY public.table_with_sql_identifier ALTER COLUMN id SET DEFAULT nextval('public.table_with_sql_identifier_id_seq'::regclass);

ALTER TABLE ONLY public."user" ALTER COLUMN id SET DEFAULT nextval('public.user_id_seq'::regclass);

ALTER TABLE ONLY public.uuid_test ALTER COLUMN id SET DEFAULT nextval('public.uuid_test_id_seq'::regclass);

INSERT INTO public.account VALUES
	(1, 1),
	(2, 2);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, false, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, true, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, false, NULL),
	(4, 'Article 4', 'Sample article content 4', 3, true, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', true),
	(2, 'Author 2', true),
	(3, 'Author 3', false);

INSERT INTO public.city VALUES
	(1, 'Durham', 'USA'),
	(2, 'New York', 'USA'),
	(3, 'Framlingham', 'UK'),
	(4, 'New Orleans', 'USA');

INSERT INTO public.message VALUES
	(1, 'hello world', NULL),
	(2, 'content 2', 1),
	(3, 'content 3', 1),
	(4, 'ahoy', NULL),
	(5, 'content 5', 4),
	(6, 'hello three', 4);

INSERT INTO public.orders VALUES
	(1, '2018-09-21 09:39:44', NULL),
	(2, '2018-09-21 09:40:44', '2018-09-21 09:50:44');

INSERT INTO public.table_with_sql_identifier VALUES
	(1, 'one'),
	(2, 'one'),
	(3, 'two'),
	(4, 'three'),
	(5, 'four'),
	(6, 'one'),
	(7, 'two');

INSERT INTO public."user" VALUES
	(1, 'user_1', false),
	(2, 'user_2', true);

INSERT INTO public.uuid_test VALUES
	(1, '28d6d683-1317-49f7-b1cf-7d195242e4e5'),
	(2, '28d6d683-1317-49f7-b1cf-7d195242e4e6'),
	(3, '28d6d683-1317-49f7-b1cf-7d195242e4e7');

SELECT pg_catalog.setval('public.account_id_seq', 2, true);

SELECT pg_catalog.setval('public.article_id_seq', 4, true);

SELECT pg_catalog.setval('public.author_id_seq', 3, true);

SELECT pg_catalog.setval('public.city_id_seq', 4, true);

SELECT pg_catalog.setval('public.orders_id_seq', 2, true);

SELECT pg_catalog.setval('public.table_with_sql_identifier_id_seq', 7, true);

SELECT pg_catalog.setval('public.user_id_seq', 2, true);

SELECT pg_catalog.setval('public.uuid_test_id_seq', 3, true);

ALTER TABLE ONLY public.account
    ADD CONSTRAINT account_account_no_key UNIQUE (account_no);

ALTER TABLE ONLY public.account
    ADD CONSTRAINT account_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.city
    ADD CONSTRAINT city_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.message
    ADD CONSTRAINT message_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.table_with_sql_identifier
    ADD CONSTRAINT table_with_sql_identifier_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_name_key UNIQUE (name);

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.uuid_test
    ADD CONSTRAINT uuid_test_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

ALTER TABLE ONLY public.message
    ADD CONSTRAINT parent_fk FOREIGN KEY (parent_id) REFERENCES public.message(id);


