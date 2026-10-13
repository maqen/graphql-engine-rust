

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

CREATE TABLE public.article_multi (
    id integer NOT NULL,
    title text,
    content text,
    author_id1 integer,
    author_id2 integer,
    is_published boolean,
    published_on timestamp without time zone
);

CREATE SEQUENCE public.article_multi_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.article_multi_id_seq OWNED BY public.article_multi.id;

CREATE TABLE public.author (
    id integer NOT NULL,
    name text,
    "createdAt" timestamp without time zone
);

CREATE SEQUENCE public.author_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_id_seq OWNED BY public.author.id;

CREATE TABLE public.author_multi (
    id1 integer NOT NULL,
    id2 integer NOT NULL,
    name text,
    "createdAt" timestamp without time zone
);

CREATE SEQUENCE public.author_multi_id1_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_multi_id1_seq OWNED BY public.author_multi.id1;

CREATE SEQUENCE public.author_multi_id2_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.author_multi_id2_seq OWNED BY public.author_multi.id2;

CREATE TABLE public.person (
    id integer NOT NULL,
    name public.citext
);

CREATE SEQUENCE public.person_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.person_id_seq OWNED BY public.person.id;

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
    c45_money money,
    c46_ltree public.ltree
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

CREATE TABLE public."user" (
    id bigint NOT NULL,
    name text NOT NULL,
    number bigint
);

CREATE SEQUENCE public.user_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.user_id_seq OWNED BY public."user".id;

ALTER TABLE ONLY public.article ALTER COLUMN id SET DEFAULT nextval('public.article_id_seq'::regclass);

ALTER TABLE ONLY public.article_multi ALTER COLUMN id SET DEFAULT nextval('public.article_multi_id_seq'::regclass);

ALTER TABLE ONLY public.author ALTER COLUMN id SET DEFAULT nextval('public.author_id_seq'::regclass);

ALTER TABLE ONLY public.author_multi ALTER COLUMN id1 SET DEFAULT nextval('public.author_multi_id1_seq'::regclass);

ALTER TABLE ONLY public.author_multi ALTER COLUMN id2 SET DEFAULT nextval('public.author_multi_id2_seq'::regclass);

ALTER TABLE ONLY public.person ALTER COLUMN id SET DEFAULT nextval('public.person_id_seq'::regclass);

ALTER TABLE ONLY public.test_types ALTER COLUMN c8_smallserial SET DEFAULT nextval('public.test_types_c8_smallserial_seq'::regclass);

ALTER TABLE ONLY public.test_types ALTER COLUMN c9_serial SET DEFAULT nextval('public.test_types_c9_serial_seq'::regclass);

ALTER TABLE ONLY public.test_types ALTER COLUMN c10_bigserial SET DEFAULT nextval('public.test_types_c10_bigserial_seq'::regclass);

ALTER TABLE ONLY public."user" ALTER COLUMN id SET DEFAULT nextval('public.user_id_seq'::regclass);

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, false, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, true, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, true, NULL);

INSERT INTO public.article_multi VALUES
	(1, 'Article 1', 'Sample article content 1', 1, 1, false, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, 1, true, NULL),
	(3, 'Article 3', 'Sample article content 3', 2, 2, true, NULL);

INSERT INTO public.author VALUES
	(1, 'Author 1', '2017-09-21 09:39:44'),
	(2, 'Author 2', '2017-09-21 09:50:44');

INSERT INTO public.author_multi VALUES
	(1, 1, 'Author 1', '2017-09-21 09:39:44'),
	(2, 2, 'Author 2', '2017-09-21 09:50:44');

INSERT INTO public.person VALUES
	(1, 'John\'),
	(2, 'Clarke'),
	(3, 'clarke');

INSERT INTO public.test_types VALUES
	(32767, 2147483647, 9223372036854775807, 123.45, 1.234, 0.00390625, 16.0001220703125, 1, 1, 1, 'abc', 'baaz', 'foo bar baz', '2004-10-19 10:23:54', '2015-10-17 14:42:43+00', '2014-09-14', '11:09:23', '15:22:23+00', '01:03:02', true, '(1,2)', '{2,3,-1}', '[(4,2),(3,1)]', '(31,12),(14,11)', '((0,0),(0,3),(1,0))', '[(0,0),(0,-1),(-3,0)]', '((0,0),(0,6),(2,0))', '<(-2,-3),3>', '192.168.100.128/25', '198.24.10.0', '08:00:2b:01:02:03', '{ "a" : "b", "obj": { "c1": "c2" }, "arr": [1, 2, 3], "_underscore": 0, "!@#$%^": "special", "translations": { "hello world!": "hi" }, "objs": [{ "你好": "Hello!" }] }', '{"c": "d", "arr": [4, 5, 6], "obj": {"e1": "e2"}, "objs": [{"你好": "Hello!"}], "!@#$%^": "special", "_underscore": 0, "translations": {"hello world!": "hi"}}', '{a,b,c}', '{{4,5,6},{7,8,9}}', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '(1.23,-3.456)', '("fuzzy dice",42,1.99)', '[123,456)', '[1147483647,2147483647)', '[1.23,4.56]', '["2010-01-01 14:30:00","2010-01-01 15:30:02")', '("2011-02-05 12:03:00+00","2012-03-04 16:40:04+00"]', '<foo>bar</foo>', '$123.45', 'A.B.C');

INSERT INTO public."user" VALUES
	(1, 'User 1', 123456789),
	(2, 'User 2', 123456780);

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.article_multi_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.author_multi_id1_seq', 2, true);

SELECT pg_catalog.setval('public.author_multi_id2_seq', 2, true);

SELECT pg_catalog.setval('public.person_id_seq', 3, true);

SELECT pg_catalog.setval('public.test_types_c10_bigserial_seq', 1, true);

SELECT pg_catalog.setval('public.test_types_c8_smallserial_seq', 1, true);

SELECT pg_catalog.setval('public.test_types_c9_serial_seq', 1, true);

SELECT pg_catalog.setval('public.user_id_seq', 2, true);

ALTER TABLE ONLY public.article_multi
    ADD CONSTRAINT article_multi_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.author_multi
    ADD CONSTRAINT author_multi_name_key UNIQUE (name);

ALTER TABLE ONLY public.author_multi
    ADD CONSTRAINT author_multi_pkey PRIMARY KEY (id1, id2);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_name_key UNIQUE (name);

ALTER TABLE ONLY public.author
    ADD CONSTRAINT author_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.person
    ADD CONSTRAINT person_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public."user"
    ADD CONSTRAINT user_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.article
    ADD CONSTRAINT article_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.author(id);

ALTER TABLE ONLY public.article_multi
    ADD CONSTRAINT article_multi_author_id1_author_id2_fkey FOREIGN KEY (author_id1, author_id2) REFERENCES public.author_multi(id1, id2);


