-- Synthetic T3 Code store, schema copied from a real statev2.sqlite.
-- @PI_SESSIONS@ is replaced with the test's Pi session root.
-- No real store holds an opencode row: the opencode reference shape comes from
-- T3's adapter source.
CREATE TABLE orchestration_v2_projection_threads (
      thread_id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      title TEXT NOT NULL,
      default_provider TEXT NOT NULL,
      runtime_mode TEXT NOT NULL,
      interaction_mode TEXT NOT NULL,
      active_provider_thread_id TEXT,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      archived_at TEXT,
      deleted_at TEXT,
      payload_json TEXT NOT NULL
    , provider_instance_id TEXT);
CREATE TABLE orchestration_v2_projection_provider_threads (
      provider_thread_id TEXT PRIMARY KEY,
      thread_id TEXT,
      owner_node_id TEXT,
      provider TEXT NOT NULL,
      provider_session_id TEXT,
      status TEXT NOT NULL,
      first_run_ordinal INTEGER,
      last_run_ordinal INTEGER,
      updated_at TEXT NOT NULL,
      payload_json TEXT NOT NULL
    , driver TEXT, provider_instance_id TEXT);
CREATE TABLE orchestration_v2_projection_subagents (
      subagent_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      run_id TEXT,
      parent_node_id TEXT NOT NULL,
      provider TEXT NOT NULL,
      provider_thread_id TEXT,
      child_thread_id TEXT,
      origin TEXT NOT NULL,
      status TEXT NOT NULL,
      started_at TEXT,
      completed_at TEXT,
      updated_at TEXT NOT NULL,
      payload_json TEXT NOT NULL
    , driver TEXT, provider_instance_id TEXT, completion_delivery_state TEXT);

INSERT INTO orchestration_v2_projection_threads
  (thread_id, project_id, title, default_provider, runtime_mode, interaction_mode, active_provider_thread_id, created_at, updated_at, deleted_at, payload_json) VALUES
  ('user-thread', 'p', 'Codex thread title', 'codex', 'm', 'i', 'pt-user', '2026-01-01', '2026-01-09', NULL, '{}'),
  ('thread:delegated-task:command%3Amcp%3Aaaaa%3Adelegate-task%3Abbbb', 'p', 'Delegated', 'pi', 'm', 'i', 'pt-delegated', '2026-01-01', '2026-01-08', NULL, '{}'),
  ('claude-thread', 'p', 'Claude thread', 'claudeAgent', 'm', 'i', 'pt-claude', '2026-01-01', '2026-01-07', NULL, '{}'),
  ('thread:provider:claudeAgent:native-thread:provider-thread%3Apending%253Arun%3Aagent-fixture', 'p', 'Subagent', 'claudeAgent', 'm', 'i', NULL, '2026-01-01', '2026-01-06', NULL, '{}'),
  ('cursor-thread', 'p', 'Cursor thread', 'cursor', 'm', 'i', 'pt-cursor', '2026-01-01', '2026-01-05', NULL, '{}'),
  ('pending-thread', 'p', 'Pending thread', 'codex', 'm', 'i', 'pt-pending', '2026-01-01', '2026-01-04', NULL, '{}'),
  ('handoff-thread', 'p', 'Handoff thread', 'codex', 'm', 'i', 'pt-handoff-a', '2026-01-01', '2026-01-03', NULL, '{}'),
  ('deleted-thread', 'p', 'Deleted thread', 'codex', 'm', 'i', 'pt-deleted', '2026-01-01', '2026-01-02', '2026-01-02', '{}');

INSERT INTO orchestration_v2_projection_provider_threads
  (provider_thread_id, thread_id, provider, status, last_run_ordinal, updated_at, payload_json) VALUES
  ('pt-user', 'user-thread', 'codex', 's', 1, '2026-01-09', '{"nativeThreadRef":{"driver":"codex","nativeId":"00000000-0000-0000-0000-000000000001","strength":"strong"}}'),
  ('pt-delegated', 'thread:delegated-task:command%3Amcp%3Aaaaa%3Adelegate-task%3Abbbb', 'pi', 's', 1, '2026-01-08', '{"nativeThreadRef":{"driver":"pi","nativeId":"@PI_SESSIONS@/2026-01-01T10-00-00-000Z_session-pi.jsonl","strength":"strong"}}'),
  ('pt-claude', 'claude-thread', 'claudeAgent', 's', 1, '2026-01-07', '{"nativeThreadRef":{"driver":"claudeAgent","nativeId":"session-claude","strength":"strong"}}'),
  ('pt-cursor', 'cursor-thread', 'cursor', 's', 1, '2026-01-05', '{"nativeThreadRef":{"driver":"cursor","nativeId":"cursor-native","strength":"strong"}}'),
  ('pt-pending', 'pending-thread', 'codex', 's', NULL, '2026-01-04', '{"nativeThreadRef":null}'),
  ('pt-malformed', 'pending-thread', 'codex', 's', NULL, '2026-01-04', 'not json'),
  ('pt-handoff-a', 'handoff-thread', 'codex', 's', 1, '2026-01-03', '{"nativeThreadRef":{"driver":"codex","nativeId":"10000000-0000-0000-0000-000000000002","strength":"strong"}}'),
  ('pt-handoff-b', 'handoff-thread', 'codex', 's', 2, '2026-01-03', '{"nativeThreadRef":{"driver":"codex","nativeId":"00000000-0000-0000-0000-00000000000b","strength":"strong"}}'),
  ('pt-deleted', 'deleted-thread', 'codex', 's', 1, '2026-01-02', '{"nativeThreadRef":{"driver":"codex","nativeId":"00000000-0000-0000-0000-00000000000d","strength":"strong"}}');

INSERT INTO orchestration_v2_projection_subagents
  (subagent_id, thread_id, parent_node_id, provider, child_thread_id, origin, status, updated_at, payload_json, driver) VALUES
  ('sa-1', 'claude-thread', 'n', 'claudeAgent', 'thread:provider:claudeAgent:native-thread:provider-thread%3Apending%253Arun%3Aagent-fixture', 'provider', 's', '2026-01-06', '{"nativeTaskRef":{"nativeId":"agent-fixture"}}', 'claudeAgent');
