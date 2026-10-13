

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
    content text,
    author_id integer,
    is_published boolean,
    published_on timestamp without time zone
);

CREATE TABLE public.author (
    id integer NOT NULL,
    name text,
    contact_id integer
);

CREATE FUNCTION public.get_articles(hasura_session json, author_row public.author) RETURNS SETOF public.article
    LANGUAGE sql STABLE
    AS $$
  SELECT *
  FROM article
  WHERE author_id = author_row.id AND
    ( title ilike  ('%' || (hasura_session ->> 'x-hasura-search') || '%')
      OR content  ilike  ('%' || (hasura_session ->> 'x-hasura-search') || '%')
    )
$$;

CREATE TABLE public."Track" (
    track_id integer NOT NULL,
    name text NOT NULL,
    album_id integer,
    milliseconds integer NOT NULL,
    bytes integer NOT NULL
);

CREATE FUNCTION public.track_size_mb(track_row public."Track") RETURNS integer
    LANGUAGE sql STABLE
    AS $$
  SELECT (track_row.bytes / 1048576)
$$;

CREATE TABLE public."Album" (
    album_id integer NOT NULL,
    title text NOT NULL
);

CREATE TABLE public."Tag" (
    id integer NOT NULL,
    name text NOT NULL,
    album_id integer NOT NULL
);

CREATE SEQUENCE public."Tag_id_seq"
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public."Tag_id_seq" OWNED BY public."Tag".id;

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

CREATE TABLE public.contact (
    id integer NOT NULL,
    phone integer,
    address text
);

CREATE SEQUENCE public.contact_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.contact_id_seq OWNED BY public.contact.id;

CREATE TABLE public.employee (
    id integer NOT NULL,
    name text NOT NULL,
    department text,
    salary integer
);

CREATE SEQUENCE public.employee_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.employee_id_seq OWNED BY public.employee.id;

ALTER TABLE ONLY public."Tag" ALTER COLUMN id SET DEFAULT nextval('public."Tag_id_seq"'::regclass);

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.contact ALTER COLUMN id SET DEFAULT nextval('public.contact_id_seq'::regclass);

ALTER TABLE ONLY public.employee ALTER COLUMN id SET DEFAULT nextval('public.employee_id_seq'::regclass);

INSERT INTO public."Album" VALUES
	(1, 'Big Ones'),
	(2, 'Face Lift');

INSERT INTO public."Tag" VALUES
	(1, 'Rock', 1),
	(2, 'Folk', 1),
	(3, 'Hip Hop', 2);

INSERT INTO public."Track" VALUES
	(1, 'Restless', 1, 123654, 9836284),
	(2, 'Keepup', 1, 637483, 6382913),
	(3, 'Havana', 1, 234512, 986384),
	(4, 'Evil Walks', 2, 437294, 6284302),
	(5, 'Random', 2, 1094732, 6032547),
	(6, 'Good One', 2, 346208, 6732987),
	(7, 'Mistress', 2, 420985, 7521946);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, false, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, true, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, true, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', 2),
	(2, 'Author 2', 1);

INSERT INTO public.contact VALUES
	(1, 1234567890, NULL),
	(2, 1234567891, NULL);

INSERT INTO public.employee VALUES
	(1, 'Kai', 'Engineering', 2345),
	(2, 'Zara', 'Services', 1234),
	(3, 'Kelsy', 'Services', 2134),
	(4, 'Damien', 'Product', 3124),
	(5, 'Kamila', 'Engineering', 4325),
	(6, 'Dara', 'Product', 1209),
	(7, 'Rickard', 'Services', 2223),
	(8, 'Bent', 'Engineering', 4122);

SELECT pg_catalog.setval('public."Tag_id_seq"', 3, true);

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.contact_id_seq', 2, true);

SELECT pg_catalog.setval('public.employee_id_seq', 8, true);

ALTER TABLE ONLY public."Album"
    ADD CONSTRAINT "Album_pkey" PRIMARY KEY (album_id);

ALTER TABLE ONLY public."Tag"
    ADD CONSTRAINT "Tag_pkey" PRIMARY KEY (id);

ALTER TABLE ONLY public."Track"
    ADD CONSTRAINT "Track_pkey" PRIMARY KEY (track_id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.contact
    ADD CONSTRAINT contact_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.employee
    ADD CONSTRAINT employee_name_key UNIQUE (name);

ALTER TABLE ONLY public.employee
    ADD CONSTRAINT employee_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public."Tag"
    ADD CONSTRAINT "Tag_album_id_fkey" FOREIGN KEY (album_id) REFERENCES public."Album"(album_id);

ALTER TABLE ONLY public."Track"
    ADD CONSTRAINT "Track_album_id_fkey" FOREIGN KEY (album_id) REFERENCES public."Album"(album_id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_contact_id_fkey FOREIGN KEY (contact_id) REFERENCES public.contact(id);


