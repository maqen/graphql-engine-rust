CREATE TABLE public.qv_author (id serial PRIMARY KEY, name text, info jsonb);
CREATE TABLE public.qv_article (id serial PRIMARY KEY, author_id int REFERENCES public.qv_author(id), title text);
