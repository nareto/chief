use super::{
    ExecutionFlow, FlowExecution, ReviewContractViolation, ReviewFlow, ReviewSpec,
    convergence_not_reached,
};
use crate::agent::{AgentRequest, CodingAgent};
use crate::config::ChiefConfig;
use crate::domain::{AgentOutput, Todo, TodoStatus};
use crate::git::GitOps;
use crate::prompt::EmbeddedPromptStore;
use crate::review::load_findings;
use crate::storage::ProjectStore;
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const FINDINGS_RELATIVE: &str = ".chief/review/findings.yaml";

const ONE_FINDING: &str = "findings:
  - severity: P1
    file: src/lib.rs
    line: 3
    title: Off-by-one in range check
    why: The last element is skipped.
";

const TWO_FINDINGS: &str = "findings:
  - severity: P1
    file: src/lib.rs
    line: 3
    title: Off-by-one in range check
    why: The last element is skipped.
  - severity: P3
    file: README.md
    title: Typo
    why: Misspelled word.
    suggestion: Fix the spelling.
";

/// What the scripted agent does on one pass.
#[derive(Debug, Clone)]
enum Pass {
    /// Leave everything untouched.
    Nothing,
    /// Overwrite the findings file with this content.
    WriteFindings(&'static str),
    /// Mark a tracked file as modified (through the fake git status) and write it.
    EditTracked(&'static str),
    /// Move HEAD, as if the agent committed.
    Commit,
}

struct ScriptedAgent {
    findings_path: PathBuf,
    project_dir: PathBuf,
    passes: Mutex<Vec<Pass>>,
    prompts: Mutex<Vec<String>>,
    git: Arc<FakeGitState>,
}

impl ScriptedAgent {
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("prompts mutex").clone()
    }
}

impl CodingAgent for ScriptedAgent {
    fn name(&self) -> &str {
        "scripted"
    }

    fn run(&self, request: AgentRequest) -> Result<AgentOutput> {
        self.prompts
            .lock()
            .expect("prompts mutex")
            .push(request.prompt.clone());
        let pass = {
            let mut passes = self.passes.lock().expect("passes mutex");
            if passes.is_empty() {
                Pass::Nothing
            } else {
                passes.remove(0)
            }
        };
        match pass {
            Pass::Nothing => {}
            Pass::WriteFindings(content) => {
                fs::create_dir_all(self.findings_path.parent().expect("findings parent"))?;
                fs::write(&self.findings_path, content)?;
            }
            Pass::EditTracked(file) => {
                let path = self.project_dir.join(file);
                let previous = fs::read_to_string(&path).unwrap_or_default();
                fs::write(&path, format!("{previous}edited\n"))?;
                self.git
                    .tracked
                    .lock()
                    .expect("tracked mutex")
                    .insert(file.to_owned());
            }
            Pass::Commit => {
                *self.git.head.lock().expect("head mutex") = "agent-commit".to_owned();
            }
        }
        Ok(AgentOutput {
            exit_code: 0,
            process_exit_code: 0,
            command: "scripted-agent".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            merged_output: String::new(),
            warnings: Vec::new(),
        })
    }
}

struct FakeGitState {
    head: Mutex<String>,
    tracked: Mutex<BTreeSet<String>>,
    commits: Mutex<usize>,
}

struct FakeGit {
    root: PathBuf,
    state: Arc<FakeGitState>,
}

impl GitOps for FakeGit {
    fn repo_root(&self) -> &Path {
        &self.root
    }

    fn head_commit(&self, _cwd: &Path) -> Result<String> {
        Ok(self.state.head.lock().expect("head mutex").clone())
    }

    fn changed_files(&self, _cwd: &Path) -> Result<Vec<String>> {
        let mut files = self
            .state
            .tracked
            .lock()
            .expect("tracked mutex")
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        files.push(".chief/".to_owned());
        Ok(files)
    }

    fn tracked_changed_files(&self, _cwd: &Path) -> Result<Vec<String>> {
        Ok(self
            .state
            .tracked
            .lock()
            .expect("tracked mutex")
            .iter()
            .cloned()
            .collect())
    }

    fn diff(&self, _cwd: &Path, _against_ref: Option<&str>) -> Result<String> {
        Ok(String::new())
    }

    fn diff_summary_for_files(&self, _cwd: &Path, _files: &[String]) -> Result<String> {
        Ok(String::new())
    }

    fn commit_committer_timestamp_rfc3339(
        &self,
        _cwd: &Path,
        _commit_hash: &str,
    ) -> Result<String> {
        Ok("1970-01-01T00:00:00+00:00".to_owned())
    }

