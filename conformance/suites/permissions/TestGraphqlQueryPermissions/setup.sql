

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

CREATE TABLE public.author (
    id integer NOT NULL,
    name text,
    is_registered boolean DEFAULT false NOT NULL,
    remarks_internal text
);

CREATE FUNCTION public.get_articles(author_row public.author) RETURNS SETOF public.article
    LANGUAGE sql STABLE
    AS $$
  SELECT * FROM article WHERE author_id = author_row.id
$$;

CREATE TABLE public.gpa (
    id integer NOT NULL,
    student_name text NOT NULL,
    gpa_score double precision NOT NULL
);

CREATE FUNCTION public.passed_students(pass_gpa double precision) RETURNS SETOF public.gpa
    LANGUAGE sql STABLE
    AS $$
  SELECT * FROM gpa WHERE
    gpa_score >= pass_gpa
$$;

CREATE TABLE public."Track" (
    id integer NOT NULL,
    name text NOT NULL,
    artist_id integer
);

CREATE FUNCTION public.search_tracks(search text) RETURNS SETOF public."Track"
    LANGUAGE sql STABLE
    AS $$
    select *
    from "Track"
    where
    name ilike ('%' || search || '%')
$$;

CREATE TABLE public.student_marks (
    id integer NOT NULL,
    name text NOT NULL,
    physics integer,
    chemistry integer,
    mathematics integer
);

CREATE FUNCTION public.student_total_marks(student_row public.student_marks) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
  SELECT student_row.physics + student_row.chemistry + student_row.mathematics
$$;

CREATE FUNCTION public.student_total_marks_offset(hasura_session json, student_row public.student_marks) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
  SELECT student_row.physics + student_row.chemistry + student_row.mathematics - (hasura_session ->> 'x-hasura-offset-marks')::integer
$$;

CREATE TABLE public."Artist" (
    id integer NOT NULL,
    name text NOT NULL
);

CREATE SEQUENCE public."Artist_id_seq"
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public."Artist_id_seq" OWNED BY public."Artist".id;

CREATE SEQUENCE public."Track_id_seq"
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public."Track_id_seq" OWNED BY public."Track".id;

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

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE TABLE public.books (
    id integer NOT NULL,
    author_name text,
    book_name text NOT NULL,
    published_on timestamp with time zone
);

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

CREATE SEQUENCE public.gpa_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.gpa_id_seq OWNED BY public.gpa.id;

CREATE TABLE public.jsonb_table (
    id integer NOT NULL,
    jsonb_col jsonb NOT NULL
);

CREATE SEQUENCE public.student_marks_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.student_marks_id_seq OWNED BY public.student_marks.id;

ALTER TABLE ONLY public."Artist" ALTER COLUMN id SET DEFAULT nextval('public."Artist_id_seq"'::regclass);

ALTER TABLE ONLY public."Track" ALTER COLUMN id SET DEFAULT nextval('public."Track_id_seq"'::regclass);

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.auction ALTER COLUMN id SET DEFAULT nextval('public.auction_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.geom_table ALTER COLUMN id SET DEFAULT nextval('public.geom_table_id_seq'::regclass);

ALTER TABLE ONLY public.gpa ALTER COLUMN id SET DEFAULT nextval('public.gpa_id_seq'::regclass);

ALTER TABLE ONLY public.student_marks ALTER COLUMN id SET DEFAULT nextval('public.student_marks_id_seq'::regclass);

INSERT INTO public."Artist" VALUES
	(1, 'Camilla'),
	(2, 'DSP'),
	(3, 'Akon');

INSERT INTO public."Track" VALUES
	(1, 'Keepup', 1),
	(2, 'Keepdown', 1),
	(3, 'Happy', 2);

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

INSERT INTO public.books VALUES
	(1, 'J.K. Rowling', 'Harry Porter', '1997-06-26 00:00:00+00');

INSERT INTO public.geom_table VALUES
	(1, 'point', '0101000020E6100000000000000000F03F0000000000000040'),
	(2, 'linestring', '0102000020E61000000400000000000000000000000000000000000000000000000000E03F000000000000F03F000000000000F03F0000000000000040000000000000F83F0000000000000840'),
	(3, 'linestring', '0102000020E610000003000000000000000000F03F0000000000000000000000000000E03F000000000000E03F0000000000000000000000000000F03F'),
	(4, 'polygon', '0103000020E61000000100000005000000000000000000000000000000000000000000000000000000000000000000F03F000000000000F03F000000000000F03F000000000000F03F000000000000000000000000000000000000000000000000'),
	(5, 'polygon', '0103000020E61000000100000005000000000000000000004000000000000000000000000000000040000000000000F03F0000000000000840000000000000F03F0000000000000840000000000000000000000000000000400000000000000000');

INSERT INTO public.gpa VALUES
	(1, 'clarke', 9.24),
	(2, 'george', 8.32),
	(3, 'blake', 7.34),
	(4, 'leonel', 9.56);

INSERT INTO public.jsonb_table VALUES
	(1, '{"age": 7, "name": "Hasura"}'),
	(2, '{"name": "Cross"}');

INSERT INTO public.student_marks VALUES
	(1, 'clarke', 84, 67, 70),
	(2, 'george', 56, 79, 70),
	(3, 'blake', 66, 89, 78),
	(4, 'leonel', 90, 93, 85);

SELECT pg_catalog.setval('public."Artist_id_seq"', 1, false);

SELECT pg_catalog.setval('public."Track_id_seq"', 1, false);

SELECT pg_catalog.setval('public.article_id_seq', 4, true);

SELECT pg_catalog.setval('public.auction_id_seq', 4, true);

SELECT pg_catalog.setval('public.author_id_seq', 3, true);

SELECT pg_catalog.setval('public.geom_table_id_seq', 5, true);

SELECT pg_catalog.setval('public.gpa_id_seq', 4, true);

SELECT pg_catalog.setval('public.student_marks_id_seq', 4, true);

ALTER TABLE ONLY public."Artist"
    ADD CONSTRAINT "Artist_pkey" PRIMARY KEY (id);

ALTER TABLE ONLY public."Track"
    ADD CONSTRAINT "Track_pkey" PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.auction
    ADD CONSTRAINT auction_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.books
    ADD CONSTRAINT books_pkey PRIMARY KEY (id, book_name);

ALTER TABLE ONLY public.geom_table
    ADD CONSTRAINT geom_table_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.gpa
    ADD CONSTRAINT gpa_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.jsonb_table
    ADD CONSTRAINT jsonb_table_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.student_marks
    ADD CONSTRAINT student_marks_name_key UNIQUE (name);

ALTER TABLE ONLY public.student_marks
    ADD CONSTRAINT student_marks_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public."Track"
    ADD CONSTRAINT "Track_artist_id_fkey" FOREIGN KEY (artist_id) REFERENCES public."Artist"(id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


