

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


CREATE TABLE hge_tests.address (
    id integer NOT NULL,
    door_no text NOT NULL,
    street text NOT NULL,
    city text NOT NULL,
    resident_id integer
);

CREATE SEQUENCE hge_tests.address_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE hge_tests.address_id_seq OWNED BY hge_tests.address.id;

CREATE VIEW hge_tests.address_view AS
 SELECT address.id,
    address.door_no,
    address.city,
    address.resident_id
   FROM hge_tests.address;

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

CREATE VIEW hge_tests.resident_view AS
 SELECT resident.id,
    resident.name
   FROM hge_tests.resident;

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

CREATE VIEW public.article_view AS
 SELECT article.id,
    article.title,
    article.content,
    article.author_id
   FROM public.article;

CREATE TABLE public.author (
    id integer NOT NULL,
    name text,
    remarks text
);

CREATE TABLE public.author_details (
    id integer NOT NULL,
    author_id integer,
    email text
);

CREATE SEQUENCE public.author_details_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_details_id_seq OWNED BY public.author_details.id;

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE VIEW public.author_view AS
 SELECT author.id,
    upper(author.name) AS name
   FROM public.author;

ALTER TABLE ONLY hge_tests.address ALTER COLUMN id SET DEFAULT nextval('hge_tests.address_id_seq'::regclass);

ALTER TABLE ONLY hge_tests.resident ALTER COLUMN id SET DEFAULT nextval('hge_tests.resident_id_seq'::regclass);

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.author_details ALTER COLUMN id SET DEFAULT nextval('public.author_details_id_seq'::regclass);

INSERT INTO hge_tests.address VALUES
	(1, '12-21', 'Madhapur', 'Hyderabad', 1),
	(2, '11-19', 'Koramangala', 'Bangalore', 1),
	(3, '13-21', 'Teynampet', 'Chennai', 2);

INSERT INTO hge_tests.resident VALUES
	(1, 'John', 23),
	(2, 'Tom', 31);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, NULL, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, NULL, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, NULL, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', NULL),
	(2, 'Author 2', NULL);

INSERT INTO public.author_details VALUES
	(1, 1, 'author1@authors.com'),
	(2, 2, 'author2@authors.com');

SELECT pg_catalog.setval('hge_tests.address_id_seq', 3, true);

SELECT pg_catalog.setval('hge_tests.resident_id_seq', 2, true);

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_details_id_seq', 2, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

ALTER TABLE ONLY hge_tests.address
    ADD CONSTRAINT address_pkey PRIMARY KEY (id);

ALTER TABLE ONLY hge_tests.resident
    ADD CONSTRAINT resident_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author_details
    ADD CONSTRAINT author_details_email_key UNIQUE (email);

ALTER TABLE ONLY public.author_details
    ADD CONSTRAINT author_details_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY hge_tests.address
    ADD CONSTRAINT address_resident_id_fkey FOREIGN KEY (resident_id) REFERENCES hge_tests.resident(id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

ALTER TABLE ONLY public.author_details
    ADD CONSTRAINT author_details_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


