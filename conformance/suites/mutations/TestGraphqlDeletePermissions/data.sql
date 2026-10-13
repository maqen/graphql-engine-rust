

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

ALTER TABLE public.account DISABLE TRIGGER ALL;

INSERT INTO public.account VALUES
	(1, 1),
	(2, 2);

ALTER TABLE public.account ENABLE TRIGGER ALL;

ALTER TABLE public.author DISABLE TRIGGER ALL;

INSERT INTO public.author VALUES
	(1, 'Author 1', false, NULL),
	(2, 'Author 2', false, NULL),
	(3, 'Author 3', true, NULL),
	(4, 'Author 4', true, NULL),
	(5, 'Author 5', true, NULL);

ALTER TABLE public.author ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content 1', 1, NULL, NULL),
	(2, 'Article 2', 'Sample article content 2', 1, NULL, NULL),
	(3, 'Article 3', 'Sample article content 3', 1, NULL, NULL),
	(4, 'Article 4', 'Sample article content 4', 2, NULL, NULL),
	(5, 'Article 5', 'Sample article content 5', 2, NULL, NULL);

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public.resident DISABLE TRIGGER ALL;

INSERT INTO public.resident VALUES
	(1, 'Griffin', 25),
	(2, 'Clarke', 26);

ALTER TABLE public.resident ENABLE TRIGGER ALL;

ALTER TABLE public."user" DISABLE TRIGGER ALL;

INSERT INTO public."user" VALUES
	(1, 'user_1', false),
	(2, 'user_2', true);

ALTER TABLE public."user" ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.account_id_seq', 2, true);

SELECT pg_catalog.setval('public.article_id_seq', 5, true);

SELECT pg_catalog.setval('public.author_id_seq', 5, true);

SELECT pg_catalog.setval('public.resident_id_seq', 2, true);

SELECT pg_catalog.setval('public.user_id_seq', 2, true);


