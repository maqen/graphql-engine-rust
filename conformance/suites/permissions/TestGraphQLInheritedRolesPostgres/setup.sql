

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


CREATE TABLE public.employee (
    id integer NOT NULL,
    name text NOT NULL,
    salary integer NOT NULL,
    title text,
    manager_id integer,
    team_id integer,
    manager_remarks text,
    hr_remarks text
);

CREATE FUNCTION public.employee_yearly_salary(employee_row public.employee) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
SELECT employee_row.salary * 12
$$;

CREATE TABLE public.articles (
    id integer NOT NULL,
    title text,
    content text,
    is_published boolean DEFAULT false,
    author_id integer
);

CREATE SEQUENCE public.articles_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.articles_id_seq OWNED BY public.articles.id;

CREATE TABLE public.authors (
    id integer NOT NULL,
    name text,
    followers integer
);

CREATE SEQUENCE public.authors_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.authors_id_seq OWNED BY public.authors.id;

CREATE SEQUENCE public.employee_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.employee_id_seq OWNED BY public.employee.id;

CREATE TABLE public.manager (
    id integer NOT NULL,
    team_id integer,
    name text,
    phone text,
    address text
);

CREATE SEQUENCE public.manager_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.manager_id_seq OWNED BY public.manager.id;

CREATE TABLE public.team (
    id integer NOT NULL,
    name text
);

CREATE SEQUENCE public.team_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.team_id_seq OWNED BY public.team.id;

ALTER TABLE ONLY public.articles ALTER COLUMN id SET DEFAULT nextval('public.articles_id_seq'::regclass);

ALTER TABLE ONLY public.authors ALTER COLUMN id SET DEFAULT nextval('public.authors_id_seq'::regclass);

ALTER TABLE ONLY public.employee ALTER COLUMN id SET DEFAULT nextval('public.employee_id_seq'::regclass);

ALTER TABLE ONLY public.manager ALTER COLUMN id SET DEFAULT nextval('public.manager_id_seq'::regclass);

ALTER TABLE ONLY public.team ALTER COLUMN id SET DEFAULT nextval('public.team_id_seq'::regclass);

INSERT INTO public.articles VALUES
	(1, 'title 1', 'content 1', false, 1),
	(2, 'title 2', 'content 2', true, 2),
	(3, 'title 3', 'content 3', true, 1),
	(4, 'title 4', 'content 4', true, 3),
	(5, 'title 5', 'content 5', true, 2);

INSERT INTO public.authors VALUES
	(1, 'J.K.Rowling', 1232344),
	(2, 'Paulo Coelho', 21312332),
	(3, 'Murakami', 1232132);

INSERT INTO public.employee VALUES
	(1, 'employee 1', 10000, 'Sales representative', 1, 1, 'good', 'good'),
	(2, 'employee 2', 14000, 'Sales representative', 1, 1, 'good', 'good'),
	(3, 'employee 3', 12400, 'HR trainee', 3, 2, 'hard working', 'hard working'),
	(4, 'employee 4', 15430, 'Sales representative', 2, 1, 'hard working', 'hard working');

INSERT INTO public.manager VALUES
	(1, 1, 'sales manager 1', '9988998822', 'basant bihar'),
	(2, 1, 'sales manager 2', '9977223344', 'buena vista'),
	(3, 2, 'HR manager 1', '1122334455', 'MG Road');

INSERT INTO public.team VALUES
	(1, 'sales'),
	(2, 'HR');

SELECT pg_catalog.setval('public.articles_id_seq', 5, true);

SELECT pg_catalog.setval('public.authors_id_seq', 3, true);

SELECT pg_catalog.setval('public.employee_id_seq', 4, true);

SELECT pg_catalog.setval('public.manager_id_seq', 3, true);

SELECT pg_catalog.setval('public.team_id_seq', 2, true);

ALTER TABLE ONLY public.articles
    ADD CONSTRAINT articles_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.authors
    ADD CONSTRAINT authors_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.employee
    ADD CONSTRAINT employee_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.manager
    ADD CONSTRAINT manager_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.team
    ADD CONSTRAINT team_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.articles
    ADD CONSTRAINT articles_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.authors(id);

ALTER TABLE ONLY public.employee
    ADD CONSTRAINT employee_manager_id_fkey FOREIGN KEY (manager_id) REFERENCES public.manager(id);

ALTER TABLE ONLY public.employee
    ADD CONSTRAINT employee_team_id_fkey FOREIGN KEY (team_id) REFERENCES public.team(id);

ALTER TABLE ONLY public.manager
    ADD CONSTRAINT manager_team_id_fkey FOREIGN KEY (team_id) REFERENCES public.team(id);


