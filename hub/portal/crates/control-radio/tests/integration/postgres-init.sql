-- ChirpStack's own migrations assume both of these exist. Creating them in the container's
-- init step rather than from the application is the same order the device's packaging uses.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS hstore;
