-- Hermes 0.19.0 state.db, schema 22: the three tables tollgate reads, copied
-- from hermes_state.py:758-856 (Hermes Agent, MIT licence), plus fixture rows.
-- Built into a db by `sqlite3` at test time (tests/inline/usage_hermes_local.rs).
-- Month under test: September 2026 (1788220800 = 2026-09-01T00:00:00Z).

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    user_id TEXT,
    session_key TEXT,
    chat_id TEXT,
    chat_type TEXT,
    thread_id TEXT,
    display_name TEXT,
    origin_json TEXT,
    expiry_finalized INTEGER DEFAULT 0,
    model TEXT,
    model_config TEXT,
    system_prompt TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_write_tokens INTEGER DEFAULT 0,
    reasoning_tokens INTEGER DEFAULT 0,
    cwd TEXT,
    git_branch TEXT,
    git_repo_root TEXT,
    billing_provider TEXT,
    billing_base_url TEXT,
    billing_mode TEXT,
    estimated_cost_usd REAL,
    actual_cost_usd REAL,
    cost_status TEXT,
    cost_source TEXT,
    pricing_version TEXT,
    title TEXT,
    api_call_count INTEGER DEFAULT 0,
    handoff_state TEXT,
    handoff_platform TEXT,
    handoff_error TEXT,
    compression_failure_cooldown_until REAL,
    compression_failure_error TEXT,
    compression_fallback_streak INTEGER NOT NULL DEFAULT 0,
    profile_name TEXT,
    rewind_count INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
);

CREATE TABLE IF NOT EXISTS session_model_usage (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    billing_provider TEXT NOT NULL DEFAULT '',
    billing_base_url TEXT NOT NULL DEFAULT '',
    billing_mode TEXT NOT NULL DEFAULT '',
    task TEXT NOT NULL DEFAULT '',
    api_call_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0,
    actual_cost_usd REAL NOT NULL DEFAULT 0,
    cost_status TEXT,
    cost_source TEXT,
    first_seen REAL,
    last_seen REAL,
    PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task)
);

INSERT INTO schema_version VALUES (22);
INSERT INTO sessions (id, source, started_at, title, billing_provider)
  VALUES ('s-aug', 'cli', 1787000000, 'last month', 'openrouter');
INSERT INTO sessions (id, source, started_at, title, billing_provider)
  VALUES ('s-sep1', 'cli', 1788300000, 'refactor the parser', 'openrouter');
INSERT INTO sessions (id, source, started_at, title, billing_provider)
  VALUES ('s-sep2', 'cli', 1788400000, 'write the docs', 'openrouter');
-- August: outside the month, never counted.
INSERT INTO session_model_usage (session_id, model, billing_provider, api_call_count,
  input_tokens, output_tokens, estimated_cost_usd, actual_cost_usd, first_seen, last_seen)
  VALUES ('s-aug', 'anthropic/claude-sonnet-4.5', 'openrouter', 9, 900, 90, 9.0, 0, 1787000000, 1787000100);
-- Billed cost wins where it is > 0.
INSERT INTO session_model_usage (session_id, model, billing_provider, api_call_count,
  input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
  estimated_cost_usd, actual_cost_usd, first_seen, last_seen)
  VALUES ('s-sep1', 'anthropic/claude-sonnet-4.5', 'openrouter', 2, 1000, 200, 50, 10, 5,
  0.5, 0.010000, 1788300000, 1788300100);
-- No billed cost: the estimate counts.
INSERT INTO session_model_usage (session_id, model, billing_provider, api_call_count,
  input_tokens, output_tokens, estimated_cost_usd, actual_cost_usd, first_seen, last_seen)
  VALUES ('s-sep2', 'anthropic/claude-sonnet-4.5', 'openrouter', 1, 300, 30, 0.002345, 0, 1788400000, 1788400050);
-- A different task of the same model groups with it.
INSERT INTO session_model_usage (session_id, model, billing_provider, task, api_call_count,
  input_tokens, output_tokens, estimated_cost_usd, actual_cost_usd, first_seen, last_seen)
  VALUES ('s-sep2', 'anthropic/claude-sonnet-4.5', 'openrouter', 'title_generation', 4, 40, 4,
  0.000001, 0, 1788400001, 1788400060);
