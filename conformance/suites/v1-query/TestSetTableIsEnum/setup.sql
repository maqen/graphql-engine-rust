

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

CREATE TABLE public.employees (
    id integer NOT NULL,
    name text NOT NULL,
    favorite_color text NOT NULL,
    gets_paid_on text NOT NULL
);

CREATE SEQUENCE public.employees_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.employees_id_seq OWNED BY public.employees.id;

CREATE TABLE public.weekdays (
    value text NOT NULL
);

ALTER TABLE ONLY public.employees ALTER COLUMN id SET DEFAULT nextval('public.employees_id_seq'::regclass);

INSERT INTO public.colors VALUES
	('red', '#FF0000'),
	('green', '#00FF00'),
	('blue', '#0000FF'),
	('orange', '#FFFF00'),
	('yellow', '#00FFFF'),
	('purple', '#FF00FF');

INSERT INTO public.employees VALUES
	(1, 'Alyssa', 'red', 'monday'),
	(2, 'Ben', 'blue', 'friday');

INSERT INTO public.weekdays VALUES
	('sunday'),
	('monday'),
	('tuesday'),
	('wednesday'),
	('thursday'),
	('friday'),
	('saturday');

SELECT pg_catalog.setval('public.employees_id_seq', 2, true);

ALTER TABLE ONLY public.colors
    ADD CONSTRAINT colors_pkey PRIMARY KEY (value);

ALTER TABLE ONLY public.employees
    ADD CONSTRAINT employees_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.weekdays
    ADD CONSTRAINT weekdays_pkey PRIMARY KEY (value);

ALTER TABLE ONLY public.employees
    ADD CONSTRAINT employees_favorite_color_fkey FOREIGN KEY (favorite_color) REFERENCES public.colors(value);

ALTER TABLE ONLY public.employees
    ADD CONSTRAINT employees_gets_paid_on_fkey FOREIGN KEY (gets_paid_on) REFERENCES public.weekdays(value);


