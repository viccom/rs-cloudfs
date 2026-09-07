BEGIN TRANSACTION;
CREATE TABLE chunks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    file_id INTEGER NOT NULL,
                    chunk_index INTEGER NOT NULL,
                    telegram_msg_id INTEGER,
                    size INTEGER NOT NULL,
                    sha256 TEXT,
                    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE,
                    UNIQUE(file_id, chunk_index)
                );
INSERT INTO "chunks" VALUES(1,5,0,201,1992294400,NULL);
INSERT INTO "chunks" VALUES(2,5,1,202,1992294400,NULL);
INSERT INTO "chunks" VALUES(3,5,2,203,1024,NULL);
CREATE TABLE files (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    rel_path TEXT UNIQUE NOT NULL,
                    name TEXT NOT NULL,
                    parent_dir TEXT NOT NULL,
                    size INTEGER DEFAULT 0,
                    mtime REAL DEFAULT 0,
                    sha256 TEXT,
                    is_dir INTEGER DEFAULT 0,
                    telegram_msg_id INTEGER,
                    is_uploaded INTEGER DEFAULT 0,
                    is_cached INTEGER DEFAULT 1,
                    is_encrypted INTEGER DEFAULT 0,
                    chunk_count INTEGER DEFAULT 1,
                    mime_type TEXT,
                    created_at REAL,
                    updated_at REAL
                );
INSERT INTO "files" VALUES(1,'/Documents','Documents','/',0,0.0,NULL,1,NULL,0,1,0,1,NULL,1.78826717518969512e+09,1.78826717518969512e+09);
INSERT INTO "files" VALUES(2,'/Documents/report.pdf','report.pdf','/Documents',1024,1700000000.5,'abababababababababababababababababababababababababababababababab',0,111,1,1,0,1,'application/pdf',1.78826717518969512e+09,1.78826717518969512e+09);
INSERT INTO "files" VALUES(3,'/notes.txt','notes.txt','/',42,1700000100.0,NULL,0,NULL,0,1,0,1,NULL,1.788267175190694571e+09,1.788267175190694571e+09);
INSERT INTO "files" VALUES(4,'/big','big','/',0,0.0,NULL,1,NULL,0,1,0,1,NULL,1.788267175190694571e+09,1.788267175190694571e+09);
INSERT INTO "files" VALUES(5,'/big/movie.mkv','movie.mkv','/big',5976883200,1700000200.0,NULL,0,200,1,1,0,3,NULL,1.788267175191694022e+09,1.788267175191694022e+09);
CREATE TABLE stats (
                    key TEXT PRIMARY KEY,
                    value TEXT
                );
CREATE INDEX idx_files_parent ON files(parent_dir);
CREATE INDEX idx_files_msg_id ON files(telegram_msg_id);
CREATE INDEX idx_files_uploaded ON files(is_uploaded);
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('files',5);
INSERT INTO "sqlite_sequence" VALUES('chunks',3);
COMMIT;
