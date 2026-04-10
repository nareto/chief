use super::*;

mod agent_limits;
mod agent_runtime;
mod context;
mod history;
mod suite_commands;

pub struct FlowExecution<'a> {
    pub run_id: String,
    pub job_id: String,
    pub worker_index: usize,
    pub project_dir: PathBuf,
    pub store: &'a ProjectStore,
    pub prompts: &'a dyn PromptStore,
    pub agent: &'a dyn CodingAgent,
    pub git: &'a dyn GitOps,
    pub chief_config: &'a ChiefConfig,
    pub all_suites: &'a [TestSuiteConfig],
    pub todo: Todo,
    pub cancel_signal: Arc<AtomicBool>,
    pub(crate) prepared_suites: RefCell<BTreeSet<String>>,
    /// When non-empty, convergence is gated on changes to these paths only.
    /// Each entry is matched as an exact file path or directory prefix.
    pub convergence_watch_paths: Vec<String>,
}

impl<'a> FlowExecution<'a> {
    const RETRY_CLEANUP_DISCARDED_MSG_PREFIX: &'static str =
        "Retry cleanup: discarded local git changes before loop";
    const ITERATION_GIT_CHANGE_DETECTION_MSG: &'static str = "Iteration git change detection";
}
