PRAGMA cache_size=-8192;
PRAGMA mmap_size=0;
PRAGMA temp_store=FILE;
CREATE TABLE copies (
    sequence INTEGER PRIMARY KEY,
    destination BLOB UNIQUE NOT NULL,
    record TEXT NOT NULL
);
CREATE TABLE shadows (anchor TEXT PRIMARY KEY, path BLOB NOT NULL);
CREATE TABLE cleanup (
    path BLOB PRIMARY KEY,
    is_dir INTEGER NOT NULL,
    depth INTEGER NOT NULL
);
CREATE INDEX cleanup_depth ON cleanup(depth DESC,path);
