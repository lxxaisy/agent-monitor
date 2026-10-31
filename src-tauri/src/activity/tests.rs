use super::*;
use crate::model::{ProgressBasis, ProgressConfidence};
use chrono::TimeZone;
use rusqlite::params;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    service: ActivityService,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-activity-{}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&root).unwrap();
        let service = ActivityService::open(&root, root.join("monitor/activity.db")).unwrap();
        Self { root, service }
    }

    fn database(&self, path: &str, schema: &str) -> Connection {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = Connection::open(path).unwrap();
        db.execute_batch(schema).unwrap();
        db
    }

    fn state_started_at(&self, at: &str) {
        self.service.state.lock().unwrap().started_at = at.into();
    }

    fn jsonl(&self, relative: &str, rows: &[Value]) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        for value in rows {
            writeln!(file, "{}", value).unwrap();
        }
        path
    }

    fn codex(&self) -> (Connection, Connection) {
        let state = self.database(".codex/state_5.sqlite",
            "CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,cwd TEXT,rollout_path TEXT,model_provider TEXT,model TEXT,source TEXT,updated_at INTEGER,archived INTEGER NOT NULL DEFAULT 0);");
        let history = self.database(".codex/thread_history_1.sqlite",
            "CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT,status TEXT,started_at INTEGER,completed_at INTEGER,duration_ms INTEGER,root_turn_id TEXT,rollout_byte_offset INTEGER,rollout_ordinal INTEGER,PRIMARY KEY(thread_id,turn_id));");
        (state, history)
    }

    fn zcode(&self) -> Connection {
        self.database(".zcode/cli/db/db.sqlite",
            "CREATE TABLE session(id TEXT PRIMARY KEY,title TEXT,directory TEXT,parent_id TEXT,time_archived INTEGER,time_deleted INTEGER);
             CREATE TABLE session_task_link(child_session_id TEXT,parent_session_id TEXT);
             CREATE TABLE dwf_actor(session_id TEXT,run_id TEXT);
             CREATE TABLE dwf_run(id TEXT,parent_session_id TEXT,tool_call_id TEXT);
             CREATE TABLE tool_usage(session_id TEXT,turn_id TEXT,tool_call_id TEXT);
             CREATE TABLE turn_usage(session_id TEXT,turn_id TEXT,status TEXT,started_at INTEGER,completed_at INTEGER,duration_ms INTEGER,input_tokens INTEGER,output_tokens INTEGER,reasoning_tokens INTEGER,cache_creation_input_tokens INTEGER,cache_read_input_tokens INTEGER,computed_total_tokens INTEGER);
             CREATE TABLE model_usage(id TEXT PRIMARY KEY,session_id TEXT,turn_id TEXT,provider_id TEXT,model_id TEXT,started_at INTEGER,completed_at INTEGER,input_tokens INTEGER,output_tokens INTEGER,reasoning_tokens INTEGER,cache_creation_input_tokens INTEGER,cache_read_input_tokens INTEGER,computed_total_tokens INTEGER);")
    }

    fn add_codex_conversation(&self, db: &Connection, session: &str, source: Value) -> PathBuf {
        let path = self.jsonl(&format!(".codex/sessions/{session}.jsonl"), &[]);
        db.execute("INSERT INTO threads(id,title,cwd,rollout_path,model_provider,model,source,updated_at) VALUES (?1,?2,'/tmp/project',?3,'example-provider','example-model',?4,?5)",
            params![session, format!("Conversation {session}"), path.to_str().unwrap(), source.to_string(), Utc::now().timestamp()]).unwrap();
        path
    }

    fn add_turn(
        &self,
        db: &Connection,
        session: &str,
        turn: &str,
        status: &str,
        start: i64,
        end: Option<i64>,
        root: &str,
        ordinal: i64,
    ) {
        db.execute(
            "INSERT INTO thread_turns VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8)",
            params![
                session,
                turn,
                status,
                start,
                end,
                end.map(|end| (end - start) * 1000),
                root,
                ordinal
            ],
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn failed_sync_commit_rolls_back_and_next_refresh_recovers() {
    let fixture = Fixture::new();
    let (source, _) = fixture.codex();
    fixture.add_codex_conversation(&source, "before-failure", json!("cli"));
    fixture.service.sync(&[], &[]).unwrap();
    fixture.add_codex_conversation(&source, "after-failure", json!("cli"));
    {
        let state = fixture.service.state.lock().unwrap();
        state.db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE sync_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE sync_child(parent_id INTEGER REFERENCES sync_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_sync_commit AFTER INSERT ON activity_sessions BEGIN
               INSERT INTO sync_child VALUES(1);
             END;",
        ).unwrap();
    }
    let first = fixture.service.sync(&[], &[]).unwrap_err();
    assert!(first.to_string().contains("FOREIGN KEY constraint failed"));
    {
        let state = fixture.service.state.lock().unwrap();
        assert!(
            state.db.is_autocommit(),
            "A failed refresh must not leave a transaction open"
        );
        let saved: u64 = state
            .db
            .query_row(
                "SELECT COUNT(*) FROM activity_sessions WHERE session_id='after-failure'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            saved, 0,
            "The failed refresh must roll back its partial writes"
        );
        state
            .db
            .execute_batch("DROP TRIGGER fail_sync_commit")
            .unwrap();
    }
    fixture
        .service
        .sync(&[], &[])
        .expect("The next refresh must recover without reopening the service");
    let state = fixture.service.state.lock().unwrap();
    assert!(state.db.is_autocommit());
    let saved: u64 = state
        .db
        .query_row("SELECT COUNT(*) FROM activity_sessions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(saved, 2);
}

#[test]
fn transient_database_lock_allows_refresh_after_writer_releases_it() {
    let fixture = Fixture::new();
    fixture.service.sync(&[], &[]).unwrap();
    fixture
        .service
        .state
        .lock()
        .unwrap()
        .db
        .busy_timeout(std::time::Duration::ZERO)
        .unwrap();
    let writer = Connection::open(fixture.root.join("monitor/activity.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = fixture.service.sync(&[], &[]).unwrap_err();
    assert!(error.to_string().contains("database is locked"));
    assert!(fixture.service.state.lock().unwrap().db.is_autocommit());
    writer.execute_batch("ROLLBACK").unwrap();
    fixture
        .service
        .sync(&[], &[])
        .expect("A temporary database lock must not require an app restart");
}

#[test]
fn prunes_only_expired_terminal_history_and_its_usage() {
    let fixture = Fixture::new();
    let state = fixture.service.state.lock().unwrap();
    state
        .db
        .execute(
            "INSERT INTO activity_executions
             (id,client,session_id,turn_id,started_at,observed_at,payload,relation)
             VALUES
             ('old','codex','session','old','2020-01-01T00:00:00Z','2020-01-01T00:00:01Z',
              '{\"status\":\"succeeded\",\"completedAt\":\"2020-01-01T00:00:01Z\"}','{}'),
             ('running','codex','session','running','2020-01-01T00:00:00Z','2020-01-01T00:00:01Z',
              '{\"status\":\"unknown\"}','{}'),
             ('recent','codex','session','recent','2099-01-01T00:00:00Z','2099-01-01T00:00:01Z',
              '{\"status\":\"succeeded\",\"completedAt\":\"2099-01-01T00:00:01Z\"}','{}')",
            [],
        )
        .unwrap();
    state
        .db
        .execute(
            "INSERT INTO activity_requests(id,execution_id,client,recorded_at,tokens)
             VALUES ('old-request','old','codex','2020-01-01T00:00:01Z','{}')",
            [],
        )
        .unwrap();
    assert_eq!(prune_history(&state.db, 30).unwrap(), 2);
    assert_eq!(
        state
            .db
            .query_row(
                "SELECT COUNT(*) FROM activity_executions WHERE id='old'",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        state
            .db
            .query_row(
                "SELECT COUNT(*) FROM activity_requests WHERE id='old-request'",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        state
            .db
            .query_row("SELECT COUNT(*) FROM activity_executions", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn unchanged_sync_reuses_the_derived_activity_cache() {
    let fixture = Fixture::new();
    let (state, _) = fixture.codex();
    fixture.add_codex_conversation(&state, "stable", json!("cli"));
    fixture.service.sync(&[], &[]).unwrap();
    let before = fixture
        .service
        .state
        .lock()
        .unwrap()
        .db
        .query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))
        .unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    let after = fixture
        .service
        .state
        .lock()
        .unwrap()
        .db
        .query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))
        .unwrap();
    assert_eq!(before, after);
    assert!(snapshot.executions.is_empty());
}

#[test]
fn retention_change_applies_on_sync_and_expired_turns_stay_pruned() {
    let fixture = Fixture::new();
    let (source, history) = fixture.codex();
    let path = fixture.add_codex_conversation(&source, "retention", json!("cli"));
    let old = Utc::now() - Duration::days(8);
    fixture.add_turn(
        &history,
        "retention",
        "ended",
        "completed",
        old.timestamp() - 10,
        Some(old.timestamp()),
        "ended",
        1,
    );
    let mut usage = codex_usage("retention", "ended", "expired", 100, 40, 20);
    usage["timestamp"] = json!(old.to_rfc3339());
    append(&path, &[usage]);
    assert_eq!(
        fixture
            .service
            .sync_with_retention(&[], &[], 30)
            .unwrap()
            .summary
            .total_turns,
        1
    );
    assert_eq!(
        fixture
            .service
            .sync_with_retention(&[], &[], 7)
            .unwrap()
            .summary
            .total_turns,
        0
    );
    assert_eq!(
        fixture
            .service
            .sync_with_retention(&[], &[], 7)
            .unwrap()
            .summary
            .total_turns,
        0,
        "source history must not restore a pruned execution on the next collection"
    );
    assert_eq!(
        fixture
            .service
            .state
            .lock()
            .unwrap()
            .db
            .query_row("SELECT COUNT(*) FROM activity_requests", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

fn append(path: &Path, rows: &[Value]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

fn codex_usage(
    session: &str,
    turn: &str,
    response: &str,
    input: u64,
    cached: u64,
    output: u64,
) -> Value {
    json!({"type":"token_usage_record","timestamp":now(),"payload":{
        "thread_id":session,"turn_id":turn,"response_id":response,"root_turn_id":"t1",
        "usage":{"input_tokens":input,"cached_input_tokens":cached,"cache_write_input_tokens":0,
                 "output_tokens":output,"reasoning_output_tokens":0,"total_tokens":input+output},
        "turn_token_usage":{"input_tokens":9999999,"total_tokens":9999999}
    }})
}

fn live_task(session: &str, turn: &str, status: TaskStatus) -> TaskSnapshot {
    TaskSnapshot {
        provider: "codex".into(),
        task_id: session.into(),
        turn_id: Some(turn.into()),
        parent_task_id: None,
        title: "Live conversation".into(),
        project_path: "/tmp/project".into(),
        source: "cli".into(),
        status,
        attention: None,
        progress_percent: None,
        progress_basis: ProgressBasis::None,
        progress_confidence: ProgressConfidence::Low,
        created_at: now(),
        updated_at: now(),
        completed_at: None,
        archived: false,
        last_error: None,
        plan_total_steps: 0,
        plan_completed_steps: 0,
        plan_in_progress_steps: 0,
    }
}

#[test]
fn token_categories_normalize_without_recounting_cache_or_reasoning() {
    let codex = TokenUsage {
        input: Some(100),
        output: Some(20),
        cache_read: Some(40),
        cache_write: Some(0),
        reasoning: Some(8),
        total: Some(120),
        input_includes_cache: true,
        output_includes_reasoning: true,
    };
    let claude = TokenUsage {
        input: Some(30),
        output: Some(10),
        cache_read: Some(50),
        cache_write: Some(5),
        reasoning: None,
        total: Some(95),
        input_includes_cache: false,
        output_includes_reasoning: true,
    };
    let normalized = TokenUsage::sum([&codex, &claude], true);
    assert_eq!(normalized.input, Some(90));
    assert_eq!(normalized.output, Some(30));
    assert_eq!(normalized.cache_read, Some(90));
    assert_eq!(normalized.total, Some(215));
    assert_eq!(normalized.reasoning, None);
    assert!(!normalized.input_includes_cache);
    let empty = TokenUsage::default();
    assert_eq!(TokenUsage::sum([&codex, &empty], true).total, None);
    let zero = TokenUsage {
        input: Some(0),
        ..TokenUsage::default()
    };
    assert_eq!(TokenUsage::sum([&zero], false).input, Some(0));
}

#[test]
fn child_history_scope_keeps_unassigned_records_separate_from_root_totals() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    fixture.add_codex_conversation(&state, "root", json!("cli"));
    fixture.add_codex_conversation(
        &state,
        "unassigned",
        json!({"subagent":{"thread_spawn":{"parent_thread_id":"unavailable-parent"}}}),
    );
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 100,
        Some(at - 90),
        "t1",
        1,
    );
    fixture.add_turn(
        &history,
        "unassigned",
        "c1",
        "completed",
        at - 80,
        Some(at - 70),
        "unknown-root",
        1,
    );
    fixture.service.sync(&[], &[]).unwrap();
    let query: ActivityQuery = serde_json::from_value(json!({"childrenOnly":true})).unwrap();
    let children = fixture.service.history(query).unwrap();
    assert_eq!(children.total, 1);
    assert!(children.executions[0].is_child);
    assert_eq!(children.executions[0].root_id, None);
    let sessions = fixture.service.sessions(ActivityQuery::default()).unwrap();
    assert_eq!((sessions.total, sessions.total_turns), (1, 1));
    assert_eq!(fixture.service.snapshot().unwrap().summary.total_turns, 1);
}

#[test]
fn conversation_pagination_keeps_rounds_together_and_counts_descendants_once() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let root = fixture.add_codex_conversation(&state, "root", json!("cli"));
    fixture.add_codex_conversation(&state, "older", json!("cli"));
    let child = fixture.add_codex_conversation(
        &state,
        "child",
        json!({"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}),
    );
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "older",
        "old",
        "completed",
        at - 1000,
        Some(at - 990),
        "old",
        1,
    );
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 300,
        Some(at - 250),
        "t1",
        1,
    );
    fixture.add_turn(
        &history,
        "root",
        "two",
        "failed",
        at - 100,
        Some(at - 50),
        "two",
        2,
    );
    fixture.add_turn(
        &history,
        "child",
        "child-one",
        "completed",
        at - 290,
        Some(at - 260),
        "t1",
        1,
    );
    append(
        &root,
        &[
            codex_usage("root", "t1", "r1", 100, 40, 10),
            codex_usage("root", "two", "r2", 30, 0, 1),
        ],
    );
    append(
        &child,
        &[codex_usage("child", "child-one", "r3", 20, 10, 5)],
    );
    fixture.service.sync(&[], &[]).unwrap();
    let first = fixture
        .service
        .sessions(ActivityQuery {
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        (first.total, first.total_turns, first.sessions.len()),
        (2, 3, 1)
    );
    let summary = &first.sessions[0];
    assert_eq!(summary.latest.session_id, "root");
    assert_eq!(summary.latest.turn_id, "two");
    assert_eq!((summary.turn_count, summary.child_count), (2, 1));
    assert_eq!(summary.tokens.total, Some(166));
    let second = fixture
        .service
        .sessions(ActivityQuery {
            limit: Some(1),
            offset: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(second.sessions[0].latest.session_id, "older");
    assert_eq!(second.sessions[0].tokens.total, None);
    let failed = fixture
        .service
        .sessions(ActivityQuery {
            status: Some("failed".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!((failed.total, failed.total_turns), (1, 1));
    assert_eq!(failed.sessions[0].tokens.total, Some(31));
    state
        .execute("UPDATE threads SET archived=1 WHERE id='root'", [])
        .unwrap();
    fixture.service.sync(&[], &[]).unwrap();
    let visible = fixture.service.sessions(ActivityQuery::default()).unwrap();
    assert_eq!(visible.total, 1);
    assert_eq!(visible.sessions[0].latest.session_id, "older");
}

#[test]
fn codex_retains_each_turn_and_deduplicates_responses_and_inherited_history() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let root = fixture.add_codex_conversation(&state, "root", json!("cli"));
    let child = fixture.add_codex_conversation(
        &state,
        "child",
        json!({"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}),
    );
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 300,
        Some(at - 250),
        "t1",
        1,
    );
    fixture.add_turn(&history, "root", "t2", "inProgress", at - 60, None, "t2", 2);
    fixture.add_turn(
        &history,
        "child",
        "c1",
        "completed",
        at - 290,
        Some(at - 260),
        "t1",
        1,
    );
    append(
        &root,
        &[
            codex_usage("root", "t1", "r1", 100, 40, 10),
            codex_usage("root", "t1", "r1", 100, 40, 10),
            codex_usage("root", "t2", "r2", 30, 0, 1),
        ],
    );
    append(
        &child,
        &[
            codex_usage("root", "t1", "r1", 100, 40, 10),
            codex_usage("child", "c1", "r3", 20, 10, 5),
        ],
    );
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.total_turns, 2);
    assert_eq!(snapshot.summary.running, 0);
    assert_eq!(snapshot.summary.completed_today, 1);
    assert_eq!(snapshot.summary.tokens.total, Some(166));
    assert_eq!(snapshot.summary.tokens.input, Some(100));
    let daily = snapshot
        .daily
        .iter()
        .find(|day| day.client == "codex")
        .unwrap();
    assert_eq!(daily.tokens.input, Some(100));
    assert!(!daily.tokens.input_includes_cache);
    let first = fixture.service.execution("codex:root:t1").unwrap().unwrap();
    assert_eq!(first.child_count, 1);
    assert_eq!(first.tokens.total, Some(135));
    assert_eq!(
        fixture
            .service
            .execution("codex:child:c1")
            .unwrap()
            .unwrap()
            .root_id
            .as_deref(),
        Some("codex:root:t1")
    );
    let again = fixture
        .service
        .sync(&[live_task("root", "t2", TaskStatus::Running)], &[])
        .unwrap();
    assert_eq!(again.summary.running, 1);
    assert_eq!(again.summary.tokens.total, Some(166));
    let page = fixture
        .service
        .history(ActivityQuery {
            roots_only: Some(true),
            limit: Some(1),
            ..ActivityQuery::default()
        })
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.executions.len(), 1);
}

#[test]
fn jsonl_partial_writes_are_resumed_and_completed_usage_is_not_added_twice() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let path = fixture.add_codex_conversation(&state, "root", json!("cli"));
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 20,
        Some(at - 1),
        "t1",
        1,
    );
    history
        .execute("UPDATE thread_turns SET rollout_byte_offset=9999999", [])
        .unwrap();
    let line = codex_usage("root", "t1", "r1", 100, 0, 10).to_string();
    let split = line.len() / 2;
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&line.as_bytes()[..split]).unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        None
    );
    file.write_all(&line.as_bytes()[split..]).unwrap();
    file.write_all(b"\n").unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        Some(110)
    );
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        Some(110)
    );
}

#[test]
fn archived_turns_and_usage_survive_upstream_pruning_and_monitor_restart() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let path = fixture.add_codex_conversation(&state, "root", json!("cli"));
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 60,
        Some(at - 10),
        "t1",
        1,
    );
    append(&path, &[codex_usage("root", "t1", "r1", 100, 0, 10)]);
    fixture.service.sync(&[], &[]).unwrap();
    history.execute("DELETE FROM thread_turns", []).unwrap();
    state.execute("DELETE FROM threads", []).unwrap();
    fs::remove_file(path).unwrap();
    let reopened =
        ActivityService::open(&fixture.root, fixture.root.join("monitor/activity.db")).unwrap();
    let snapshot = reopened.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.total_turns, 1);
    assert_eq!(snapshot.summary.tokens.total, Some(110));
    let path = fixture.add_codex_conversation(&state, "root", json!("cli"));
    fixture.add_turn(
        &history,
        "root",
        "t2",
        "completed",
        at - 5,
        Some(at - 1),
        "t2",
        2,
    );
    append(&path, &[codex_usage("root", "t2", "r2", 10, 0, 1)]);
    let snapshot = reopened.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.total_turns, 2);
    assert_eq!(
        reopened
            .execution("codex:root:t2")
            .unwrap()
            .unwrap()
            .turn_index,
        2
    );
}

#[test]
fn claude_real_user_turns_are_separate_from_tool_results_and_duplicate_blocks() {
    let fixture = Fixture::new();
    let at = now();
    let usage = json!({"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":40});
    fixture.jsonl(".claude/projects/project/session.jsonl",&[
        json!({"type":"custom-title","sessionId":"session","customTitle":"Conversation"}),
        json!({"type":"user","uuid":"u1","sessionId":"session","timestamp":at,"cwd":"/tmp/project","isSidechain":false,"message":{"role":"user","content":"SECRET USER TEXT"}}),
        json!({"type":"assistant","sessionId":"session","timestamp":at,"message":{"id":"r1","model":"example-model","usage":usage,"stop_reason":"tool_use"}}),
        json!({"type":"assistant","sessionId":"session","timestamp":at,"message":{"id":"r1","model":"example-model","usage":usage,"stop_reason":"tool_use"}}),
        json!({"type":"user","uuid":"tool-result","sessionId":"session","timestamp":at,"message":{"content":[{"type":"tool_result","content":"SECRET COMMAND OUTPUT"}]}}),
        json!({"type":"assistant","sessionId":"session","timestamp":at,"message":{"id":"r2","model":"example-model","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"stop_reason":"end_turn"}}),
    ]);
    fixture.jsonl(".claude/projects/project/session/subagents/agent-child.jsonl",&[
        json!({"type":"user","uuid":"cu1","agentId":"child","isSidechain":true,"sessionId":"session","timestamp":at,"message":{"role":"user","content":"SECRET CHILD PROMPT"}}),
        json!({"type":"assistant","sessionId":"session","timestamp":at,"message":{"id":"cr1","usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"stop_reason":"end_turn"}}),
    ]);
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.total_turns, 1);
    assert_eq!(snapshot.summary.completed_today, 1);
    assert_eq!(snapshot.summary.tokens.total, Some(208));
    let root = fixture
        .service
        .execution("claude:session:u1")
        .unwrap()
        .unwrap();
    assert_eq!(root.tokens.total, Some(202));
    assert_eq!(root.child_count, 0);
    let child = fixture
        .service
        .execution("claude:session:child/cu1")
        .unwrap()
        .unwrap();
    assert!(child.is_child);
    assert_eq!(child.root_id, None);
    let bytes = fs::read(fixture.root.join("monitor/activity.db")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("SECRET"));
}

#[test]
fn metadata_hooks_do_not_store_prompt_commands_transcript_or_keys() {
    let fixture = Fixture::new();
    ingest_hook_at(
        &fixture.root,
        "claude",
        "UserPromptSubmit",
        json!({
            "session_id":"s","prompt":"SECRET PROMPT","transcript_path":"/private/transcript",
            "tool_input":{"command":"SECRET COMMAND"},"api_key":"SECRET KEY"
        }),
    )
    .unwrap();
    let text = fs::read_to_string(events_path(&fixture.root)).unwrap();
    assert!(text.contains("UserPromptSubmit"));
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("transcript"));
    assert!(ingest_hook_at(
        &fixture.root,
        "claude",
        "DeleteEverything",
        json!({"session_id":"s"})
    )
    .is_err());
}

#[test]
fn hook_turn_identity_uses_each_clients_native_source_field() {
    let fixture = Fixture::new();
    for client in ["claude", "zcode"] {
        ingest_hook_at(
            &fixture.root,
            client,
            "PreToolUse",
            json!({"session_id":"s","turnId":"zcode-turn","turn_id":"claude-turn"}),
        )
        .unwrap();
        let wrong_field = if client == "zcode" {
            "turn_id"
        } else {
            "turnId"
        };
        ingest_hook_at(
            &fixture.root,
            client,
            "PreToolUse",
            json!({"session_id":"s",(wrong_field):"other-client-field"}),
        )
        .unwrap();
    }
    let text = fs::read_to_string(events_path(&fixture.root)).unwrap();
    let events: Vec<ActivityEvent> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events[0].turn_id.as_deref(), Some("claude-turn"));
    assert_eq!(events[1].turn_id, None);
    assert_eq!(events[2].turn_id.as_deref(), Some("zcode-turn"));
    assert_eq!(events[3].turn_id, None);
}

#[test]
fn claude_child_hooks_require_the_exact_native_agent_and_never_change_the_root() {
    let fixture = Fixture::new();
    let at = now();
    fixture.jsonl(".claude/projects/project/session.jsonl", &[
        json!({"type":"user","uuid":"root-user","sessionId":"s","timestamp":at,"message":{"content":"private"}}),
    ]);
    let hook = |name, input| ingest_hook_at(&fixture.root, "claude", name, input).unwrap();
    hook(
        "PermissionRequest",
        json!({"session_id":"s","tool_use_id":"root-call"}),
    );
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        1
    );
    hook("PreToolUse", json!({"session_id":"s","agent_id":"child-a"}));
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.needs_action, 1);
    assert_eq!(snapshot.summary.running, 0);
    hook(
        "PostToolUse",
        json!({"session_id":"s","tool_use_id":"root-call"}),
    );
    hook(
        "PermissionRequest",
        json!({"session_id":"s","agent_id":"child-a","turn_id":"root-user"}),
    );
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.running, 1);
    assert_eq!(snapshot.client_summaries.claude.running, 1);
    assert_eq!(snapshot.client_summaries.zcode.running, 0);
    assert_eq!(snapshot.client_summaries.codex.running, 0);
    assert_eq!(snapshot.summary.needs_action, 0);

    fixture.jsonl(".claude/projects/project/session/subagents/agent-a.jsonl", &[
        json!({"type":"user","uuid":"child-user","agentId":"child-a","isSidechain":true,"sessionId":"s","timestamp":at,"message":{"content":"private child"}}),
    ]);
    hook(
        "PermissionRequest",
        json!({"session_id":"s","agent_id":"child-a"}),
    );
    hook(
        "PreToolUse",
        json!({"session_id":"s","agent_id":"unknown-child"}),
    );
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.running, 1);
    assert_eq!(snapshot.summary.needs_action, 0);
    let child = fixture
        .service
        .execution("claude:s:child-a/child-user")
        .unwrap()
        .unwrap();
    assert_eq!(child.status, TaskStatus::NeedsAction);
    assert!(child.live);
    assert_eq!(
        fixture
            .service
            .execution("claude:s:root-user")
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Running
    );
    let events = fs::read_to_string(events_path(&fixture.root)).unwrap();
    assert!(!events.contains("private"));
    assert!(events.contains("child-a"));
}

#[test]
fn native_ask_user_hooks_and_approvals_resolve_only_the_matching_tool() {
    for client in ["claude", "zcode"] {
        let fixture = Fixture::new();
        if client == "claude" {
            fixture.jsonl(".claude/projects/project/session.jsonl", &[
                json!({"type":"user","uuid":"turn","sessionId":"session","timestamp":now(),"message":{"content":"PRIVATE PROMPT"}}),
            ]);
        } else {
            fixture
                .zcode()
                .execute(
                    "INSERT INTO session(id,title,directory,parent_id) VALUES ('session','Conversation','/tmp/project',NULL)",
                    [],
                )
                .unwrap();
        }
        let hook = |event: &str, name: &str, id: &str| {
            let mut input = json!({"session_id":"session"});
            if client == "claude" {
                input["turn_id"] = json!("turn");
                input["tool_name"] = json!(name);
                input["tool_use_id"] = json!(id);
                input["tool_input"] =
                    json!({"questions":["PRIVATE QUESTION"],"command":"PRIVATE COMMAND"});
            } else {
                input["turnId"] = json!("turn");
                input["timestamp"] = json!(now());
                input["toolName"] = json!(name);
                input["toolCallId"] = json!(id);
                input["toolInput"] =
                    json!({"questions":["PRIVATE QUESTION"],"command":"PRIVATE COMMAND"});
            }
            ingest_hook_at(&fixture.root, client, event, input).unwrap();
        };
        let execution = format!("{client}:session:turn");
        hook("UserPromptSubmit", "", "");
        assert_eq!(fixture.service.sync(&[], &[]).unwrap().summary.running, 1);
        hook("PreToolUse", "AskUserQuestion", "question-1");
        let snapshot = fixture.service.sync(&[], &[]).unwrap();
        assert_eq!(snapshot.summary.needs_action, 1, "{client}");
        assert_eq!(
            fixture
                .service
                .execution(&execution)
                .unwrap()
                .unwrap()
                .attention,
            Some(AttentionKind::UserInput)
        );
        hook("PreToolUse", "AskUserQuestion", "question-2");
        hook("PreToolUse", "Read", "read");
        hook("PostToolUse", "Read", "read");
        assert_eq!(
            fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
            1
        );
        hook("PostToolUse", "AskUserQuestion", "question-1");
        assert_eq!(
            fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
            1
        );
        hook("PostToolUse", "AskUserQuestion", "question-2");
        assert_eq!(fixture.service.sync(&[], &[]).unwrap().summary.running, 1);
        assert_eq!(
            fixture
                .service
                .execution(&execution)
                .unwrap()
                .unwrap()
                .attention,
            None
        );
        hook("PermissionRequest", "Bash", "approval");
        hook("PostToolUse", "Read", "unrelated");
        assert_eq!(
            fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
            1
        );
        assert_eq!(
            fixture
                .service
                .execution(&execution)
                .unwrap()
                .unwrap()
                .attention,
            Some(AttentionKind::Approval)
        );
        hook("PostToolUseFailure", "Bash", "approval");
        assert_eq!(fixture.service.sync(&[], &[]).unwrap().summary.running, 1);
        let metadata = fs::read_to_string(events_path(&fixture.root)).unwrap();
        assert!(!metadata.contains("PRIVATE"));
        assert!(metadata.contains("question-1"));
        let database = fs::read(fixture.root.join("monitor/activity.db")).unwrap();
        assert!(!String::from_utf8_lossy(&database).contains("PRIVATE"));
    }
}

#[test]
fn claude_sidechain_uses_exact_prompt_identity_not_the_latest_or_nearest_turn() {
    let fixture = Fixture::new();
    let at = now();
    fixture.jsonl(".claude/projects/project/s.jsonl", &[
        json!({"type":"custom-title","sessionId":"s","customTitle":"Canonical conversation"}),
        json!({"type":"user","uuid":"u1","promptId":"p1","sessionId":"s","timestamp":at,"message":{"content":"private"}}),
        json!({"type":"assistant","sessionId":"s","timestamp":at,"message":{"id":"r1","stop_reason":"end_turn"}}),
        json!({"type":"user","uuid":"u2","promptId":"p2","sessionId":"s","timestamp":at,"message":{"content":"private"}}),
        json!({"type":"assistant","sessionId":"s","timestamp":at,"message":{"id":"r2","stop_reason":"end_turn"}}),
    ]);
    fixture.jsonl(".claude/projects/project/s/subagents/agent-child.jsonl", &[
        json!({"type":"user","uuid":"cu","promptId":"p1","agentId":"child","isSidechain":true,"sessionId":"s","timestamp":at,"message":{"content":"private"}}),
        json!({"type":"assistant","sessionId":"s","timestamp":at,"message":{"id":"cr","stop_reason":"end_turn"}}),
    ]);
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.total_turns, 2);
    let child = fixture
        .service
        .execution("claude:s:child/cu")
        .unwrap()
        .unwrap();
    assert_eq!(child.root_id.as_deref(), Some("claude:s:u1"));
    assert_eq!(child.title, "Canonical conversation");
    assert_eq!(
        fixture
            .service
            .execution("claude:s:u1")
            .unwrap()
            .unwrap()
            .child_count,
        1
    );
    assert_eq!(
        fixture
            .service
            .execution("claude:s:u2")
            .unwrap()
            .unwrap()
            .child_count,
        0
    );
}

