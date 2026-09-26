-- No query filters on resolved_model alone. Reports filter on the effective model,
-- COALESCE(resolved_model, requested_model), within the endpoint and time range.
DROP INDEX IF EXISTS usage_model_time;
CREATE INDEX usage_endpoint_model_time
 ON request_usage(endpoint, COALESCE(resolved_model, requested_model), started_at);

-- Same limit as person.name. SQLite cannot add a CHECK constraint to an existing table.
CREATE TRIGGER api_key_label_length_insert BEFORE INSERT ON api_key
 WHEN length(NEW.label) NOT BETWEEN 1 AND 100
BEGIN SELECT RAISE(ABORT, 'api_key.label must be 1 to 100 characters'); END;
CREATE TRIGGER api_key_label_length_update BEFORE UPDATE OF label ON api_key
 WHEN length(NEW.label) NOT BETWEEN 1 AND 100
BEGIN SELECT RAISE(ABORT, 'api_key.label must be 1 to 100 characters'); END;
