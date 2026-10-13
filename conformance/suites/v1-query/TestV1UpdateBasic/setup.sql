

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

CREATE TABLE public.products (
    product_id integer NOT NULL,
    name text,
    price numeric DEFAULT 10.00
);

CREATE SEQUENCE public.products_product_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.products_product_id_seq OWNED BY public.products.product_id;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.person ALTER COLUMN id SET DEFAULT nextval('public.person_id_seq'::regclass);

ALTER TABLE ONLY public.products ALTER COLUMN product_id SET DEFAULT nextval('public.products_product_id_seq'::regclass);

INSERT INTO public.author VALUES
	(1, 'Author 1'),
	(2, 'Author 2');

INSERT INTO public.person VALUES
	(1, '{"name": {"last": "bar", "first": "foo"}, "address": "foobar"}');

INSERT INTO public.products VALUES
	(1, 'Product 1', 5),
	(2, 'Product 2', 15);

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.person_id_seq', 1, true);

SELECT pg_catalog.setval('public.products_product_id_seq', 2, true);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.person
    ADD CONSTRAINT person_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.products
    ADD CONSTRAINT products_pkey PRIMARY KEY (product_id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