#[test]
fn current_hook_attention_is_live_but_old_open_turns_remain_unknown_after_restart() {
    let fixture = Fixture::new();
    let at = now();
    fixture.jsonl(".claude/projects/project/session.jsonl",&[
        json!({"type":"user","uuid":"u1","sessionId":"s","timestamp":at,"message":{"content":"private"}}),
    ]);
    assert_eq!(fixture.service.sync(&[], &[]).unwrap().summary.running, 0);
    let event = |name: &str| ActivityEvent {
        client: "claude".into(),
        session_id: "s".into(),
        turn_id: None,
        event: name.into(),
        observed_at: now(),
        native_timestamp: None,
        tool_call_id: Some("call".into()),
        is_child: Some(false),
        agent_id: None,
        process: None,
    };
    fixture
        .service
        .record_event(event("PermissionRequest"))
        .unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.needs_action, 1);
    fixture.service.record_event(event("PostToolUse")).unwrap();
    assert_eq!(fixture.service.sync(&[], &[]).unwrap().summary.running, 1);
    std::thread::sleep(std::time::Duration::from_millis(3));
    let reopened =
        ActivityService::open(&fixture.root, fixture.root.join("monitor/activity.db")).unwrap();
    let snapshot = reopened.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.running, 0);
    assert_eq!(snapshot.summary.needs_action, 0);
    assert_eq!(snapshot.executions[0].status, TaskStatus::Unknown);
    reopened
        .record_event(ActivityEvent {
            observed_at: now(),
            ..event("StopFailure")
        })
        .unwrap();
    assert_eq!(reopened.sync(&[], &[]).unwrap().summary.failed_today, 1);
}