    fn commit_and_tag(&self, _cwd: &Path, _message: &str) -> Result<String> {
        *self.state.commits.lock().expect("commits mutex") += 1;
        Ok("unexpected-commit".to_owned())
    }

    fn commit_paths(&self, _cwd: &Path, _paths: &[&str], _message: &str) -> Result<()> {
        *self.state.commits.lock().expect("commits mutex") += 1;
        Ok(())
    }

    fn create_worktree(&self, _branch: &str, _worktree_path: &Path) -> Result<()> {
        Ok(())
    }

    fn merge_branch_into_main(&self, _branch: &str, _main_branch: &str) -> Result<()> {
        Ok(())
    }

    fn remove_worktree(&self, _worktree_path: &Path, _branch: &str) -> Result<()> {
        Ok(())
    }

    fn current_branch(&self) -> Result<String> {
        Ok("feature".to_owned())
    }
}

struct Harness {
    project_dir: PathBuf,
    findings_path: PathBuf,
    store: ProjectStore,
    prompts: EmbeddedPromptStore,
    agent: ScriptedAgent,
    git: FakeGit,
    git_state: Arc<FakeGitState>,
    config: ChiefConfig,
}

impl Harness {
    fn new(passes: Vec<Pass>, tracked_dirty: &[&str]) -> Self {
        let project_dir =
            std::env::temp_dir().join(format!("chief-review-flow-test-{}", Uuid::new_v4()));
        fs::create_dir_all(project_dir.join("src")).expect("project dir should be created");
        fs::write(project_dir.join("src/lib.rs"), "fn main() {}\n").expect("source file");
        let store = ProjectStore::new(&project_dir);
        store.init().expect("store init should succeed");
        let findings_path = project_dir.join(FINDINGS_RELATIVE);
        let git_state = Arc::new(FakeGitState {
            head: Mutex::new("base-head".to_owned()),
            tracked: Mutex::new(
                tracked_dirty
                    .iter()
                    .map(|file| (*file).to_owned())
                    .collect(),
            ),
            commits: Mutex::new(0),
        });
        let agent = ScriptedAgent {
            findings_path: findings_path.clone(),
            project_dir: project_dir.clone(),
            passes: Mutex::new(passes),
            prompts: Mutex::new(Vec::new()),
            git: git_state.clone(),
        };
        let git = FakeGit {
            root: project_dir.clone(),
            state: git_state.clone(),
        };
        Self {
            project_dir,
            findings_path,
            store,
            prompts: EmbeddedPromptStore::from_embedded_prompts().expect("embedded prompts"),
            agent,
            git,
            git_state,
            config: ChiefConfig::default(),
        }
    }

    fn run(&self, max_loop: usize, required_stable: usize) -> Result<()> {
        let spec = ReviewSpec {
            base_ref: "origin/main".to_owned(),
            merge_base: "abc123".to_owned(),
            findings_path: FINDINGS_RELATIVE.to_owned(),
            findings_abs_path: self.findings_path.clone(),
        };
        let mut execution = FlowExecution {
            run_id: "run-1".to_owned(),
            job_id: "job-1".to_owned(),
            worker_index: 1,
            project_dir: self.project_dir.clone(),
            store: &self.store,
            prompts: &self.prompts,
            agent: &self.agent,
            git: &self.git,
            chief_config: &self.config,
            all_suites: &[],
            todo: Todo {
                id: "review-todo".to_owned(),
                todo: "review".to_owned(),
                expectations: String::new(),
                priority: 1,
                test_suites: Vec::new(),
                status: TodoStatus::Pending,
                done_at_commit: None,
            },
            cancel_signal: Arc::new(AtomicBool::new(false)),
            prepared_suites: RefCell::new(BTreeSet::new()),
            convergence_watch_paths: vec![FINDINGS_RELATIVE.to_owned()],
        };
        let flow = ReviewFlow::new(spec, max_loop, required_stable);
        let outcome = flow.run(&mut execution)?;
        assert_eq!(
            outcome.commit_hash, None,
            "review must never report a commit"
        );
        Ok(())
    }

    fn commits(&self) -> usize {
        *self.git_state.commits.lock().expect("commits mutex")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.project_dir);
    }
}

