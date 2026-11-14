-- Native stable database shape; part indexes support session and message lookup.
CREATE TABLE project(id TEXT PRIMARY KEY);
CREATE TABLE session(
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL,
 directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT,
 summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER,
 summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL,
 time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER,
 workspace_id TEXT, path TEXT, agent TEXT, model TEXT,
 cost REAL DEFAULT 0 NOT NULL, tokens_input INTEGER DEFAULT 0 NOT NULL,
 tokens_output INTEGER DEFAULT 0 NOT NULL, tokens_reasoning INTEGER DEFAULT 0 NOT NULL,
 tokens_cache_read INTEGER DEFAULT 0 NOT NULL, tokens_cache_write INTEGER DEFAULT 0 NOT NULL,
 metadata TEXT, FOREIGN KEY(project_id) REFERENCES project(id) ON DELETE CASCADE
);
CREATE INDEX session_project_idx ON session(project_id);
CREATE INDEX session_parent_idx ON session(parent_id);
CREATE INDEX session_workspace_idx ON session(workspace_id);
CREATE TABLE message(
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
 time_updated INTEGER NOT NULL, data TEXT NOT NULL,
 FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);
CREATE INDEX message_session_time_created_id_idx ON message(session_id,time_created,id);
CREATE TABLE part(
 id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
 time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL,
 FOREIGN KEY(message_id) REFERENCES message(id) ON DELETE CASCADE
);
CREATE INDEX part_session_idx ON part(session_id);
CREATE INDEX part_message_id_id_idx ON part(message_id,id);
INSERT INTO project VALUES('project_fixture');
INSERT INTO session(id,project_id,parent_id,slug,directory,title,version,time_created,time_updated)
 VALUES('ses_fixture','project_fixture','ses_parent','fixture','/fixtures','Fixture','fixture',1,1);
INSERT INTO message VALUES('msg_fixture','ses_fixture',1,1,'{"role":"assistant","parentID":"msg_parent","time":{"created":1,"completed":2}}');
