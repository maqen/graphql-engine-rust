

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

ALTER TABLE public.author DISABLE TRIGGER ALL;

INSERT INTO public.author VALUES
	(1, 'Author 1'),
	(2, 'Author 2');

ALTER TABLE public.author ENABLE TRIGGER ALL;

ALTER TABLE public.article DISABLE TRIGGER ALL;

INSERT INTO public.article VALUES
	(1, 'Article 1', 'Sample article content', NULL, NULL, NULL),
	(2, 'Article 2', 'Sample article content', NULL, NULL, NULL),
	(3, 'Article 3', 'Sample article content', NULL, NULL, NULL);

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public.orders DISABLE TRIGGER ALL;

INSERT INTO public.orders VALUES
	(1, '2017-08-19 12:22:11.802755+00', NULL);

ALTER TABLE public.orders ENABLE TRIGGER ALL;

ALTER TABLE public.person DISABLE TRIGGER ALL;

ALTER TABLE public.person ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 3, true);

SELECT pg_catalog.setval('public.author_id_seq', 2, true);

SELECT pg_catalog.setval('public.orders_id_seq', 1, true);

SELECT pg_catalog.setval('public.person_id_seq', 1, false);


