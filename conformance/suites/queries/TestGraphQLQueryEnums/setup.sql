

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


CREATE TABLE public.colors (
    value text NOT NULL,
    comment text
);

CREATE TABLE public.country (
    value text NOT NULL,
    comment text
);

CREATE TABLE public.users (
    id integer NOT NULL,
    name text NOT NULL,
    favorite_color text NOT NULL
);

CREATE SEQUENCE public.users_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.users_id_seq OWNED BY public.users.id;

CREATE TABLE public.zones (
    id integer NOT NULL,
    code text NOT NULL,
    country text NOT NULL
);

CREATE SEQUENCE public.zones_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.zones_id_seq OWNED BY public.zones.id;

ALTER TABLE ONLY public.users ALTER COLUMN id SET DEFAULT nextval('public.users_id_seq'::regclass);

ALTER TABLE ONLY public.zones ALTER COLUMN id SET DEFAULT nextval('public.zones_id_seq'::regclass);

INSERT INTO public.colors VALUES
	('red', '#FF0000'),
	('green', '#00FF00'),
	('blue', '#0000FF'),
	('orange', '#FFFF00'),
	('yellow', '#00FFFF'),
	('purple', '#FF00FF');

INSERT INTO public.country VALUES
	('India', 'Republic of India'),
	('USA', 'United States of America');

INSERT INTO public.users VALUES
	(1, 'Alyssa', 'red'),
	(2, 'Ben', 'blue');

INSERT INTO public.zones VALUES
	(1, '67432', 'USA'),
	(2, '600036', 'India');

SELECT pg_catalog.setval('public.users_id_seq', 2, true);

SELECT pg_catalog.setval('public.zones_id_seq', 2, true);

ALTER TABLE ONLY public.colors
    ADD CONSTRAINT colors_pkey PRIMARY KEY (value);

ALTER TABLE ONLY public.country
    ADD CONSTRAINT country_pkey PRIMARY KEY (value);

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.zones
    ADD CONSTRAINT zones_pkey PRIMARY KEY (code, country);

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_favorite_color_fkey FOREIGN KEY (favorite_color) REFERENCES public.colors(value);

ALTER TABLE ONLY public.zones
    ADD CONSTRAINT zones_country_fkey FOREIGN KEY (country) REFERENCES public.country(value);


