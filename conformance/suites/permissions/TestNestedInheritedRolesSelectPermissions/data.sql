

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

SET SESSION AUTHORIZATION DEFAULT;

ALTER TABLE public.dummy_table DISABLE TRIGGER ALL;

INSERT INTO public.dummy_table VALUES
	(1, 'Dummy 1'),
	(2, 'Dummy 2'),
	(3, 'Dummy 3');

ALTER TABLE public.dummy_table ENABLE TRIGGER ALL;

ALTER TABLE public.users DISABLE TRIGGER ALL;

INSERT INTO public.users VALUES
	(1, 'User 1', 'Allowed Data 1'),
	(2, 'User 2', 'Allowed Data 2'),
	(3, 'User 3', 'Allowed Data 3');

ALTER TABLE public.users ENABLE TRIGGER ALL;


