

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
	(1, 'Article 1', '1.0.0', 'Sample article 1, content version 1.0.0', 1, true, NULL),
	(2, 'Article 1', '1.0.1', 'Sample article 1, content version 1.0.1', 1, false, NULL),
	(3, 'Article 2', '1.0.0', 'Sample article 2, content version 1.0.2', 2, false, NULL);

ALTER TABLE public.article ENABLE TRIGGER ALL;

ALTER TABLE public.person DISABLE TRIGGER ALL;

ALTER TABLE public.person ENABLE TRIGGER ALL;

ALTER TABLE public.resident DISABLE TRIGGER ALL;

INSERT INTO public.resident VALUES
	(1, 'phillips', 25, 'canberra'),
	(2, 'george', 26, 'sydney'),
	(3, 'clarke', 21, 'perth');

ALTER TABLE public.resident ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.article_id_seq', 1, false);

SELECT pg_catalog.setval('public.author_id_seq', 1, false);

SELECT pg_catalog.setval('public.person_id_seq', 1, false);

SELECT pg_catalog.setval('public.resident_id_seq', 3, true);


