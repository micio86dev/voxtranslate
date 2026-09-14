#!/bin/bash
# Create the two local databases and give each the extensions the schema needs.
#
# Runs once, when the data directory is empty. `set -e` matters: a half-created database
# that the healthcheck then reports as ready is worse than a container that fails loudly.
set -euo pipefail

for db in voxtranslate_dev voxtranslate_voip_test; do
  echo "creating $db"
  psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres <<-EOSQL
    CREATE DATABASE $db;
EOSQL
  # pgvector, in BOTH. The test database needs it for the embedding-backed tests — without
  # it they skip and the run still prints ok, which is the most expensive kind of green.
  # The dev database needs it because migrations create vector columns.
  psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$db" <<-EOSQL
    CREATE EXTENSION IF NOT EXISTS vector;
EOSQL
done

echo "ready: voxtranslate_dev (the server), voxtranslate_voip_test (the suite)"