#[test]
fn unbound_hook_waits_for_the_native_input_record_instead_of_being_lost() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join(".claude/projects")).unwrap();
    let input_time = now();
    fixture
        .service
        .record_event(ActivityEvent {
            client: "claude".into(),
            session_id: "s".into(),
            turn_id: None,
            event: "PermissionRequest".into(),
            observed_at: now(),
            native_timestamp: None,
            tool_call_id: None,
            is_child: Some(false),
            agent_id: None,
            process: None,
        })
        .unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        0
    );
    fixture.jsonl(".claude/projects/project/session.jsonl",&[
        json!({"type":"user","uuid":"new-input","sessionId":"s","timestamp":input_time,"message":{"content":"private"}}),
    ]);
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.needs_action, 1);
    assert_eq!(snapshot.executions[0].turn_id, "new-input");
}

#[test]
fn event_lease_expiry_or_unavailable_source_never_leaves_a_task_falsely_live() {
    let fixture = Fixture::new();
    let old = (Utc::now() - Duration::seconds(180)).to_rfc3339_opts(SecondsFormat::Millis, true);
    fixture.state_started_at(&old);
    fixture.jsonl(".claude/projects/project/session.jsonl", &[
        json!({"type":"user","uuid":"u1","sessionId":"s","timestamp":old,"message":{"content":"private"}}),
    ]);
    let expired = (Utc::now() - Duration::seconds(LIVE_EVIDENCE_SECONDS + 1))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    fixture
        .service
        .record_event(ActivityEvent {
            client: "claude".into(),
            session_id: "s".into(),
            turn_id: None,
            event: "PreToolUse".into(),
            observed_at: expired.clone(),
            native_timestamp: None,
            tool_call_id: None,
            is_child: Some(false),
            agent_id: None,
            process: None,
        })
        .unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.running, 0);
    assert_eq!(snapshot.executions[0].status, TaskStatus::Unknown);
    assert_eq!(snapshot.executions[0].observed_at, expired);
    fixture
        .service
        .record_event(ActivityEvent {
            client: "claude".into(),
            session_id: "s".into(),
            turn_id: None,
            event: "PermissionRequest".into(),
            observed_at: now(),
            native_timestamp: None,
            tool_call_id: None,
            is_child: Some(false),
            agent_id: None,
            process: None,
        })
        .unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        1
    );
    fs::remove_dir_all(fixture.root.join(".claude")).unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.needs_action, 0);
    assert_eq!(snapshot.executions[0].status, TaskStatus::Unknown);
    assert_eq!(snapshot.summary.completed_today, 0);
}

