INSERT INTO orchestration_v2_projection_threads
  (thread_id, project_id, title, default_provider, runtime_mode, interaction_mode,
   active_provider_thread_id, created_at, updated_at, payload_json)
  VALUES ('switch-thread','p','Example switch','antigravity','m','i',
          'pt-example-acp','2026-01-01','2026-01-10','{}');

INSERT INTO orchestration_v2_projection_provider_threads
  (provider_thread_id, thread_id, provider, status, last_run_ordinal, updated_at, payload_json, driver) VALUES
  ('pt-example-acp','switch-thread','antigravity','s',2,'2026-01-10',
   '{"nativeThreadRef":{"driver":"antigravity","nativeId":"a0000000-0000-4000-8000-000000000001","strength":"strong"}}','antigravity'),
  ('pt-example-codex','switch-thread','codex','s',1,'2026-01-09',
   '{"nativeThreadRef":{"driver":"codex","nativeId":"00000000-0000-0000-0000-000000000001","strength":"strong"}}','codex');

CREATE TABLE orchestration_v2_projection_provider_turns (
  provider_turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, provider_thread_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL, status TEXT NOT NULL, started_at TEXT, completed_at TEXT, payload_json TEXT
);
CREATE TABLE orchestration_v2_projection_turn_items (
  turn_item_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, provider_thread_id TEXT,
  provider_turn_id TEXT, ordinal INTEGER NOT NULL, type TEXT NOT NULL, status TEXT, payload_json TEXT
);
INSERT INTO orchestration_v2_projection_provider_turns VALUES
  ('ant-fail','switch-thread','pt-example-acp',3,'failed','2026-01-01T10:00:07Z','2026-01-01T10:00:08Z','{}'),
  ('ant-stop','switch-thread','pt-example-acp',4,'interrupted','2026-01-01T10:00:09Z',NULL,'{}'),
  ('codex-fail','switch-thread','pt-example-codex',1,'failed','2026-01-01T09:00:00Z',NULL,'{}');
INSERT INTO orchestration_v2_projection_turn_items VALUES
  ('error1','switch-thread','pt-example-acp','ant-fail',1,'error','failed',
   '{"type":"error","failure":{"class":"provider_error","code":null,"message":"Example provider quota exhausted","retryable":false}}'),
  ('error2','switch-thread','pt-example-codex','codex-fail',1,'error','failed',
   '{"type":"error","failure":{"class":"provider_error","code":null,"message":"Other provider failure","retryable":false}}');
