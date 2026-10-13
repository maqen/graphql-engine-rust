-- Chinook-style music store schema used by the benchmark and parity suite.

CREATE TABLE public.artist (
  artist_id   integer PRIMARY KEY,
  name        text NOT NULL
);

CREATE TABLE public.genre (
  genre_id    integer PRIMARY KEY,
  name        text NOT NULL
);

CREATE TABLE public.media_type (
  media_type_id integer PRIMARY KEY,
  name          text NOT NULL
);

CREATE TABLE public.album (
  album_id    integer PRIMARY KEY,
  title       text NOT NULL,
  artist_id   integer NOT NULL REFERENCES public.artist (artist_id),
  released_on date
);
CREATE INDEX album_artist_id_idx ON public.album (artist_id);

CREATE TABLE public.track (
  track_id      integer PRIMARY KEY,
  name          text NOT NULL,
  album_id      integer NOT NULL REFERENCES public.album (album_id),
  genre_id      integer NOT NULL REFERENCES public.genre (genre_id),
  media_type_id integer NOT NULL REFERENCES public.media_type (media_type_id),
  composer      text,
  milliseconds  integer NOT NULL,
  bytes         integer NOT NULL,
  unit_price    numeric(10, 2) NOT NULL
);
CREATE INDEX track_album_id_idx ON public.track (album_id);
CREATE INDEX track_genre_id_idx ON public.track (genre_id);

CREATE TABLE public.employee (
  employee_id integer PRIMARY KEY,
  first_name  text NOT NULL,
  last_name   text NOT NULL,
  title       text,
  reports_to  integer REFERENCES public.employee (employee_id),
  email       text NOT NULL
);

CREATE TABLE public.customer (
  customer_id    integer PRIMARY KEY,
  first_name     text NOT NULL,
  last_name      text NOT NULL,
  email          text NOT NULL,
  country        text NOT NULL,
  support_rep_id integer REFERENCES public.employee (employee_id),
  metadata       jsonb NOT NULL DEFAULT '{}'::jsonb,
  updated_at     timestamptz NOT NULL DEFAULT '2024-01-01T00:00:00Z'
);
CREATE INDEX customer_support_rep_id_idx ON public.customer (support_rep_id);

CREATE TABLE public.invoice (
  invoice_id    integer PRIMARY KEY,
  customer_id   integer NOT NULL REFERENCES public.customer (customer_id),
  invoice_date  timestamptz NOT NULL,
  billing_country text NOT NULL,
  total         numeric(10, 2) NOT NULL
);
CREATE INDEX invoice_customer_id_idx ON public.invoice (customer_id);

CREATE TABLE public.invoice_line (
  invoice_line_id integer PRIMARY KEY,
  invoice_id      integer NOT NULL REFERENCES public.invoice (invoice_id),
  track_id        integer NOT NULL REFERENCES public.track (track_id),
  unit_price      numeric(10, 2) NOT NULL,
  quantity        integer NOT NULL
);
CREATE INDEX invoice_line_invoice_id_idx ON public.invoice_line (invoice_id);
CREATE INDEX invoice_line_track_id_idx ON public.invoice_line (track_id);

-- Mutation target: rows written by the benchmark (inserts/updates/deletes).
CREATE TABLE public.playlist (
  playlist_id serial PRIMARY KEY,
  name        text NOT NULL,
  owner_id    integer NOT NULL REFERENCES public.customer (customer_id),
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX playlist_owner_id_idx ON public.playlist (owner_id);

-- Computed field: track duration in seconds.
CREATE FUNCTION public.track_seconds(track_row public.track)
RETURNS integer LANGUAGE sql STABLE AS $$
  SELECT track_row.milliseconds / 1000
$$;

-- Tracked view.
CREATE VIEW public.artist_stats AS
SELECT a.artist_id,
       a.name,
       count(DISTINCT al.album_id) AS album_count,
       count(t.track_id)           AS track_count
FROM public.artist a
LEFT JOIN public.album al ON al.artist_id = a.artist_id
LEFT JOIN public.track t  ON t.album_id = al.album_id
GROUP BY a.artist_id, a.name;

