use super::{ProjectStore, ReadinessStatus};
use crate::domain::{EventRecord, EventType, Todo, TodoStatus};
use crate::git::GIT_TRANSIENT_LOCK_RETRY_DELAY;
use chrono::Utc;
use rusqlite::{Connection, params};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

fn temp_project_dir() -> PathBuf {
    std::env::temp_dir().join(format!("chief-storage-test-{}", Uuid::new_v4()))
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-c")
        .arg("safe.directory=*")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: stdout={} stderr={}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn init_git_repo(project_dir: &Path) {
    run_git(project_dir, &["init"]);
    run_git(
        project_dir,
        &["config", "user.email", "chief-storage-tests@example.com"],
    );
    run_git(project_dir, &["config", "user.name", "Chief Storage Tests"]);
    run_git(project_dir, &["add", "--all"]);
    run_git(project_dir, &["commit", "-m", "chore: baseline"]);
}

fn write_todos(project_dir: &Path, todos_yaml: &str) {
    fs::write(project_dir.join("todos.yaml"), format!("{todos_yaml}\n"))
        .expect("failed to write todos.yaml");
}

#[test]
fn claim_todo_updates_sqlite_and_todos_file() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: String::new(),
        todo: "test atomic claim".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    }
    .normalize();
    let todo = store.append_todo(todo).expect("append_todo should succeed");

    let claimed = store
        .claim_todo(&todo.id)
        .expect("claim_todo should succeed")
        .expect("todo should be claimable");
    assert_eq!(claimed.status, TodoStatus::InProgress);

    let db_todo = store
        .list_todos()
        .expect("list_todos should succeed")
        .into_iter()
        .find(|item| item.id == todo.id)
        .expect("todo should exist in db");
    assert_eq!(db_todo.status, TodoStatus::InProgress);

    let file_todo = store
        .load_todo_file()
        .expect("load_todo_file should succeed")
        .todos
        .into_iter()
        .find(|item| item.id == todo.id)
        .expect("todo should exist in file");
    assert_eq!(file_todo.status, TodoStatus::InProgress);

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn readiness_state_defaults_to_not_ready_before_any_check() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let readiness = store
        .get_readiness_state()
        .expect("default readiness state should be readable");
    assert_eq!(readiness.status, ReadinessStatus::NotReady);
    assert_eq!(readiness.summary, "Readiness check has not run yet.");
    assert!(readiness.checked_at.is_none());
    assert!(readiness.checking_started_at.is_none());

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn readiness_state_transitions_from_checking_to_ready() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    store
        .set_readiness_checking("checking now")
        .expect("setting readiness checking should succeed");
    let checking = store
        .get_readiness_state()
        .expect("checking readiness state should be readable");
    assert_eq!(checking.status, ReadinessStatus::Checking);
    assert_eq!(checking.summary, "checking now");
    assert!(checking.checking_started_at.is_some());
    assert!(checking.checked_at.is_none());

    store
        .set_readiness_result(
            ReadinessStatus::Ready,
            "ready now",
            &json!({ "commands_total": 3, "commands_failed": 0 }),
        )
        .expect("setting readiness result should succeed");
    let ready = store
        .get_readiness_state()
        .expect("ready state should be readable");
    assert_eq!(ready.status, ReadinessStatus::Ready);
    assert_eq!(ready.summary, "ready now");
    assert!(ready.checked_at.is_some());
    assert!(ready.checking_started_at.is_none());
    assert_eq!(
        ready
            .details
            .get("commands_failed")
            .and_then(|value| value.as_i64()),
        Some(0)
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn claim_next_pending_todo_respects_priority_then_id_order() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let fixtures = vec![
        Todo {
            id: "todo-z".to_owned(),
            todo: "lower priority".to_owned(),
            expectations: String::new(),
            priority: 5,
            test_suites: Vec::new(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize(),
        Todo {
            id: "todo-b".to_owned(),
            todo: "highest priority second id".to_owned(),
            expectations: String::new(),
            priority: 9,
            test_suites: Vec::new(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize(),
        Todo {
            id: "todo-a".to_owned(),
            todo: "highest priority first id".to_owned(),
            expectations: String::new(),
            priority: 9,
            test_suites: Vec::new(),
            status: TodoStatus::Pending,
            done_at_commit: None,
        }
        .normalize(),
    ];

    for todo in fixtures {
        store
            .append_todo(todo)
            .expect("append_todo should succeed for fixture");
    }

    let first = store
        .claim_next_pending_todo()
        .expect("first claim should succeed")
        .expect("first claim should exist");
    let second = store
        .claim_next_pending_todo()
        .expect("second claim should succeed")
        .expect("second claim should exist");
    let third = store
        .claim_next_pending_todo()
        .expect("third claim should succeed")
        .expect("third claim should exist");
    let fourth = store
        .claim_next_pending_todo()
        .expect("fourth claim should succeed");

    assert_eq!(first.id, "todo-a");
    assert_eq!(second.id, "todo-b");
    assert_eq!(third.id, "todo-z");
    assert!(fourth.is_none(), "no more pending todos should remain");

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn claim_next_pending_todo_normalizes_legacy_attempted_status_before_claim() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: String::new(),
        todo: "legacy attempted".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    }
    .normalize();
    let todo = store.append_todo(todo).expect("append_todo should succeed");

    {
        let conn = Connection::open(&store.db_path).expect("failed to open sqlite db");
        conn.execute(
            "UPDATE todos SET status = 'attempted' WHERE id = ?1",
            params![&todo.id],
        )
        .expect("failed to force attempted legacy status");
    }

    let claimed = store
        .claim_next_pending_todo()
        .expect("claim_next_pending_todo should succeed")
        .expect("legacy attempted todo should be normalized and claimed");
    assert_eq!(claimed.id, todo.id);
    assert_eq!(claimed.status, TodoStatus::InProgress);

    let persisted = store
        .list_todos()
        .expect("list_todos should succeed")
        .into_iter()
        .find(|item| item.id == todo.id)
        .expect("todo should still exist");
    assert_eq!(
        persisted.status,
        TodoStatus::InProgress,
        "todo should be claimed after legacy status normalization"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn update_todo_status_fails_for_missing_todo() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let err = store
        .update_todo_status("missing-id", TodoStatus::Done, None)
        .expect_err("missing todo should return error");
    assert!(
        err.to_string().contains("not found"),
        "unexpected error message: {}",
        err
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn delete_todo_removes_from_sqlite_and_todos_file() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: String::new(),
        todo: "delete me".to_owned(),
        expectations: String::new(),
        priority: 1,
        test_suites: Vec::new(),
        status: TodoStatus::Pending,
        done_at_commit: None,
    }
    .normalize();
    let todo = store.append_todo(todo).expect("append_todo should succeed");

    store
        .delete_todo(&todo.id)
        .expect("delete_todo should succeed");

    let db_todo = store
        .list_todos()
        .expect("list_todos should succeed")
        .into_iter()
        .find(|item| item.id == todo.id);
    assert!(db_todo.is_none(), "todo should be removed from sqlite");

    let file_todo = store
        .load_todo_file()
        .expect("load_todo_file should succeed")
        .todos
        .into_iter()
        .find(|item| item.id == todo.id);
    assert!(
        file_todo.is_none(),
        "todo should be removed from todos.yaml"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn delete_done_todos_removes_only_done_items() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let pending = store
        .append_todo(
            Todo {
                id: "pending-item".to_owned(),
                todo: "keep pending".to_owned(),
                expectations: String::new(),
                priority: 1,
                test_suites: Vec::new(),
                status: TodoStatus::Pending,
                done_at_commit: None,
            }
            .normalize(),
        )
        .expect("append pending should succeed");
    let done = store
        .append_todo(
            Todo {
                id: "done-item".to_owned(),
                todo: "remove done".to_owned(),
                expectations: String::new(),
                priority: 2,
                test_suites: Vec::new(),
                status: TodoStatus::Done,
                done_at_commit: None,
            }
            .normalize(),
        )
        .expect("append done should succeed");

    let deleted = store
        .delete_done_todos()
        .expect("delete_done_todos should succeed");
    assert_eq!(deleted, 1, "exactly one done todo should be removed");

    let remaining = store.list_todos().expect("list_todos should succeed");
    assert!(
        remaining.iter().any(|item| item.id == pending.id),
        "pending todo should remain",
    );
    assert!(
        remaining.iter().all(|item| item.id != done.id),
        "done todo should be deleted",
    );

    let file_todos = store
        .load_todo_file()
        .expect("load_todo_file should succeed")
        .todos;
    assert!(
        file_todos.iter().any(|item| item.id == pending.id),
        "pending todo should remain in file",
    );
    assert!(
        file_todos.iter().all(|item| item.id != done.id),
        "done todo should be removed from file",
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn inconsistent_db_requires_confirmation_before_reset() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    let todo = Todo {
        id: String::new(),
        todo: "survives db reset".to_owned(),
        expectations: String::new(),
        priority: 3,
        test_suites: Vec::new(),
        status: TodoStatus::InProgress,
        done_at_commit: None,
    }
    .normalize();
    let todo = store.append_todo(todo).expect("append_todo should succeed");

    let conn = Connection::open(project_dir.join("chief.db")).expect("db should open");
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
            DROP TABLE todos;
            CREATE TABLE todos (
                id TEXT PRIMARY KEY,
                run_id TEXT
            );
            PRAGMA foreign_keys = ON;",
    )
    .expect("should create inconsistent schema");
    drop(conn);

    let err = store
        .sync_todos_from_file()
        .expect_err("sync_todos_from_file should require db reset");
    let reset_error = super::db_reset_required_from_anyhow(&err)
        .expect("error should carry db reset required details");
    assert_eq!(
        reset_error.db_path,
        project_dir.join("chief.db"),
        "reset-required error should include db path"
    );

    store
        .reset_db_from_todos_file()
        .expect("explicit db reset should succeed");

    let conn = Connection::open(project_dir.join("chief.db")).expect("db should reopen");
    let mut stmt = conn
        .prepare("PRAGMA table_info(todos)")
        .expect("table info should prepare");
    let todo_columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("table info should query")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("table info rows should parse");
    assert_eq!(
        todo_columns,
        vec![
            "id".to_owned(),
            "priority".to_owned(),
            "todo".to_owned(),
            "expectations".to_owned(),
            "test_suites".to_owned(),
            "status".to_owned(),
            "done_at_commit".to_owned(),
            "updated_at".to_owned(),
        ],
        "recreated todos table should match canonical schema"
    );

    let todos = store.list_todos().expect("list_todos should succeed");
    let recovered = todos.iter().find(|item| item.id == todo.id);
    assert!(
        recovered.is_some(),
        "todo should still exist after db reset"
    );
    assert_eq!(
        recovered.expect("todo should be present").status,
        TodoStatus::Pending,
        "in_progress todos should be re-queued to pending during reset"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn trim_events_keeps_only_latest_runs() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    for index in 1..=4 {
        let run_id = format!("run-{index}");
        store.start_run(&run_id).expect("start_run should succeed");
        store
            .record_event(&EventRecord {
                id: None,
                run_id: run_id.clone(),
                job_id: None,
                todo_id: None,
                timestamp: Utc::now(),
                level: "info".to_owned(),
                phase: None,
                msg: format!("event for {run_id}"),
                event_type: EventType::Msg,
                payload: BTreeMap::new(),
            })
            .expect("record_event should succeed");
    }

    let conn = Connection::open(project_dir.join("chief.db")).expect("db should open");
    conn.execute(
        "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-1'",
        ["2024-01-01T00:00:00Z"],
    )
    .expect("should update run-1 started_at");
    conn.execute(
        "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-2'",
        ["2024-01-02T00:00:00Z"],
    )
    .expect("should update run-2 started_at");
    conn.execute(
        "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-3'",
        ["2024-01-03T00:00:00Z"],
    )
    .expect("should update run-3 started_at");
    conn.execute(
        "UPDATE runs SET started_at = ?1 WHERE run_id = 'run-4'",
        ["2024-01-04T00:00:00Z"],
    )
    .expect("should update run-4 started_at");
    drop(conn);

    let deleted = store
        .trim_events_to_recent_runs(2)
        .expect("trim_events_to_recent_runs should succeed");
    assert_eq!(deleted, 2, "two older runs should be removed");

    let remaining = store
        .query_events(super::EventQuery {
            limit: 10,
            ..super::EventQuery::default()
        })
        .expect("query_events should succeed");
    let remaining_run_ids = remaining
        .into_iter()
        .map(|event| event.run_id)
        .collect::<Vec<_>>();
    assert_eq!(
        remaining_run_ids,
        vec!["run-4".to_owned(), "run-3".to_owned()],
        "only latest two runs should remain",
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn trim_events_reclaims_db_space() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    for index in 1..=8 {
        let run_id = format!("run-{index}");
        store.start_run(&run_id).expect("start_run should succeed");
        for event_index in 0..30 {
            store
                .record_event(&EventRecord {
                    id: None,
                    run_id: run_id.clone(),
                    job_id: None,
                    todo_id: None,
                    timestamp: Utc::now(),
                    level: "info".to_owned(),
                    phase: None,
                    msg: format!("event {event_index} {}", "x".repeat(4096)),
                    event_type: EventType::Msg,
                    payload: BTreeMap::new(),
                })
                .expect("record_event should succeed");
        }
    }

    let db_path = project_dir.join("chief.db");
    let size_before = fs::metadata(&db_path)
        .expect("db metadata before trim should be readable")
        .len();

    let deleted = store
        .trim_events_to_recent_runs(1)
        .expect("trim_events_to_recent_runs should succeed");
    assert!(deleted > 0, "trim should delete older events");

    let size_after = fs::metadata(&db_path)
        .expect("db metadata after trim should be readable")
        .len();
    assert!(
        size_after < size_before,
        "expected chief.db to shrink after trim (before={size_before}, after={size_after})"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn append_todo_auto_commits_todos_yaml_when_repo_available() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    init_git_repo(&project_dir);

    let before = run_git(&project_dir, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count should parse");

    store
        .append_todo(
            Todo {
                id: String::new(),
                todo: "auto commit from append".to_owned(),
                expectations: "status persisted".to_owned(),
                priority: 5,
                test_suites: Vec::new(),
                status: TodoStatus::Pending,
                done_at_commit: None,
            }
            .normalize(),
        )
        .expect("append_todo should succeed");

    let after = run_git(&project_dir, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count should parse");
    assert_eq!(after, before + 1, "append_todo should create one commit");

    let subject = run_git(&project_dir, &["log", "-1", "--pretty=%s"]);
    assert_eq!(subject, "chore(todos): sync todos.yaml");

    let files = run_git(
        &project_dir,
        &["show", "--name-only", "--pretty=format:", "HEAD"],
    );
    let changed_files = files
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(changed_files, vec!["todos.yaml"]);

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn append_todo_auto_commit_retries_transient_index_lock() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    init_git_repo(&project_dir);

    let before = run_git(&project_dir, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count should parse");
    let index_lock = project_dir.join(".git").join("index.lock");
    fs::write(&index_lock, "lock").expect("failed to seed index.lock");
    let lock_to_clear = index_lock.clone();
    let clear_lock = thread::spawn(move || {
        thread::sleep(
            GIT_TRANSIENT_LOCK_RETRY_DELAY
                .checked_sub(Duration::from_millis(5))
                .unwrap_or(GIT_TRANSIENT_LOCK_RETRY_DELAY),
        );
        let _ = fs::remove_file(lock_to_clear);
    });

    store
        .append_todo(
            Todo {
                id: String::new(),
                todo: "auto commit lock retry".to_owned(),
                expectations: "retries should recover".to_owned(),
                priority: 4,
                test_suites: Vec::new(),
                status: TodoStatus::Pending,
                done_at_commit: None,
            }
            .normalize(),
        )
        .expect("append_todo should succeed after transient lock retry");

    clear_lock.join().expect("lock clear thread should join");

    let after = run_git(&project_dir, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count should parse");
    assert_eq!(after, before + 1, "append_todo should still commit once");

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn sync_todos_from_file_auto_commits_external_todos_yaml_edit() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");
    init_git_repo(&project_dir);

    fs::write(
            project_dir.join("todos.yaml"),
            "todos:\n  - id: imported-todo\n    priority: 7\n    todo: imported from requirements\n    expectations: persisted after sync\n    test_suites: []\n    status: pending\n    done_at_commit: null\n",
        )
        .expect("manual todos.yaml edit should succeed");

    let before = run_git(&project_dir, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count should parse");

    store
        .sync_todos_from_file()
        .expect("sync_todos_from_file should succeed");

    let after = run_git(&project_dir, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count should parse");
    assert_eq!(
        after,
        before + 1,
        "sync_todos_from_file should commit external todos.yaml updates"
    );

    let todos = store.list_todos().expect("list_todos should succeed");
    assert!(
        todos.iter().any(|todo| todo.id == "imported-todo"),
        "synced todo should exist in sqlite"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn sync_todos_from_file_reconciles_add_update_remove_without_duplicates() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    write_todos(
        &project_dir,
        r#"todos:
  - id: keep-unchanged
    todo: Keep this todo exactly
    expectations: Keep this expectations text
    priority: 4
    test_suites: ["unit"]
    status: done
    done_at_commit: keep-commit
  - id: update-me
    todo: Old todo text
    expectations: Old expectations
    priority: 2
    test_suites: []
    status: pending
    done_at_commit: null
  - id: remove-me
    todo: Remove this todo
    expectations: Remove this expectations
    priority: 3
    test_suites: []
    status: attempted
    done_at_commit: null"#,
    );
    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite");

    let baseline_todos = store.list_todos().expect("list_todos should succeed");
    assert!(
        baseline_todos.iter().any(|todo| todo.id == "remove-me"),
        "baseline sync should include todos present in todos.yaml",
    );
    assert!(
        baseline_todos.iter().all(|todo| todo.id != "add-me"),
        "baseline sync should not include todos not yet added to todos.yaml",
    );

    let unchanged_before = baseline_todos
        .iter()
        .find(|todo| todo.id == "keep-unchanged")
        .cloned()
        .expect("baseline unchanged todo should exist");

    write_todos(
        &project_dir,
        r#"todos:
  - id: add-me
    todo: Newly added todo
    expectations: Added in yaml
    priority: 9
    test_suites: ["integration"]
    status: pending
    done_at_commit: null
  - id: update-me
    todo: Updated todo text
    expectations: Updated expectations
    priority: 1
    test_suites: ["smoke"]
    status: done
    done_at_commit: updated-commit
  - id: keep-unchanged
    todo: Keep this todo exactly
    expectations: Keep this expectations text
    priority: 4
    test_suites: ["unit"]
    status: done
    done_at_commit: keep-commit"#,
    );

    store
        .sync_todos_from_file()
        .expect("sync_todos_from_file should reconcile sqlite to file");

    let todos = store.list_todos().expect("list_todos should succeed");
    let ids = todos.iter().map(|todo| todo.id.clone()).collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            "add-me".to_owned(),
            "keep-unchanged".to_owned(),
            "update-me".to_owned()
        ],
        "todo ordering should remain priority DESC then id ASC after reconciliation"
    );
    assert_eq!(
        ids.iter().collect::<HashSet<_>>().len(),
        ids.len(),
        "reconciliation should not leave duplicate todo IDs in sqlite"
    );
    assert!(
        todos.iter().all(|todo| todo.id != "remove-me"),
        "todo removed from todos.yaml should be deleted from sqlite"
    );

    let updated = todos
        .iter()
        .find(|todo| todo.id == "update-me")
        .expect("updated todo should exist");
    assert_eq!(updated.todo, "Updated todo text");
    assert_eq!(updated.expectations, "Updated expectations");
    assert_eq!(updated.priority, 1);
    assert_eq!(updated.status, TodoStatus::Done);
    assert_eq!(updated.done_at_commit.as_deref(), Some("updated-commit"));

    let unchanged_after = todos
        .iter()
        .find(|todo| todo.id == "keep-unchanged")
        .expect("unchanged todo should still exist");
    assert_eq!(
        unchanged_after, &unchanged_before,
        "todo not changed in todos.yaml should keep persisted values after reconciliation"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn sync_todos_from_file_returns_error_for_invalid_yaml_without_mutating_sqlite() {
    let project_dir = temp_project_dir();
    let store = ProjectStore::new(&project_dir);
    store.init().expect("store init should succeed");

    write_todos(
        &project_dir,
        r#"todos:
  - id: baseline-todo
    todo: Baseline todo
    expectations: Baseline expectations
    priority: 3
    test_suites: []
    status: pending"#,
    );
    store
        .reset_db_from_todos_file()
        .expect("reset_db_from_todos_file should seed sqlite");

    let before = store.list_todos().expect("baseline todos should load");

    fs::write(
        project_dir.join("todos.yaml"),
        "todos:\n  - id: broken\n    todo: [missing quote\n",
    )
    .expect("failed to write invalid todos.yaml");

    let err = store
        .sync_todos_from_file()
        .expect_err("sync_todos_from_file should fail for invalid todos.yaml");
    assert!(
        err.to_string().contains("invalid YAML in"),
        "expected invalid YAML error, got: {err}",
    );

    let after = store
        .list_todos()
        .expect("sqlite should remain readable after sync error");
    assert_eq!(
        after, before,
        "failed sync should not mutate sqlite todo rows"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
