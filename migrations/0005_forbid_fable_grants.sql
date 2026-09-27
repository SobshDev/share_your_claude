-- Fable 5.1 is reserved for the owner and must never be granted to a router key. These checks
-- mirror policy::blocked (group 'fable-5.1', id 'claude-fable-5-1' or 'claude-fable-5-1-*').
-- LIKE is case-insensitive here, so the database blocks at least what the Rust check blocks.
DELETE FROM key_model_grant WHERE model_id LIKE 'claude-fable-5-1'
 OR model_id LIKE 'claude-fable-5-1-%'
 OR model_id IN (SELECT id FROM model WHERE model_group='fable-5.1');

CREATE TRIGGER key_model_grant_no_fable_insert BEFORE INSERT ON key_model_grant
 WHEN NEW.model_id LIKE 'claude-fable-5-1' OR NEW.model_id LIKE 'claude-fable-5-1-%'
  OR EXISTS (SELECT 1 FROM model WHERE id=NEW.model_id AND model_group='fable-5.1')
BEGIN SELECT RAISE(ABORT, 'Fable 5.1 cannot be granted to a key'); END;
CREATE TRIGGER key_model_grant_no_fable_update BEFORE UPDATE OF model_id ON key_model_grant
 WHEN NEW.model_id LIKE 'claude-fable-5-1' OR NEW.model_id LIKE 'claude-fable-5-1-%'
  OR EXISTS (SELECT 1 FROM model WHERE id=NEW.model_id AND model_group='fable-5.1')
BEGIN SELECT RAISE(ABORT, 'Fable 5.1 cannot be granted to a key'); END;

-- A model moved into the Fable group loses every grant it had.
CREATE TRIGGER model_fable_group_revokes_grants AFTER UPDATE OF model_group ON model
 WHEN NEW.model_group='fable-5.1'
BEGIN DELETE FROM key_model_grant WHERE model_id=NEW.id; END;
