

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


CREATE TABLE public.article (
    id integer NOT NULL,
    title text,
    content text,
    author_id integer NOT NULL,
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
    emails text[] DEFAULT '{}'::text[] NOT NULL,
    info jsonb,
    location public.geography
);

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, NULL, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, NULL, NULL),
	(3, 'Article 3', 'Sample article content 3', 1, NULL, NULL),
	(4, 'Article 4', 'Sample article content 4', 2, NULL, NULL),
	(5, 'Article 5', 'Sample article content 5', 2, NULL, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', '{}', NULL, NULL),
	(2, 'Author 2', '{}', NULL, NULL),
	(3, 'Author 3', '{}', '{"age": 23}', '0101000020E610000075029A081B9A5DC0F085C954C1F84040');

SELECT pg_catalog.setval('public.article_id_seq', 5, true);

SELECT pg_catalog.setval('public.author_id_seq', 3, true);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