#[test]
fn zcode_history_refresh_does_not_rewrite_turn_metadata_for_every_request() {
    let fixture = Fixture::new();
    let db = fixture.zcode();
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    db.execute(
        "INSERT INTO session(id,title,directory,parent_id) VALUES('s','Conversation','/tmp/project',NULL)",
        [],
    )
    .unwrap();
    let at = Utc::now().timestamp_millis();
    db.execute(
        "INSERT INTO turn_usage VALUES('s','t1','completed',?1,?2,1000,1000,1000,0,0,0,2000)",
        params![at - 2000, at],
    )
    .unwrap();
    for index in 0..1000 {
        db.execute(
            "INSERT INTO model_usage VALUES(?1,'s','t1','provider',?2,?3,?3,1,1,0,0,0,2)",
            params![
                format!("r{index:04}"),
                format!("model-{index}"),
                at - 1000 + index
            ],
        )
        .unwrap();
    }
    let first = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(first.summary.tokens.total, Some(2000));
    assert_eq!(first.executions[0].model.as_deref(), Some("model-999"));
    let changes = || {
        fixture
            .service
            .state
            .lock()
            .unwrap()
            .db
            .query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))
            .unwrap()
    };
    let before = changes();
    // A source WAL change invalidates the import without changing any request.
    db.execute("INSERT INTO session_task_link VALUES('unrelated',NULL)", [])
        .unwrap();
    let refreshed = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(refreshed.summary.tokens.total, Some(2000));
    assert_eq!(refreshed.executions[0].model.as_deref(), Some("model-999"));
    assert!(
        changes() - before < 20,
        "unchanged history must not issue one turn-metadata write per request; observed {} writes",
        changes() - before
    );
}

