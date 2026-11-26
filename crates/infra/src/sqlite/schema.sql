CREATE TABLE metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
);
INSERT INTO metadata(singleton, revision) VALUES (1, 0);
CREATE TABLE objects (
    namespace TEXT NOT NULL,
    scheme TEXT NOT NULL,
    authority TEXT NOT NULL,
    object_key BLOB NOT NULL,
    size_bytes INTEGER CHECK (size_bytes >= 0),
    data_class TEXT,
    restore_state TEXT,
    restore_expiry_seconds INTEGER,
    original_mtime_seconds INTEGER,
    original_mtime_nanos INTEGER,
    original_creation_seconds INTEGER,
    original_creation_nanos INTEGER,
    unix_mode INTEGER,
    queried_seconds INTEGER NOT NULL,
    queried_nanos INTEGER NOT NULL,
    store_modified_seconds INTEGER,
    version_token TEXT,
    checksum_algorithm TEXT,
    checksum_type TEXT,
    checksum_value TEXT,
    revision INTEGER NOT NULL,
    PRIMARY KEY (namespace, scheme, authority, object_key)
) WITHOUT ROWID;
