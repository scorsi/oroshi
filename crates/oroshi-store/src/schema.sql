CREATE TABLE blob (
    sha256 BLOB PRIMARY KEY NOT NULL CHECK (typeof(sha256) = 'blob' AND length(sha256) = 32),
    size INTEGER NOT NULL CHECK (typeof(size) = 'integer' AND size >= 0),
    created_at INTEGER NOT NULL
);

CREATE TABLE nar (
    url TEXT PRIMARY KEY NOT NULL,
    blob_sha256 BLOB NOT NULL REFERENCES blob(sha256)
);

CREATE TABLE narinfo (
    store_path TEXT PRIMARY KEY NOT NULL,
    store_hash TEXT NOT NULL UNIQUE CHECK (length(store_hash) = 32),
    nar_url TEXT NOT NULL REFERENCES nar(url),
    content TEXT NOT NULL,
    source TEXT NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE TABLE reference (
    referrer TEXT NOT NULL REFERENCES narinfo(store_path),
    reference TEXT NOT NULL REFERENCES narinfo(store_path),
    PRIMARY KEY (referrer, reference)
);