#[test]
fn zcode_cache_imports_wal_only_changes_and_keeps_corrected_zero_usage() {
    let fixture = Fixture::new();
    let db = fixture.zcode();
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    db.execute(
        "INSERT INTO session(id,title,directory,parent_id) VALUES('s','Conversation','/tmp/project',NULL)",
        [],
    )
    .unwrap();
    let at = Utc::now().timestamp_millis();
    db.execute(
        "INSERT INTO turn_usage VALUES('s','t1','completed',?1,?2,1000,100,20,0,0,0,120)",
        params![at - 1000, at],
    )
    .unwrap();
    db.execute(
        "INSERT INTO model_usage VALUES('r1','s','t1','provider','model',?1,?2,100,20,0,0,0,120)",
        params![at - 1000, at],
    )
    .unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        Some(120)
    );
    let path = fixture.root.join(".zcode/cli/db/db.sqlite");
    let before = common::sqlite_version(&path).unwrap();
    db.execute(
        "UPDATE model_usage SET input_tokens=0,output_tokens=0,computed_total_tokens=0",
        [],
    )
    .unwrap();
    db.execute("UPDATE turn_usage SET status='error'", [])
        .unwrap();
    let after = common::sqlite_version(&path).unwrap();
    assert!(
        before[0] == after[0],
        "the main SQLite file should remain unchanged"
    );
    assert!(
        before[1] != after[1],
        "the changed WAL must invalidate the import"
    );
    let updated = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(updated.summary.tokens.total, Some(0));
    assert_eq!(updated.executions[0].status, TaskStatus::Failed);
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        Some(0)
    );
}

#[test]
fn zcode_uses_request_rows_once_instead_of_adding_turn_aggregates() {
    let fixture = Fixture::new();
    let db = fixture.zcode();
    db.execute(
        "INSERT INTO session(id,title,directory,parent_id) VALUES ('s','Conversation','/tmp/project',NULL)",
        [],
    )
    .unwrap();
    let at = Utc::now().timestamp_millis();
    db.execute(
        "INSERT INTO turn_usage VALUES ('s','t1','completed',?1,?2,1000,9999,9999,0,0,0,19998)",
        params![at - 1000, at],
    )
    .unwrap();
    db.execute(
        "INSERT INTO model_usage VALUES ('r1','s','t1','provider','model',?1,?2,100,20,5,0,40,120)",
        params![at - 1000, at],
    )
    .unwrap();
    db.execute(
        "INSERT INTO session(id,title,directory,parent_id) VALUES ('c','Child','/tmp/project','s')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO turn_usage VALUES ('c','ct1','completed',?1,?2,500,888,888,0,0,0,1776)",
        params![at - 800, at - 300],
    )
    .unwrap();
    db.execute(
        "INSERT INTO model_usage VALUES ('r2','c','ct1','provider','model',?1,?2,20,10,2,0,0,30)",
        params![at - 800, at - 300],
    )
    .unwrap();
    db.execute("INSERT INTO tool_usage VALUES ('s','t1','call1')", [])
        .unwrap();
    db.execute("INSERT INTO dwf_run VALUES ('run1','s','call1')", [])
        .unwrap();
    db.execute("INSERT INTO dwf_actor VALUES ('c','run1')", [])
        .unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.total_turns, 1);
    assert_eq!(snapshot.summary.tokens.total, Some(150));
    let root = fixture.service.execution("zcode:s:t1").unwrap().unwrap();
    assert_eq!(root.tokens.input, Some(120));
    assert_eq!(root.tokens.reasoning, Some(7));
    assert_eq!(root.duration_ms, Some(1000));
    assert_eq!(root.child_count, 1);
    assert_eq!(
        fixture
            .service
            .execution("zcode:c:ct1")
            .unwrap()
            .unwrap()
            .parent_id
            .as_deref(),
        Some("zcode:s:t1")
    );
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        Some(150)
    );
}

#[test]
fn zcode_hooked_turn_is_live_before_request_or_terminal_usage_rows_exist() {
    let fixture = Fixture::new();
    let db = fixture.zcode();
    db.execute(
        "INSERT INTO session(id,title,directory,parent_id) VALUES ('s','Conversation','/tmp/project',NULL)",
        [],
    )
    .unwrap();
    let at = Utc::now().timestamp_millis();
    let started = common::utc_ms(at - 3000).unwrap();
    ingest_hook_at(
        &fixture.root,
        "zcode",
        "UserPromptSubmit",
        json!({
            "session_id":"s","turnId":"t1","timestamp":started,
            "prompt":"PRIVATE PROMPT"
        }),
    )
    .unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.running, 1);
    let pending = fixture.service.execution("zcode:s:t1").unwrap().unwrap();
    assert_eq!(snapshot.client_summaries.zcode.running, 1);
    assert_eq!(pending.status, TaskStatus::Running);
    assert!(pending.live);
    assert_eq!(pending.started_at.as_deref(), Some(started.as_str()));
    assert!(pending.duration_ms.unwrap() >= 3000);
    assert_eq!(pending.tokens.total, None);

    db.execute(
        "INSERT INTO turn_usage VALUES ('s','older','completed',?1,?2,1000,0,0,0,0,0,0)",
        params![at - 10000, at - 9000],
    )
    .unwrap();
    db.execute("INSERT INTO model_usage VALUES ('request','s','t1','provider','model',?1,?2,100,20,0,0,40,120)", params![at - 2800, at - 2000]).unwrap();
    for _ in 0..2 {
        let snapshot = fixture.service.sync(&[], &[]).unwrap();
        assert_eq!(snapshot.summary.running, 1);
        let active = fixture.service.execution("zcode:s:t1").unwrap().unwrap();
        assert_eq!(active.started_at.as_deref(), Some(started.as_str()));
        assert_eq!(active.tokens.total, Some(120));
    }
    let native_start = common::utc_ms(at - 3100).unwrap();
    db.execute(
        "INSERT INTO turn_usage VALUES ('s','t1','completed',?1,?2,3100,100,20,0,0,40,120)",
        params![at - 3100, at],
    )
    .unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.running, 0);
    let done = fixture.service.execution("zcode:s:t1").unwrap().unwrap();
    assert_eq!(done.status, TaskStatus::Succeeded);
    assert!(!done.live);
    assert_eq!(done.started_at.as_deref(), Some(native_start.as_str()));
    assert_eq!(done.duration_ms, Some(3100));
    assert_eq!(done.tokens.total, Some(120));
    ingest_hook_at(
        &fixture.root,
        "zcode",
        "UserPromptSubmit",
        json!({"session_id":"s","turnId":"t2"}),
    )
    .unwrap();
    assert_eq!(
        fixture
            .service
            .sync(&[], &[])
            .unwrap()
            .client_summaries
            .zcode
            .running,
        1
    );
    let missing_start = fixture.service.execution("zcode:s:t2").unwrap().unwrap();
    assert!(missing_start.live);
    assert_eq!(missing_start.started_at, None);
    assert_eq!(missing_start.duration_ms, None);
}

