

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
    title text NOT NULL,
    content text,
    author_id integer NOT NULL
);

CREATE TABLE public.author (
    id integer NOT NULL,
    first_name text NOT NULL,
    last_name text
);

CREATE FUNCTION public.fetch_articles(search text, author_row public.author) RETURNS SETOF public.article
    LANGUAGE sql STABLE
    AS $$
  SELECT *
  FROM article
  WHERE
    ( title ilike ('%' || search || '%')
      OR content ilike ('%' || search || '%')
    ) AND author_id = author_row.id
$$;

CREATE FUNCTION public.fetch_articles_plain(author_row public.author) RETURNS SETOF public.article
    LANGUAGE sql STABLE
    AS $$
  SELECT *
  FROM article
  WHERE author_id = author_row.id
$$;

CREATE FUNCTION public.full_name(public.author) RETURNS text
    LANGUAGE plpgsql STABLE
    AS $_$
  DECLARE
    first_name text;
    last_name text;
    full_name text;
  BEGIN
    first_name := $1.first_name;
    last_name := $1.last_name;
    IF last_name IS NULL THEN
      full_name := first_name;
    ELSE full_name := first_name || ' ' || last_name;
    END IF;
    RETURN full_name;
  END;
$_$;

CREATE TABLE public.float_test (
    id integer NOT NULL,
    first_int integer NOT NULL,
    second_int integer NOT NULL
);

CREATE FUNCTION public.get_sum(table_row public.float_test) RETURNS double precision
    LANGUAGE sql STABLE
    AS $$
SELECT table_row.first_int::float8 + table_row.second_int::float8
$$;

CREATE FUNCTION public.get_sum_offset(table_row public.float_test, hasura_session json) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
SELECT table_row.first_int + table_row.second_int + (hasura_session ->> 'x-hasura-offset-int')::int
$$;

CREATE TABLE public.locations (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    location public.geometry(Point,4326)
);

CREATE FUNCTION public.locations_distance("from" json, locations_row public.locations) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
   SELECT ST_DISTANCE( locations_row.location::geography
                     , ST_GeomFromGeoJSON("from"::text)::geography
                     )::integer
 $$;

CREATE SEQUENCE public.article_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.article_id_seq OWNED BY public.article.id;

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE SEQUENCE public.float_test_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.float_test_id_seq OWNED BY public.float_test.id;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.float_test ALTER COLUMN id SET DEFAULT nextval('public.float_test_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Content for Article 1', 1),
	(2, 'Article 2', 'Content for Article 2', 2),
	(3, 'Article 3', 'Content for Article 3', 2);

INSERT INTO public.author VALUES
	(1, 'Roger', 'Chris'),
	(2, 'Daniel', NULL);

INSERT INTO public.float_test VALUES
	(1, 1, 2),
	(2, 3, 2),
	(3, 4, 6);

INSERT INTO public.locations VALUES
	('2d56b5bc-1d21-4fb0-a198-653e7391c213', '0101000020E610000000000000000000000000000000000000'),
	('8f85ec5b-7f65-43f8-91de-974a00bd877e', '0101000020E6100000000000000000F03F0000000000000000');

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.float_test_id_seq', 3, true);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.float_test
    ADD CONSTRAINT float_test_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


