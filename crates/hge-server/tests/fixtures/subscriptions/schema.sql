DROP SCHEMA IF EXISTS public CASCADE;
CREATE SCHEMA public;
SET TIME ZONE 'UTC';

CREATE TABLE message (
  id serial PRIMARY KEY,
  author_id integer NOT NULL,
  body text NOT NULL,
  priority integer NOT NULL DEFAULT 0
);

INSERT INTO message (author_id, body, priority) VALUES
  (1, 'one', 1),
  (2, 'two', 2),
  (1, 'three', 2);

-- A tracked function with a session argument: every cohort must call it with its own session.
CREATE FUNCTION my_messages(hasura_session json, min_priority integer DEFAULT 0)
RETURNS SETOF message STABLE LANGUAGE sql AS $$
  SELECT * FROM message
  WHERE author_id = (hasura_session->>'x-hasura-user-id')::integer AND priority >= min_priority
$$;
