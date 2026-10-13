

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

ALTER TABLE public.colors DISABLE TRIGGER ALL;

INSERT INTO public.colors VALUES
	('red', '#FF0000'),
	('green', '#00FF00'),
	('blue', '#0000FF'),
	('orange', '#FFFF00'),
	('yellow', '#00FFFF'),
	('purple', '#FF00FF');

ALTER TABLE public.colors ENABLE TRIGGER ALL;

ALTER TABLE public.posts DISABLE TRIGGER ALL;

ALTER TABLE public.posts ENABLE TRIGGER ALL;

ALTER TABLE public.users DISABLE TRIGGER ALL;

INSERT INTO public.users VALUES
	(1, 'Alyssa', 'red'),
	(2, 'Ben', 'blue');

ALTER TABLE public.users ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.posts_id_seq', 1, false);

SELECT pg_catalog.setval('public.users_id_seq', 2, true);


