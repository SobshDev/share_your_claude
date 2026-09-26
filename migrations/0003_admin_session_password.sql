-- Bind admin sessions to the owner password hash they were issued under, so changing
-- ADMIN_PASSWORD_HASH signs every session out. Sessions from before this column existed
-- cannot be attributed to a password, so the owner signs in once more after upgrading.
DELETE FROM admin_session;
ALTER TABLE admin_session ADD COLUMN password_fp BLOB NOT NULL DEFAULT x'';
