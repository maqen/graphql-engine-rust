

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

ALTER TABLE public.person DISABLE TRIGGER ALL;

INSERT INTO public.person VALUES
	(1, '{"name": {"last": "Taylor", "first": "John"}}'),
	(2, '[{"address": {"city": "Copenhagen", "country": "Denmark"}}, {"address": {"city": "Canterbury", "country": "United Kingdom"}}]'),
	(3, '{"name": {"last": "Wilson", "first": "Robert"}, "address": {"city": "Copenhagen", "country": "Denmark"}}');

ALTER TABLE public.person ENABLE TRIGGER ALL;

SELECT pg_catalog.setval('public.person_id_seq', 3, true);