#[test]
fn zcode_native_timestamp_is_metadata_and_does_not_replace_observation_time() {
    let fixture = Fixture::new();
    let native = (Utc::now() - Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let observed_after = now();
    ingest_hook_at(
        &fixture.root,
        "zcode",
        "UserPromptSubmit",
        json!({
            "session_id":"session","turnId":"turn","timestamp":native,
            "prompt":"SECRET PROMPT","tool_input":{"command":"SECRET COMMAND"},
            "api_key":"SECRET KEY","transcript_path":"/SECRET/transcript"
        }),
    )
    .unwrap();
    let text = fs::read_to_string(events_path(&fixture.root)).unwrap();
    let event: ActivityEvent = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(event.native_timestamp.as_deref(), Some(native.as_str()));
    assert!(event.observed_at >= observed_after);
    assert_ne!(event.observed_at, native);
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("transcript"));
    ingest_hook_at(
        &fixture.root,
        "zcode",
        "UserPromptSubmit",
        json!({
            "session_id":"session","turnId":"other-turn","timestamp":1792633541972_i64
        }),
    )
    .unwrap();
    let text = fs::read_to_string(events_path(&fixture.root)).unwrap();
    let numeric: ActivityEvent = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    assert_eq!(numeric.native_timestamp, None);
}

#[test]
fn client_summaries_use_the_whole_ledger_unique_requests_and_actual_completion_day() {
    let fixture = Fixture::new();
    let midnight = Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .single()
        .unwrap();
    let yesterday = (midnight - Duration::minutes(5))
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let today = (midnight + Duration::minutes(5))
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let record = |client: &str, session: &str, child, status, end: &str| CollectedExecution {
        record: ExecutionRecord {
            id: execution_id(client, session, "turn"),
            client: client.into(),
            session_id: session.into(),
            turn_id: "turn".into(),
            root_id: None,
            parent_id: None,
            is_child: child,
            title: "Fixture metadata".into(),
            project_path: "/tmp/project".into(),
            model: None,
            provider: None,
            turn_index: 1,
            status,
            attention: None,
            started_at: Some(yesterday.clone()),
            completed_at: Some(end.into()),
            observed_at: end.into(),
            duration_ms: Some(0),
            active_ms: None,
            waiting_ms: None,
            tokens: TokenUsage::default(),
            live: false,
            child_count: 0,
        },
        relation: Relation::default(),
    };
    {
        let state = fixture.service.state.lock().unwrap();
        for index in 0..502 {
            store::upsert(
                &state.db,
                &record(
                    "codex",
                    &format!("c{index}"),
                    false,
                    TaskStatus::Succeeded,
                    if index == 0 { &today } else { &yesterday },
                ),
            )
            .unwrap();
        }
        let mut child = record("codex", "child", true, TaskStatus::Succeeded, &today);
        child.relation.parent_session = Some("c0".into());
        child.relation.parent_native_turn = Some("turn".into());
        store::upsert(&state.db, &child).unwrap();
        store::upsert(
            &state.db,
            &record("claude", "root", false, TaskStatus::Succeeded, &today),
        )
        .unwrap();
        store::upsert(
            &state.db,
            &record("claude", "child", true, TaskStatus::Failed, &today),
        )
        .unwrap();
        store::upsert(
            &state.db,
            &record("zcode", "yesterday", false, TaskStatus::Failed, &yesterday),
        )
        .unwrap();
        store::upsert(
            &state.db,
            &record("zcode", "today", false, TaskStatus::Failed, &today),
        )
        .unwrap();
        let usage = |id: &str, client: &str, session: &str, tokens| UsageRecord {
            id: id.into(),
            client: client.into(),
            execution_id: execution_id(client, session, "turn"),
            recorded_at: today.clone(),
            tokens,
        };
        let root_usage = usage(
            "codex-request",
            "codex",
            "c0",
            TokenUsage {
                input: Some(100),
                output: Some(20),
                cache_read: Some(40),
                cache_write: Some(0),
                total: Some(120),
                input_includes_cache: true,
                ..TokenUsage::default()
            },
        );
        store::usage(&state.db, &root_usage).unwrap();
        store::usage(&state.db, &root_usage).unwrap();
        store::usage(
            &state.db,
            &usage(
                "child-request",
                "codex",
                "child",
                TokenUsage {
                    input: Some(10),
                    output: Some(5),
                    cache_read: Some(0),
                    cache_write: Some(0),
                    total: Some(15),
                    input_includes_cache: true,
                    ..TokenUsage::default()
                },
            ),
        )
        .unwrap();
        store::usage(
            &state.db,
            &usage(
                "zero-request",
                "zcode",
                "today",
                TokenUsage {
                    input: Some(0),
                    output: Some(0),
                    cache_read: Some(0),
                    cache_write: Some(0),
                    total: Some(0),
                    ..TokenUsage::default()
                },
            ),
        )
        .unwrap();
    }
    let snapshot = fixture.service.snapshot().unwrap();
    assert_eq!(snapshot.executions.len(), 500);
    assert_eq!(snapshot.summary.total_turns, 505);
    assert_eq!(snapshot.summary.completed_today, 2);
    assert_eq!(snapshot.summary.failed_today, 1);
    assert_eq!(snapshot.summary.tokens.total, Some(135));
    assert_eq!(snapshot.summary.token_coverage.recorded_executions, 3);
    assert_eq!(snapshot.summary.token_coverage.total_executions, 507);
    let codex = &snapshot.client_summaries.codex;
    assert_eq!(codex.total_turns, 502);
    assert_eq!(codex.completed_today, 1);
    assert_eq!(codex.failed_today, 0);
    assert_eq!(codex.tokens.input, Some(70));
    assert_eq!(codex.tokens.total, Some(135));
    assert_eq!(codex.token_coverage.recorded_executions, 2);
    assert_eq!(codex.token_coverage.total_executions, 503);
    let claude = &snapshot.client_summaries.claude;
    assert_eq!(claude.total_turns, 1);
    assert_eq!(claude.completed_today, 1);
    assert_eq!(claude.failed_today, 0);
    assert_eq!(claude.tokens.total, None);
    assert_eq!(claude.token_coverage.recorded_executions, 0);
    assert_eq!(claude.token_coverage.total_executions, 2);
    let zcode = &snapshot.client_summaries.zcode;
    assert_eq!(zcode.total_turns, 2);
    assert_eq!(zcode.failed_today, 1);
    assert_eq!(zcode.tokens.total, Some(0));
    assert_eq!(zcode.token_coverage.recorded_executions, 1);
    assert_eq!(zcode.token_coverage.total_executions, 2);
    let wire = serde_json::to_value(snapshot).unwrap();
    assert_eq!(wire["clientSummaries"].as_object().unwrap().len(), 3);
}

#[test]
fn codex_zstd_archives_are_decoded_without_writing_raw_transcripts() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let path = fixture.add_codex_conversation(&state, "root", json!("cli"));
    let archive = path.with_extension("jsonl.zst");
    let value = codex_usage("root", "t1", "r1", 100, 40, 10);
    let data = format!("{}\n{}\n", value, value);
    fs::write(
        &archive,
        zstd::stream::encode_all(data.as_bytes(), 1).unwrap(),
    )
    .unwrap();
    fs::remove_file(&path).unwrap();
    state
        .execute(
            "UPDATE threads SET rollout_path=?1",
            [archive.to_str().unwrap()],
        )
        .unwrap();
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 20,
        Some(at - 1),
        "t1",
        1,
    );
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.tokens.total, Some(110));
    assert_eq!(
        snapshot
            .sources
            .iter()
            .find(|source| source.client == "codex")
            .unwrap()
            .state,
        "available"
    );
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.tokens.total,
        Some(110)
    );
    assert!(!path.exists());
}