#[test]
fn review_flow_stops_after_first_pass_without_new_findings() {
    let harness = Harness::new(
        vec![
            Pass::WriteFindings(ONE_FINDING),
            Pass::WriteFindings(TWO_FINDINGS),
            Pass::Nothing,
        ],
        &[],
    );

    harness.run(10, 1).expect("review should converge");

    let prompts = harness.agent.prompts();
    assert_eq!(
        prompts.len(),
        3,
        "two passes add findings, the third adds none"
    );
    assert!(prompts[0].contains("review pass 1"));
    assert!(prompts[2].contains("review pass 3"));
    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.contains("git diff abc123"))
    );
    let findings = load_findings(&harness.findings_path).expect("findings should parse");
    assert_eq!(findings.findings.len(), 2);
    assert_eq!(findings.blocking_count(), 1);
    assert_eq!(harness.commits(), 0, "review must not commit");
}

#[test]
fn review_flow_with_no_findings_converges_in_one_pass_without_creating_the_file() {
    let harness = Harness::new(vec![Pass::Nothing], &["src/lib.rs"]);

    harness.run(10, 1).expect("review should converge");

    assert_eq!(harness.agent.prompts().len(), 1);
    assert!(!harness.findings_path.exists());
    assert_eq!(harness.commits(), 0);
}

#[test]
fn review_flow_honors_required_stable_iterations() {
    let harness = Harness::new(vec![Pass::WriteFindings(ONE_FINDING)], &[]);

    harness.run(10, 2).expect("review should converge");

    assert_eq!(harness.agent.prompts().len(), 3);
}

#[test]
fn review_flow_reports_non_convergence_with_typed_error() {
    let harness = Harness::new(
        vec![
            Pass::WriteFindings(ONE_FINDING),
            Pass::WriteFindings(TWO_FINDINGS),
        ],
        &[],
    );

    let err = harness
        .run(2, 1)
        .expect_err("two busy passes should exhaust a cap of 2");

    let not_reached = convergence_not_reached(&err).expect("error should be typed");
    assert_eq!(not_reached.max_loops, 2);
    let findings = load_findings(&harness.findings_path).expect("findings should be kept");
    assert_eq!(findings.findings.len(), 2);
}

#[test]
fn review_flow_feeds_validation_errors_back_into_the_next_prompt() {
    let harness = Harness::new(
        vec![
            Pass::WriteFindings("findings:\n  - severity: P9\n    file: x\n"),
            Pass::WriteFindings(ONE_FINDING),
            Pass::Nothing,
        ],
        &[],
    );

    harness
        .run(10, 1)
        .expect("review should converge once the file is valid");

    let prompts = harness.agent.prompts();
    assert_eq!(prompts.len(), 3);
    assert!(!prompts[0].contains("could not parse the findings file"));
    assert!(prompts[1].contains("could not parse the findings file"));
    assert!(!prompts[2].contains("could not parse the findings file"));
}

#[test]
fn review_flow_does_not_converge_on_an_untouched_invalid_file() {
    let harness = Harness::new(
        vec![Pass::WriteFindings("findings: [not, a, finding]\n")],
        &[],
    );

    let err = harness
        .run(3, 1)
        .expect_err("an invalid findings file must never count as stable");

    assert!(convergence_not_reached(&err).is_some());
}

#[test]
fn review_flow_fails_when_agent_edits_other_tracked_files() {
    let harness = Harness::new(
        vec![
            Pass::WriteFindings(ONE_FINDING),
            Pass::EditTracked("src/lib.rs"),
        ],
        &[],
    );

    let err = harness
        .run(10, 1)
        .expect_err("editing src/lib.rs breaks the contract");

    let violation = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<ReviewContractViolation>())
        .expect("error should be a contract violation");
    assert!(violation.to_string().contains("src/lib.rs"));
    assert_eq!(harness.agent.prompts().len(), 2);
    assert_eq!(harness.commits(), 0);
}

#[test]
fn review_flow_tolerates_tracked_changes_that_existed_before_the_pass() {
    let harness = Harness::new(vec![Pass::WriteFindings(ONE_FINDING)], &["src/lib.rs"]);

    harness
        .run(10, 1)
        .expect("pre-existing uncommitted changes are part of the review, not a violation");
}

#[test]
fn review_flow_fails_when_agent_moves_head() {
    let harness = Harness::new(vec![Pass::Commit], &[]);

    let err = harness
        .run(10, 1)
        .expect_err("moving HEAD breaks the contract");

    assert!(
        err.chain()
            .any(|cause| cause.downcast_ref::<ReviewContractViolation>().is_some()),
        "unexpected error: {err:#}"
    );
    assert!(format!("{err:#}").contains("must not commit"));
}
