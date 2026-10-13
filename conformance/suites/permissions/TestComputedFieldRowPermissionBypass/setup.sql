

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


CREATE TABLE public.documents (
    id integer NOT NULL,
    title text NOT NULL,
    body text,
    owner_id integer NOT NULL,
    is_private boolean DEFAULT false NOT NULL
);

CREATE TABLE public.users (
    id integer NOT NULL,
    name text NOT NULL
);

CREATE FUNCTION public.user_all_documents(user_row public.users) RETURNS SETOF public.documents
    LANGUAGE sql STABLE
    AS $$
  SELECT * FROM documents
$$;

CREATE SEQUENCE public.documents_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.documents_id_seq OWNED BY public.documents.id;

CREATE SEQUENCE public.users_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.users_id_seq OWNED BY public.users.id;

ALTER TABLE ONLY public.documents ALTER COLUMN id SET DEFAULT nextval('public.documents_id_seq'::regclass);

ALTER TABLE ONLY public.users ALTER COLUMN id SET DEFAULT nextval('public.users_id_seq'::regclass);

INSERT INTO public.documents VALUES
	(1, 'Alice Public Doc', 'public content', 1, false),
	(2, 'Alice Private Doc', 'alice secret content', 1, true),
	(3, 'Admin Secret Doc', 'admin secret content', 2, true),
	(4, 'Admin Public Doc', 'bob public content', 2, false);

INSERT INTO public.users VALUES
	(1, 'alice'),
	(2, 'bob_admin');

SELECT pg_catalog.setval('public.documents_id_seq', 4, true);

SELECT pg_catalog.setval('public.users_id_seq', 2, true);

ALTER TABLE ONLY public.documents
    ADD CONSTRAINT documents_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.documents
    ADD CONSTRAINT documents_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES public.users(id);