#[test]
#[cfg(target_os = "macos")]
fn anchored_hook_checks_process_birth_and_current_native_turn_for_long_running_work() {
    let fixture = Fixture::new();
    let old = (Utc::now() - Duration::seconds(180)).to_rfc3339_opts(SecondsFormat::Millis, true);
    fixture.state_started_at(&old);
    let transcript = fixture.jsonl(".claude/projects/project/session.jsonl", &[
        json!({"type":"user","uuid":"u1","sessionId":"s","timestamp":old,"message":{"content":"private"}}),
    ]);
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let anchor = process::identity(child.id()).expect("macOS process metadata");
    let expired = (Utc::now() - Duration::seconds(LIVE_EVIDENCE_SECONDS + 1))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let event = ActivityEvent {
        client: "claude".into(),
        session_id: "s".into(),
        turn_id: Some("u1".into()),
        event: "PermissionRequest".into(),
        observed_at: expired.clone(),
        native_timestamp: None,
        tool_call_id: None,
        is_child: Some(false),
        agent_id: None,
        process: Some(anchor.clone()),
    };
    fixture.service.record_event(event.clone()).unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(snapshot.summary.needs_action, 1);
    assert!(snapshot.executions[0].duration_ms.unwrap() >= 180_000);
    let reopened =
        ActivityService::open(&fixture.root, fixture.root.join("monitor/activity.db")).unwrap();
    assert_eq!(reopened.sync(&[], &[]).unwrap().summary.needs_action, 1);
    let mut reused = anchor.clone();
    reused.started_microseconds += 1;
    fixture
        .service
        .record_event(ActivityEvent {
            process: Some(reused),
            ..event.clone()
        })
        .unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        0
    );
    fixture.service.record_event(event.clone()).unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        1
    );
    append(
        &transcript,
        &[
            json!({"type":"user","uuid":"u2","sessionId":"s","timestamp":now(),"message":{"content":"private"}}),
        ],
    );
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        0
    );
    fixture
        .service
        .record_event(ActivityEvent {
            turn_id: Some("u2".into()),
            observed_at: now(),
            ..event
        })
        .unwrap();
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        1
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!process::is_alive(&anchor));
    assert_eq!(
        fixture.service.sync(&[], &[]).unwrap().summary.needs_action,
        0
    );
}

#[test]
fn cached_reads_keep_request_corrections_and_deletions_consistent_across_views() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let path = fixture.add_codex_conversation(&state, "root", json!("cli"));
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "root",
        "t1",
        "completed",
        at - 10,
        Some(at),
        "t1",
        1,
    );
    append(&path, &[codex_usage("root", "t1", "r1", 100, 0, 10)]);
    let first = fixture.service.sync(&[], &[]).unwrap();
    assert_eq!(first.summary.tokens.total, Some(110));
    assert_eq!(fixture.service.snapshot().unwrap(), first);
    let id = execution_id("codex", "root", "t1");
    assert_eq!(
        fixture
            .service
            .execution(&id)
            .unwrap()
            .unwrap()
            .tokens
            .total,
        Some(110)
    );
    {
        let state = fixture.service.state.lock().unwrap();
        state.db.execute("UPDATE activity_requests SET tokens=json_set(tokens,'$.total',0,'$.input',0,'$.output',0) WHERE id='codex:root:r1'", []).unwrap();
    }
    assert_eq!(
        fixture.service.snapshot().unwrap().summary.tokens.total,
        Some(0)
    );
    assert_eq!(
        fixture
            .service
            .history(ActivityQuery::default())
            .unwrap()
            .executions[0]
            .tokens
            .total,
        Some(0)
    );
    assert_eq!(
        fixture
            .service
            .execution(&id)
            .unwrap()
            .unwrap()
            .tokens
            .total,
        Some(0)
    );
    {
        let state = fixture.service.state.lock().unwrap();
        state
            .db
            .execute("DELETE FROM activity_requests", [])
            .unwrap();
    }
    assert_eq!(
        fixture.service.snapshot().unwrap().summary.tokens.total,
        None
    );
    assert_eq!(
        fixture
            .service
            .execution(&id)
            .unwrap()
            .unwrap()
            .tokens
            .total,
        None
    );
}

#[test]
#[ignore = "Reads actual local metadata; prints counts and exact workspace identity/status only"]
fn local_source_smoke() {
    let service = ActivityService::open_default().unwrap();
    let snapshot = service.sync(&[], &[]).unwrap();
    println!(
        "activity smoke: roots={} recent={} token_total={:?} coverage={}/{}",
        snapshot.summary.total_turns,
        snapshot.executions.len(),
        snapshot.summary.tokens.total,
        snapshot.summary.token_coverage.recorded_executions,
        snapshot.summary.token_coverage.total_executions,
    );
    for source in snapshot.sources {
        println!("{}: {} ({})", source.client, source.state, source.detail);
    }
    assert!(snapshot.summary.total_turns > 0);
    for client in ["codex", "claude", "zcode"] {
        let history = service
            .history(ActivityQuery {
                client: Some(client.into()),
                limit: Some(10_000),
                ..ActivityQuery::default()
            })
            .unwrap();
        assert!(history.total > 0);
        let children = history
            .executions
            .iter()
            .filter(|record| record.is_child)
            .count();
        let linked = history
            .executions
            .iter()
            .filter(|record| record.is_child && record.root_id.is_some())
            .count();
        println!(
            "{client}: executions={} children={} exact_root_links={}",
            history.total, children, linked
        );
        if client == "zcode" {
            let workspace = dirs::home_dir().unwrap().join(".zcode/workspace/default");
            let mut scoped: Vec<_> = history
                .executions
                .iter()
                .filter(|record| Path::new(&record.project_path) == workspace)
                .collect();
            scoped.sort_by(|left, right| right.observed_at.cmp(&left.observed_at));
            if let Some(latest) = scoped.first() {
                println!(
                    "zcode workspace metadata: {}",
                    json!({
                        "workspace":workspace,"executionId":latest.id,"sessionId":latest.session_id,
                        "turnId":latest.turn_id,"status":status_name(&latest.status),"live":latest.live,
                        "startedAt":latest.started_at,"completedAt":latest.completed_at,
                        "durationMs":latest.duration_ms,"tokens":latest.tokens,
                        "clientSummary":snapshot.client_summaries.zcode,
                    })
                );
            }
        }
    }
}

#[test]
fn archived_conversation_is_hidden_while_recorded_usage_is_retained() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let path = fixture.jsonl(".codex/sessions/archived.jsonl", &[]);
    state.execute("INSERT INTO threads(id,title,cwd,rollout_path,model_provider,model,source,updated_at,archived) VALUES('archived','Archived conversation','/tmp/project',?1,'provider','model','\"cli\"',?2,0)", params![path.to_str(),Utc::now().timestamp()]).unwrap();
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        "archived",
        "t1",
        "completed",
        at - 10,
        Some(at),
        "t1",
        1,
    );
    append(&path, &[codex_usage("archived", "t1", "r1", 100, 0, 10)]);
    assert_eq!(fixture.service.sync(&[], &[]).unwrap().executions.len(), 1);
    state.execute("UPDATE threads SET archived=1", []).unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert!(
        snapshot.executions.is_empty(),
        "archived conversation still visible"
    );
    assert_eq!(
        snapshot
            .daily
            .iter()
            .filter_map(|day| day.tokens.total)
            .sum::<u64>(),
        110
    );
    assert!(fixture
        .service
        .history(ActivityQuery::default())
        .unwrap()
        .executions
        .is_empty());
    state.execute("UPDATE threads SET archived=0", []).unwrap();
    assert_eq!(fixture.service.sync(&[], &[]).unwrap().executions.len(), 1);
    state.execute("DELETE FROM threads", []).unwrap();
    assert!(fixture
        .service
        .sync(&[], &[])
        .unwrap()
        .executions
        .is_empty());
    assert_eq!(
        fixture.service.snapshot().unwrap().summary.tokens.total,
        Some(110)
    );
}

