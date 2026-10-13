

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
    bio text,
    is_registered boolean DEFAULT false NOT NULL
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

CREATE TABLE public."Company" (
    id integer NOT NULL,
    name text
);

CREATE SEQUENCE public."Company_id_seq"
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public."Company_id_seq" OWNED BY public."Company".id;

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

CREATE TABLE public.address (
    id integer NOT NULL,
    door_no text NOT NULL,
    street text NOT NULL,
    city text NOT NULL,
    resident_id integer
);

CREATE SEQUENCE public.address_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.address_id_seq OWNED BY public.address.id;

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

CREATE TABLE public.blog (
    id integer NOT NULL,
    title text NOT NULL,
    content text,
    author_id integer,
    last_updated timestamp with time zone,
    updated_by integer
);

CREATE SEQUENCE public.blog_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.blog_id_seq OWNED BY public.blog.id;

CREATE TABLE public.computer (
    id integer NOT NULL,
    name text NOT NULL,
    spec jsonb NOT NULL
);

CREATE SEQUENCE public.computer_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.computer_id_seq OWNED BY public.computer.id;

CREATE TABLE public.items (
    id integer NOT NULL,
    name text,
    item_quantity integer
);

CREATE SEQUENCE public.items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.items_id_seq OWNED BY public.items.id;

CREATE TABLE public.leads (
    id integer NOT NULL,
    name text NOT NULL,
    added_by text NOT NULL
);

CREATE SEQUENCE public.leads_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.leads_id_seq OWNED BY public.leads.id;

CREATE TABLE public.order_cart (
    id integer NOT NULL,
    item_id integer NOT NULL,
    order_cart_quantity integer
);

CREATE SEQUENCE public.order_cart_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.order_cart_id_seq OWNED BY public.order_cart.id;

CREATE TABLE public.resident (
    id integer NOT NULL,
    name text NOT NULL,
    age integer NOT NULL,
    is_user boolean DEFAULT false NOT NULL
);

CREATE SEQUENCE public.resident_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.resident_id_seq OWNED BY public.resident.id;

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

ALTER TABLE ONLY public."Company" ALTER COLUMN id SET DEFAULT nextval('public."Company_id_seq"'::regclass);

ALTER TABLE ONLY public.account ALTER COLUMN id SET DEFAULT nextval('public.account_id_seq'::regclass);

ALTER TABLE ONLY public.address ALTER COLUMN id SET DEFAULT nextval('public.address_id_seq'::regclass);

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.blog ALTER COLUMN id SET DEFAULT nextval('public.blog_id_seq'::regclass);

ALTER TABLE ONLY public.computer ALTER COLUMN id SET DEFAULT nextval('public.computer_id_seq'::regclass);

ALTER TABLE ONLY public.items ALTER COLUMN id SET DEFAULT nextval('public.items_id_seq'::regclass);

ALTER TABLE ONLY public.leads ALTER COLUMN id SET DEFAULT nextval('public.leads_id_seq'::regclass);

ALTER TABLE ONLY public.order_cart ALTER COLUMN id SET DEFAULT nextval('public.order_cart_id_seq'::regclass);

ALTER TABLE ONLY public.resident ALTER COLUMN id SET DEFAULT nextval('public.resident_id_seq'::regclass);

ALTER TABLE ONLY public."user" ALTER COLUMN id SET DEFAULT nextval('public.user_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content', 1, NULL, NULL),
	(2, 'Article 2', 'Sample article content', 1, NULL, NULL),
	(3, 'Article 3', 'Sample article content', 2, NULL, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', NULL, false),
	(2, 'Author 2', NULL, false);

INSERT INTO public.blog VALUES
	(1, 'first blog', NULL, 1, NULL, NULL),
	(2, 'second blog', NULL, 2, NULL, NULL);

INSERT INTO public.items VALUES
	(1, 'Apsara Pencil', 10),
	(2, 'Reynolds Trimax', 1323);

INSERT INTO public.resident VALUES
	(5, 'Resident 5', 21, false),
	(6, 'Resident 6', 22, false);

INSERT INTO public."user" VALUES
	(1, 'user_1', false),
	(2, 'user_2', true);

SELECT pg_catalog.setval('public."Company_id_seq"', 1, false);

SELECT pg_catalog.setval('public.account_id_seq', 1, false);

SELECT pg_catalog.setval('public.address_id_seq', 1, false);

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.blog_id_seq', 1, false);

SELECT pg_catalog.setval('public.computer_id_seq', 1, false);

SELECT pg_catalog.setval('public.items_id_seq', 2, true);

SELECT pg_catalog.setval('public.leads_id_seq', 1, false);

SELECT pg_catalog.setval('public.order_cart_id_seq', 1, false);

SELECT pg_catalog.setval('public.resident_id_seq', 1, false);

SELECT pg_catalog.setval('public.user_id_seq', 2, true);

ALTER TABLE ONLY public."Company"
    ADD CONSTRAINT "Company_pkey" PRIMARY KEY (id);

ALTER TABLE ONLY public.account
    ADD CONSTRAINT account_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.address
    ADD CONSTRAINT address_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.blog
    ADD CONSTRAINT blog_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.computer
    ADD CONSTRAINT computer_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.items
    ADD CONSTRAINT items_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.leads
    ADD CONSTRAINT leads_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.order_cart
    ADD CONSTRAINT order_cart_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.resident
    ADD CONSTRAINT resident_name_key UNIQUE (name);

ALTER TABLE ONLY public.resident
    ADD CONSTRAINT resident_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_name_key UNIQUE (name);

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.address
    ADD CONSTRAINT address_resident_id_fkey FOREIGN KEY (resident_id) REFERENCES public.resident(id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

ALTER TABLE ONLY public.blog
    ADD CONSTRAINT blog_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

ALTER TABLE ONLY public.blog
    ADD CONSTRAINT blog_updated_by_fkey FOREIGN KEY (updated_by) REFERENCES public.author(id);

ALTER TABLE ONLY public.order_cart
    ADD CONSTRAINT order_cart_item_id_fkey FOREIGN KEY (item_id) REFERENCES public.items(id);


