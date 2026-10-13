

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


CREATE TABLE public.crust (
    name text NOT NULL
);

CREATE TABLE public.pizza (
    id integer NOT NULL,
    name text NOT NULL,
    crust text NOT NULL
);

CREATE SEQUENCE public.pizza_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.pizza_id_seq OWNED BY public.pizza.id;

ALTER TABLE ONLY public.pizza ALTER COLUMN id SET DEFAULT nextval('public.pizza_id_seq'::regclass);

INSERT INTO public.crust VALUES
	('thin');

INSERT INTO public.pizza VALUES
	(1, 'margarita', 'thin');

SELECT pg_catalog.setval('public.pizza_id_seq', 1, true);

ALTER TABLE ONLY public.crust
    ADD CONSTRAINT crust_pkey PRIMARY KEY (name);

ALTER TABLE ONLY public.pizza
    ADD CONSTRAINT pizza_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.pizza
    ADD CONSTRAINT pizza_crust_fkey FOREIGN KEY (crust) REFERENCES public.crust(name);


