-- Sessions are keyed by a digest bound to the admin token (D-016): the cookie carries the
-- id, the row only SHA-256 over the token's digest and that id. Rows written before this
-- hold raw ids that no cookie can match any more — delete them now rather than keep
-- usable ids in the store and in every backup until they expire. Everyone signs in again
-- once.
DELETE FROM sessions;
