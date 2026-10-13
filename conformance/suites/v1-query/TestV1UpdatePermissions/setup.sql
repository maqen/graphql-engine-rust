

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


CREATE TABLE public.article (
    id integer NOT NULL,
    title text,
    version text NOT NULL,
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
    name text
);

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE TABLE public.person (
    id integer NOT NULL,
    details jsonb NOT NULL
);

CREATE SEQUENCE public.person_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.person_id_seq OWNED BY public.person.id;

CREATE TABLE public.resident (
    id integer NOT NULL,
    name text NOT NULL,
    age integer NOT NULL,
    city text NOT NULL
);

CREATE SEQUENCE public.resident_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.resident_id_seq OWNED BY public.resident.id;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.person ALTER COLUMN id SET DEFAULT nextval('public.person_id_seq'::regclass);

ALTER TABLE ONLY public.resident ALTER COLUMN id SET DEFAULT nextval('public.resident_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', '1.0.0', 'Sample article 1, content version 1.0.0', 1, true, NULL),
	(2, 'Article 1', '1.0.1', 'Sample article 1, content version 1.0.1', 1, false, NULL),
	(3, 'Article 2', '1.0.0', 'Sample article 2, content version 1.0.2', 2, false, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1'),
	(2, 'Author 2');

INSERT INTO public.resident VALUES
	(1, 'phillips', 25, 'canberra'),
	(2, 'george', 26, 'sydney'),
	(3, 'clarke', 21, 'perth');

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 1, false);

SELECT pg_catalog.setval('public.person_id_seq', 1, false);

SELECT pg_catalog.setval('public.resident_id_seq', 3, true);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id, version);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.person
    ADD CONSTRAINT person_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.resident
    ADD CONSTRAINT resident_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


