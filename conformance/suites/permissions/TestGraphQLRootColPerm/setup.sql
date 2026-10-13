

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


CREATE TABLE public.dml_rbp_grants (
    task_id integer NOT NULL,
    user_id integer NOT NULL,
    user_level integer NOT NULL
);

CREATE TABLE public.dml_rbp_projects (
    id integer NOT NULL,
    min_level integer NOT NULL,
    name text NOT NULL
);

CREATE SEQUENCE public.dml_rbp_projects_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.dml_rbp_projects_id_seq OWNED BY public.dml_rbp_projects.id;

CREATE TABLE public.dml_rbp_tasks (
    id integer NOT NULL,
    project_id integer NOT NULL,
    min_level integer NOT NULL,
    title text NOT NULL
);

CREATE SEQUENCE public.dml_rbp_tasks_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.dml_rbp_tasks_id_seq OWNED BY public.dml_rbp_tasks.id;

ALTER TABLE ONLY public.dml_rbp_projects ALTER COLUMN id SET DEFAULT nextval('public.dml_rbp_projects_id_seq'::regclass);

ALTER TABLE ONLY public.dml_rbp_tasks ALTER COLUMN id SET DEFAULT nextval('public.dml_rbp_tasks_id_seq'::regclass);

INSERT INTO public.dml_rbp_grants VALUES
	(1, 1, 5),
	(2, 1, 5);

INSERT INTO public.dml_rbp_projects VALUES
	(1, 0, 'project-accessible'),
	(2, 0, 'project-restricted');

INSERT INTO public.dml_rbp_tasks VALUES
	(1, 1, 0, 'task-low'),
	(2, 2, 10, 'task-high');

SELECT pg_catalog.setval('public.dml_rbp_projects_id_seq', 2, true);

SELECT pg_catalog.setval('public.dml_rbp_tasks_id_seq', 2, true);

ALTER TABLE ONLY public.dml_rbp_projects
    ADD CONSTRAINT dml_rbp_projects_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.dml_rbp_tasks
    ADD CONSTRAINT dml_rbp_tasks_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.dml_rbp_grants
    ADD CONSTRAINT dml_rbp_grants_task_id_fkey FOREIGN KEY (task_id) REFERENCES public.dml_rbp_tasks(id);

ALTER TABLE ONLY public.dml_rbp_tasks
    ADD CONSTRAINT dml_rbp_tasks_project_id_fkey FOREIGN KEY (project_id) REFERENCES public.dml_rbp_projects(id);


