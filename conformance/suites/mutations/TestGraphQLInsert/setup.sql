

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

CREATE TYPE public.complex AS (
	r double precision,
	i double precision
);

CREATE TYPE public.inventory_item AS (
	name text,
	supplier_id integer,
	price numeric
);

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

CREATE TABLE public.orders (
    id integer NOT NULL,
    placed timestamp with time zone NOT NULL,
    shipped timestamp with time zone
);

CREATE SEQUENCE public.orders_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.orders_id_seq OWNED BY public.orders.id;

CREATE TABLE public.person (
    id integer NOT NULL,
    details jsonb NOT NULL
);

CREATE VIEW public.person_const_view AS
 SELECT person.id,
    person.details
   FROM public.person
 LIMIT 600;

CREATE SEQUENCE public.person_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.person_id_seq OWNED BY public.person.id;

CREATE VIEW public.person_mut_view AS
 SELECT person.id,
    person.details
   FROM public.person;

CREATE TABLE public.test_types (
    c1_smallint smallint,
    c2_integer integer,
    c3_bigint bigint,
    c4_decimal numeric(5,2),
    c5_numeric numeric(4,3),
    c6_real real,
    c7_double_precision double precision,
    c8_smallserial smallint NOT NULL,
    c9_serial integer NOT NULL,
    c10_bigserial bigint NOT NULL,
    c11_varchar_3 character varying(3),
    c12_char_4 character(4),
    c13_text text,
    c14_timestamp timestamp without time zone,
    c15_timestamptz timestamp with time zone,
    c16_date date,
    c17_time time without time zone,
    c18_time_with_zone time with time zone,
    c19_interval interval,
    c20_boolean boolean,
    c21_point point,
    c22_line line,
    c23_lseg lseg,
    c24_box box,
    c25_closed_path path,
    c26_open_path path,
    c27_polygon polygon,
    c28_circle circle,
    c29_cidr cidr,
    c30_inet inet,
    c31_macaddr macaddr,
    c32_json json,
    c33_jsonb jsonb,
    c34_text_array text[],
    c35_integer_2d_array integer[],
    c36_uuid uuid,
    c37_composite_type_complex public.complex,
    c38_composite_type_inventory public.inventory_item,
    c39_range_integer int4range,
    c40_range_bigint int8range,
    c41_range_numeric numrange,
    c42_range_timestamp tsrange,
    c43_range_timestamptz tstzrange,
    c44_xml xml,
    c45_char character(1)
);

CREATE SEQUENCE public.test_types_c10_bigserial_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.test_types_c10_bigserial_seq OWNED BY public.test_types.c10_bigserial;

CREATE SEQUENCE public.test_types_c8_smallserial_seq
    AS smallint
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.test_types_c8_smallserial_seq OWNED BY public.test_types.c8_smallserial;

CREATE SEQUENCE public.test_types_c9_serial_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.test_types_c9_serial_seq OWNED BY public.test_types.c9_serial;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.orders ALTER COLUMN id SET DEFAULT nextval('public.orders_id_seq'::regclass);

ALTER TABLE ONLY public.person ALTER COLUMN id SET DEFAULT nextval('public.person_id_seq'::regclass);

ALTER TABLE ONLY public.test_types ALTER COLUMN c8_smallserial SET DEFAULT nextval('public.test_types_c8_smallserial_seq'::regclass);

ALTER TABLE ONLY public.test_types ALTER COLUMN c9_serial SET DEFAULT nextval('public.test_types_c9_serial_seq'::regclass);

ALTER TABLE ONLY public.test_types ALTER COLUMN c10_bigserial SET DEFAULT nextval('public.test_types_c10_bigserial_seq'::regclass);

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 1, false);

SELECT pg_catalog.setval('public.orders_id_seq', 1, false);

SELECT pg_catalog.setval('public.person_id_seq', 1, false);

SELECT pg_catalog.setval('public.test_types_c10_bigserial_seq', 1, false);

SELECT pg_catalog.setval('public.test_types_c8_smallserial_seq', 1, false);

SELECT pg_catalog.setval('public.test_types_c9_serial_seq', 1, false);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.person
    ADD CONSTRAINT person_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.test_types
    ADD CONSTRAINT test_types_pkey PRIMARY KEY (c8_smallserial);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);


