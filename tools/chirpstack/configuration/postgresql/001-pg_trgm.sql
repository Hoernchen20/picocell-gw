-- ChirpStack's search needs pg_trgm. Runs once, when the database volume is created.
create extension if not exists pg_trgm;
