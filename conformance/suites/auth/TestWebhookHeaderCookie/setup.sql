

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

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, false, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, true, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, true, NULL),
	(4, 'Article 4', 'Sample article content 4', 3, false, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', false, 'remark 1'),
	(2, 'Author 2', false, 'remark 2'),
	(3, 'Author 3', false, 'remark 3');

SELECT pg_catalog.setval('public.article_id_seq', 4, true);

SELECT pg_catalog.setval('public.author_id_seq', 3, true);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


