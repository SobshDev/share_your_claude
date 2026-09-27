-- Fable 5.1 is an ordinary catalog model: the owner reviews, enables, and grants it like any
-- other. Remove the grant triggers from 0005 and file former Fable rows with the other models.
DROP TRIGGER IF EXISTS key_model_grant_no_fable_insert;
DROP TRIGGER IF EXISTS key_model_grant_no_fable_update;
DROP TRIGGER IF EXISTS model_fable_group_revokes_grants;
UPDATE model SET model_group='claude' WHERE model_group='fable-5.1';
