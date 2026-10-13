

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
    is_published boolean DEFAULT false NOT NULL,
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

CREATE TABLE public.auction (
    id integer NOT NULL,
    price integer DEFAULT 250 NOT NULL,
    bid_price integer
);

CREATE SEQUENCE public.auction_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.auction_id_seq OWNED BY public.auction.id;

CREATE TABLE public.author (
    id integer NOT NULL,
    name text,
    is_registered boolean DEFAULT false NOT NULL,
    remarks_internal text
);

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE TABLE public.geog_table (
    id integer NOT NULL,
    name text NOT NULL,
    geog_col public.geography NOT NULL
);

CREATE SEQUENCE public.geog_table_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.geog_table_id_seq OWNED BY public.geog_table.id;

CREATE TABLE public.geom_table (
    id integer NOT NULL,
    type text NOT NULL,
    geom_col public.geometry NOT NULL
);

CREATE SEQUENCE public.geom_table_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.geom_table_id_seq OWNED BY public.geom_table.id;

CREATE TABLE public.jsonb_table (
    id integer NOT NULL,
    jsonb_col jsonb NOT NULL
);

CREATE SEQUENCE public.jsonb_table_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.jsonb_table_id_seq OWNED BY public.jsonb_table.id;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.auction ALTER COLUMN id SET DEFAULT nextval('public.auction_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.geog_table ALTER COLUMN id SET DEFAULT nextval('public.geog_table_id_seq'::regclass);

ALTER TABLE ONLY public.geom_table ALTER COLUMN id SET DEFAULT nextval('public.geom_table_id_seq'::regclass);

ALTER TABLE ONLY public.jsonb_table ALTER COLUMN id SET DEFAULT nextval('public.jsonb_table_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, false, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, true, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, true, NULL),
	(4, 'Article 4', 'Sample article content 4', 3, false, NULL);

INSERT INTO public.auction VALUES
	(1, 250, 100),
	(2, 250, 120),
	(3, 250, 300),
	(4, 250, 260);

INSERT INTO public.author VALUES
	(1, 'Author 1', false, 'remark 1'),
	(2, 'Author 2', false, 'remark 2'),
	(3, 'Author 3', false, 'remark 3');

INSERT INTO public.geog_table VALUES
	(1, 'London', '0101000020E6100000EBE2361AC05BC03FC5FEB27BF2C04940'),
	(2, 'Paris', '0101000020E6100000A835CD3B4ED1024076E09C11A56D4840'),
	(3, 'Moscow', '0101000020E610000010E9B7AF03CF42408D28ED0DBEE04B40'),
	(4, 'New York', '0101000020E6100000AAF1D24D628052C05E4BC8073D5B4440'),
	(5, 'Linestring', '0102000020E610000002000000000000000000000000000000000000000000000000000000000000000000F03F'),
	(6, 'Point', '0101000020E61000000000000000000000000000000000E03F');

INSERT INTO public.geom_table VALUES
	(1, 'point', '0101000020E6100000000000000000F03F0000000000000040'),
	(2, 'linestring', '0102000020E61000000400000000000000000000000000000000000000000000000000E03F000000000000F03F000000000000F03F0000000000000040000000000000F83F0000000000000840'),
	(3, 'linestring', '0102000020E610000003000000000000000000F03F0000000000000000000000000000E03F000000000000E03F0000000000000000000000000000F03F'),
	(4, 'polygon', '0103000020E61000000100000005000000000000000000000000000000000000000000000000000000000000000000F03F000000000000F03F000000000000F03F000000000000F03F000000000000000000000000000000000000000000000000'),
	(5, 'polygon', '0103000020E61000000100000005000000000000000000004000000000000000000000000000000040000000000000F03F0000000000000840000000000000F03F0000000000000840000000000000000000000000000000400000000000000000');

INSERT INTO public.jsonb_table VALUES
	(1, '{"age": 7, "name": "Hasura"}'),
	(2, '{"name": "Cross"}');

SELECT pg_catalog.setval('public.article_id_seq', 4, true);

SELECT pg_catalog.setval('public.auction_id_seq', 4, true);

SELECT pg_catalog.setval('public.author_id_seq', 3, true);

SELECT pg_catalog.setval('public.geog_table_id_seq', 6, true);

SELECT pg_catalog.setval('public.geom_table_id_seq', 5, true);

SELECT pg_catalog.setval('public.jsonb_table_id_seq', 2, true);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.auction
    ADD CONSTRAINT auction_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.geog_table
    ADD CONSTRAINT geog_table_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.geom_table
    ADD CONSTRAINT geom_table_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.jsonb_table
    ADD CONSTRAINT jsonb_table_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


