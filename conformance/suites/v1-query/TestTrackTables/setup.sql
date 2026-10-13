

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


CREATE TABLE public.test2 (
    id integer NOT NULL
);

CREATE FUNCTION public.test1() RETURNS SETOF public.test2
    LANGUAGE sql STABLE
    AS $$
    SELECT * FROM test2
$$;

CREATE TABLE hge_tests.resident (
    id integer NOT NULL,
    name text NOT NULL,
    age integer NOT NULL
);

CREATE SEQUENCE hge_tests.resident_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE hge_tests.resident_id_seq OWNED BY hge_tests.resident.id;

CREATE TABLE public.article (
    id integer NOT NULL,
    title text,
    author_id integer
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
    remarks text
);

CREATE MATERIALIZED VIEW public.articles AS
 SELECT article.title,
    author.name
   FROM (public.article
     LEFT JOIN public.author ON ((author.id = article.author_id)))
  WITH NO DATA;

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE TABLE public.test1 (
    id integer NOT NULL
);

CREATE SEQUENCE public.test1_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.test1_id_seq OWNED BY public.test1.id;

CREATE SEQUENCE public.test2_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.test2_id_seq OWNED BY public.test2.id;

CREATE TABLE public."users address" (
    id integer NOT NULL,
    user_id integer,
    address text
);

CREATE SEQUENCE public."users address_id_seq"
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public."users address_id_seq" OWNED BY public."users address".id;

ALTER TABLE ONLY hge_tests.resident ALTER COLUMN id SET DEFAULT nextval('hge_tests.resident_id_seq'::regclass);

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.test1 ALTER COLUMN id SET DEFAULT nextval('public.test1_id_seq'::regclass);

ALTER TABLE ONLY public.test2 ALTER COLUMN id SET DEFAULT nextval('public.test2_id_seq'::regclass);

ALTER TABLE ONLY public."users address" ALTER COLUMN id SET DEFAULT nextval('public."users address_id_seq"'::regclass);

INSERT INTO hge_tests.resident VALUES
	(1, 'Resident 1', 23),
	(2, 'Resident 2', 31);

INSERT INTO public.article VALUES
	(1, 'Lorem ipsum dolor sit amet', 1),
	(2, 'lolcats: an anthology', 2),
	(3, 'consectetur adipiscing elit', 1);

INSERT INTO public.author VALUES
	(1, 'Author 1', NULL),
	(2, 'Author 2', NULL);

SELECT pg_catalog.setval('hge_tests.resident_id_seq', 2, true);

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.test1_id_seq', 1, false);

SELECT pg_catalog.setval('public.test2_id_seq', 1, false);

SELECT pg_catalog.setval('public."users address_id_seq"', 1, false);

ALTER TABLE ONLY hge_tests.resident
    ADD CONSTRAINT resident_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.test1
    ADD CONSTRAINT test1_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.test2
    ADD CONSTRAINT test2_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public."users address"
    ADD CONSTRAINT "users address_pkey" PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

REFRESH MATERIALIZED VIEW public.articles;