#[test]
fn zcode_archive_delete_and_restore_follow_native_wal_flags_without_erasing_usage() {
    let fixture = Fixture::new();
    let db = fixture.zcode();
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    db.execute("INSERT INTO session(id,title,directory,parent_id) VALUES('s','Conversation','/tmp/project',NULL)",[]).unwrap();
    let at = Utc::now().timestamp_millis();
    db.execute(
        "INSERT INTO turn_usage VALUES('s','t','completed',?1,?1,0,100,20,0,0,0,120)",
        [at],
    )
    .unwrap();
    db.execute(
        "INSERT INTO model_usage VALUES('r','s','t','provider','model',?1,?1,100,20,0,0,0,120)",
        [at],
    )
    .unwrap();
    assert_eq!(fixture.service.sync(&[], &[]).unwrap().executions.len(), 1);
    for column in ["time_archived", "time_deleted"] {
        db.execute(&format!("UPDATE session SET {column}=?1"), [at])
            .unwrap();
        let snapshot = fixture.service.sync(&[], &[]).unwrap();
        assert!(snapshot.executions.is_empty());
        assert_eq!(snapshot.summary.tokens.total, Some(120));
        assert_eq!(
            fixture
                .service
                .history(ActivityQuery::default())
                .unwrap()
                .total,
            0
        );
        db.execute(&format!("UPDATE session SET {column}=NULL"), [])
            .unwrap();
        assert_eq!(fixture.service.sync(&[], &[]).unwrap().executions.len(), 1);
    }
    db.execute("DELETE FROM session", []).unwrap();
    assert!(fixture
        .service
        .sync(&[], &[])
        .unwrap()
        .executions
        .is_empty());
}

#[test]
fn daily_usage_groups_requests_and_completions_by_their_actual_local_day() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let path = fixture.add_codex_conversation(&state, "s", json!("cli"));
    let today = Local::now().date_naive();
    let start = Local
        .from_local_datetime(&today.pred_opt().unwrap().and_hms_opt(23, 59, 0).unwrap())
        .single()
        .unwrap();
    let end = Local
        .from_local_datetime(&today.and_hms_opt(0, 1, 0).unwrap())
        .single()
        .unwrap();
    fixture.add_turn(
        &history,
        "s",
        "t",
        "completed",
        start.timestamp(),
        Some(end.timestamp()),
        "t",
        1,
    );
    let mut usage = codex_usage("s", "t", "r", 100, 40, 20);
    usage["timestamp"] = json!(end.to_rfc3339());
    append(&path, &[usage]);
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    let day = snapshot
        .daily
        .iter()
        .find(|day| day.date == today.to_string())
        .unwrap();
    assert_eq!(day.tokens.total, Some(120));
    assert_eq!(day.tokens.input, Some(60));
    assert_eq!(day.turns, 0);
    assert_eq!(
        day.completed, 1,
        "completion belongs to completion date, not submission date"
    );
}

#[test]
fn claude_removed_local_conversation_is_hidden_but_usage_survives() {
    let fixture = Fixture::new();
    let path=fixture.jsonl(".claude/projects/project/session.jsonl",&[
        json!({"type":"user","uuid":"u","sessionId":"s","timestamp":now(),"message":{"content":"PRIVATE"}}),
        json!({"type":"assistant","sessionId":"s","timestamp":now(),"message":{"id":"r","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"stop_reason":"end_turn"}}),
    ]);
    assert_eq!(fixture.service.sync(&[], &[]).unwrap().executions.len(), 1);
    fs::remove_file(path).unwrap();
    let snapshot = fixture.service.sync(&[], &[]).unwrap();
    assert!(snapshot.executions.is_empty());
    assert_eq!(snapshot.summary.tokens.total, Some(12));
}

#[test]
#[ignore = "Reads local sources into an explicitly supplied isolated ledger copy"]
fn local_visibility_and_usage_audit() {
    let path =
        std::env::var("AGENT_MONITOR_ACTIVITY_QA_DB").expect("isolated ledger path required");
    let service = ActivityService::open(dirs::home_dir().unwrap(), path).unwrap();
    let snapshot = service.sync(&[], &[]).unwrap();
    assert!(snapshot
        .sources
        .iter()
        .all(|source| source.state == "available"));
    let mut state = service.state.lock().unwrap();
    let hidden = store::hidden_sessions(&state.db).unwrap();
    assert!(snapshot
        .executions
        .iter()
        .all(|record| !hidden.contains(&(record.client.clone(), record.session_id.clone()))));
    let requests = store::usages(&state.db).unwrap();
    let ids: HashSet<_> = derived_activity(&mut state)
        .unwrap()
        .records
        .iter()
        .map(|record| record.id.clone())
        .collect();
    for client in ["codex", "claude", "zcode"] {
        let assigned: Vec<_> = requests
            .iter()
            .filter(|request| request.client == client && ids.contains(&request.execution_id))
            .collect();
        let normalized = TokenUsage::sum(assigned.iter().map(|request| &request.tokens), true);
        let daily: Vec<_> = snapshot
            .daily
            .iter()
            .filter(|day| day.client == client)
            .collect();
        assert_eq!(
            normalized.total,
            Some(daily.iter().filter_map(|day| day.tokens.total).sum())
        );
        println!(
            "{}",
            json!({"client":client,"requests":assigned.len(),"tokens":normalized,"hiddenConversations":hidden.iter().filter(|(source,_)|source==client).count()})
        );
    }
}

#[test]
#[ignore = "Audits conversation pagination in an explicitly supplied isolated local ledger copy"]
fn local_conversation_history_audit() {
    let path =
        std::env::var("AGENT_MONITOR_ACTIVITY_QA_DB").expect("isolated ledger path required");
    let service = ActivityService::open(dirs::home_dir().unwrap(), path).unwrap();
    let roots = service
        .history(ActivityQuery {
            roots_only: Some(true),
            limit: Some(0),
            ..Default::default()
        })
        .unwrap();
    let mut seen = HashSet::new();
    let mut turns = 0;
    let mut offset = 0;
    loop {
        let page = service
            .sessions(ActivityQuery {
                offset: Some(offset),
                limit: Some(17),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total_turns, roots.total);
        for session in &page.sessions {
            assert!(!session.latest.is_child);
            assert!(seen.insert((
                session.latest.client.clone(),
                session.latest.session_id.clone()
            )));
            turns += session.turn_count;
        }
        offset += page.sessions.len() as u64;
        if offset >= page.total {
            break;
        }
        assert!(!page.sessions.is_empty());
    }
    assert_eq!(turns, roots.total);
    let mut child_offset = 0;
    let mut linked = 0;
    let mut unassigned = 0;
    let mut child_ids = HashSet::new();
    loop {
        let page = service
            .history(ActivityQuery {
                children_only: Some(true),
                offset: Some(child_offset),
                limit: Some(200),
                ..Default::default()
            })
            .unwrap();
        for child in &page.executions {
            assert!(child.is_child);
            assert!(child_ids.insert(child.id.clone()));
            if child.root_id.is_some() {
                linked += 1;
            } else {
                unassigned += 1;
            }
        }
        child_offset += page.executions.len() as u64;
        if child_offset >= page.total {
            break;
        }
        assert!(!page.executions.is_empty());
    }
    println!(
        "session-audit {}",
        json!({"visibleConversations":seen.len(),"visibleRootRounds":turns,"childRecords":child_ids.len(),"linkedChildren":linked,"unassignedChildren":unassigned,"duplicates":0,"productionModified":false})
    );
}

#[test]
fn resumed_codex_history_uses_rollout_run_identity_but_keeps_conversation_and_usage_identity() {
    let fixture = Fixture::new();
    let (state, history) = fixture.codex();
    let session = "01a158b5-42da-7e73-bdaf-91895cb36ace";
    let run = "01a16672-52e1-7df0-9b98-63a9846c7e40";
    fixture.add_codex_conversation(&state, session, json!("vscode"));
    let path = fixture.jsonl(
        &format!(".codex/sessions/rollout-2026-10-23T09-48-50-{session}_{run}.jsonl"),
        &[],
    );
    state
        .execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            params![path.to_str(), session],
        )
        .unwrap();
    let at = Utc::now().timestamp();
    fixture.add_turn(
        &history,
        run,
        "resumed-turn",
        "inProgress",
        at,
        None,
        "resumed-turn",
        1,
    );
    append(
        &path,
        &[codex_usage(
            session,
            "resumed-turn",
            "response",
            100,
            40,
            20,
        )],
    );
    let snapshot = fixture
        .service
        .sync(
            &[],
            &[crate::codex::SessionActivity {
                session_id: session.into(),
                turn_id: "resumed-turn".into(),
                attention: Some(AttentionKind::UserInput),
            }],
        )
        .unwrap();
    let execution = fixture
        .service
        .execution(&execution_id("codex", session, "resumed-turn"))
        .unwrap()
        .unwrap();
    assert_eq!(
        execution.started_at.as_deref(),
        Some(
            Utc.timestamp_opt(at, 0)
                .unwrap()
                .to_rfc3339_opts(SecondsFormat::Millis, true)
                .as_str()
        ),
        "native run turn must populate the canonical conversation ledger"
    );
    assert_eq!(execution.tokens.total, Some(120));
    assert_eq!(execution.status, TaskStatus::NeedsAction);
    assert_eq!(snapshot.summary.total_turns, 1);
    assert_eq!(snapshot.summary.tokens.total, Some(120));
    assert_eq!(snapshot.summary.needs_action, 1);
}
