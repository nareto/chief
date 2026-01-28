#!/usr/bin/env python3
"""
chief.py - TDD Orchestrator for Claude Code

Runs a Red-Green-Refactor cycle using a coding agent.
Loads todos from todos.json and processes them by priority.
This is agnostic of the testing framework: test setup and running commands are loaded from chief.toml
"""

import sys
from abc import ABC, abstractmethod
import typing as t
import sqlite3
from dataclasses import dataclass
from functools import partial
from enum import Enum
from textwrap import dedent
import atexit

# TODO: decide on sqlite schema. one table with "type" column or different tables one per type? 



# GEMINI SQLite Schema Recommendations
# For this architecture, you need to track two very different types of data:
# 1. **State (Todos):** The "Current Truth" of what needs to be done.
# 2. **History (Logs):** The "Stream of Events" that happened (prompts, errors, tool outputs).
# I recommend two separate tables. Putting them in one table is a mistake because logs grow infinitely, while todos are a fixed set of tasks.
# #### Table 1: `todos`
# This is your "Queue". It mirrors your `todos.json` but adds execution state.
# ```sql
# CREATE TABLE IF NOT EXISTS todos (
#     uuid TEXT PRIMARY KEY,             -- Unique ID (matches json)
#     task TEXT NOT NULL,                -- The requirement
#     status TEXT NOT NULL,              -- 'pending', 'in_progress', 'done', 'failed'
#     file_checksum TEXT,                -- Hash of the file state (to detect manual edits)
#     attempts INTEGER DEFAULT 0,        -- How many times Chief tried this
#     last_phase TEXT,                   -- 'red' or 'green'
#     created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
#     updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
# );
# ```
# #### Table 2: `run_logs`
# This is your "Black Box Recorder". Every time the agent does something, you record it here. This is vital for the "Retry Logic" (reading the last 500 lines).
# ```sql
# CREATE TABLE IF NOT EXISTS run_logs (
#     id INTEGER PRIMARY KEY AUTOINCREMENT,
#     run_id TEXT NOT NULL,              -- A UUID for this specific execution cycle of Chief
#     todo_uuid TEXT,                    -- Foreign Key: Which task was being worked on?
#     event_type TEXT NOT NULL,          -- 'agent_input', 'agent_output', 'tool_call', 'error', 'system'
#     phase TEXT,                        -- 'red', 'green', 'refactor'
#     content TEXT,                      -- The actual prompt, code, or error message
#     timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
#     FOREIGN KEY(todo_uuid) REFERENCES todos(uuid)
# );
# ```
# #### Why this schema?
# 1. **Retry Intelligence:** When your `main()` loop catches an `AgentCapitulationError`, it can do:
# `SELECT content FROM run_logs WHERE todo_uuid = ? ORDER BY timestamp DESC LIMIT 1` to get the exact error output to feed into the next prompt.
# 2. **Analytics:** You can run `SELECT AVG(attempts) FROM todos` to see how hard your tasks are on average.
# 3. **Concurrency:** If you ever run parallel agents, `run_id` lets you separate their logs.



### ---
# implementation guidelines: 
# - use static typing 
# - when many groups of arguments are often passed together, create a dataclass and use that instead
### ---


### DATA MODELS
@dataclass
class TestSuite:
    name: str
    # TODO: other fields from chief.toml.example

    def to_prompt_str(self) -> str:
        return f"name: {self.name}\n" #???

@dataclass 
class Todo:
    task: str
    expectations: str
    # TODO: other fields from todos.json.example

    def to_prompt_str(self) -> str:
        return f"task: {self.task}\nexpectations: {self.expectations}" #???

class Phase(Enum):
    red = "red"
    green = "green"

@dataclass
class PrevRunFail:
    output_tail: str
    failed_at_phase: Phase


### END DATA MODELS


### EXCEPTIONS
# You are moving from Fail-Stop (stop on any unknown error to prevent corruption) to Fail-Safe (assume the agent was just being silly/stochastic, revert, and try again). Given that LLMs are non-deterministic, this is actually the preferred strategy. If the agent writes bad code that crashes your parser, running it again might yield valid code.

class ChiefError(Exception):
    """Base class for all application errors."""
    pass


class AgentCapitulationError(ChiefError):
    """
    RETRYABLE: The agent failed to complete the task.
    
    This wraps two types of failures:
    1. Explicit: The agent output "NO CHANGES" when it shouldn't have, or linter failed.
    2. Implicit: The agent produced garbage output that caused a parsing exception.
    
    Payload:
    - output_tail: The last N lines of logs/output to show the agent in the next attempt.
    - phase: 'red', 'green', or 'refactor'
    """
    def __init__(self, message: str, phase: str, output_tail: str):
        super().__init__(message)
        self.phase = phase
        self.output_tail = output_tail


class EnvironmentError(ChiefError):
    """
    FATAL: Something is wrong with the setup.
    (DB locked, API key missing, Validation failed).
    """
    pass


### END EXCEPTIONS


### CORE LOGIC

class CodingAgent(ABC):
    def run(self, disallowed_files: t.List[str], prompt: str, ??) -> str:
        """Invoke coding agent with prompt, while disallowing it to edit disallowed_files files. Returns full output of agent."""
        # TODO: log: timestamp, type=agent_input, processed todo uuid, phase (red/green), full prompt being passed to agent
        output = self._run(??)
        # TODO: log: timestamp, type=agent_output, full_output, any tool calls, any tool calls results??

    @abstractmethod
    def _run(self, disallowed_files: t.List[str], prompt: str) -> str:
        pass


class ClaudeCode(CodingAgent):
    def _run(self, disallowed_files: t.List[str], prompt: str):
        # TODO: use subprocess to invoke `claude` with appropriate options and capture output

@dataclass
class StabilityLoop:
    """Manages stability loops"""
    iteration: t.Callable[[],t.Any]
    required_stable_iterations: int # required consecutive iterations that satisfy all stability conditions
    stability_conditions: t.List[t.Callable[[], bool]] # returns True if current iteration is stable (e.g., no files were modified)
    post_iteration_callback: t.Optional[t.Callable[[], None]]
    max_loops: t.Optional[int] = None

    def run(self, ):
        counter = 0:
        stable_iterations = 0
        while True:
            self.iteration(self.iteration_kwargs)
            counter += 1
            if self.max_loops is not None and counter >= self.max_loops:
                break
            if all(cond() for cond in self.stability_conditions):
                stable_iterations += 1
                if stable_iterations > self.required_stable_iterations:
                    break
            else:
                stable_iterations = 0
            if self.post_iteration_callback is not None:
                self.post_iteration_callback(self.post_iteration_callback_kwargs)


@dataclass
class TodoProcessor:
    coding_agent: CodingAgent

    RED_PROMPT: str = dedent("""
    We are doing TDD and are in Red Phase for task:
    {task_info}

    {action_instruction} the tests for this task. The functionality is not there yet, so the tests are supposed to fail. 

    The tests should be written for the following test suites:
    {test_suites_info}

    And they should be:
    - comprehensive 
    - cover edge cases

    {previous_error_block}

    If you had to make no modifications, output simply "NO CHANGES"
    """)

    RED_PROMPT_ERROR_BLOCK: str = dedent("""
        Keep in mind that the previous development cycle tackling this task failed in {phase} phase with error:
        {error_message}
    """)

    GREEN_PROMPT: str = dedent(""" """)

    def process(self,prev_run_fail: t.Optional[PrevRunFail] = None)-> None:
        try:
            self.red_phase(prev_run_fail)
        except Exception as e:
            # log exception
            # log "FAILED RED PHASE FOR {TEST UUID}"
            # If it's already a controlled error, re-raise it
            if isinstance(e, ChiefError): 
                raise e
            
            # If it's a generic coding failure (e.g. timeout, bad output), wrap it
            # This is the signal to MAIN to retry
            raise AgentCapitulationError(
                message="Agent failed Red Phase",
                phase="red",
                output_tail= ??? # TODO: extract this from the agent output
            )

        try:
            self.green_phase()
        except Exception as e:
            # log exception
            # log "FAILED GREEN PHASE FOR {TEST UUID}"
            # If it's already a controlled error, re-raise it
            if isinstance(e, ChiefError): 
                raise e            
            raise AgentCapitulationError("Agent failed Green Phase", "green", ???)


    def red_phase(self):
        stability_loop = StabilityLoop(
            iteration = partial(self._red_phase_iteration, arg1=?? ),
            iteration_kwargs = ??,
            required_stable_iterations = 2,
            stability_conditions = [
                # linting of modified files,
                # output ends with "NO CHANGES"
            ]
            stability_conditions_kwargs = ??
            # post_iteration_callback: t.Optional[t.Callable]
            # post_iteration_callback_kwargs: t.Optional[t.Dict]
            max_loops = 10
            )
        stability_loop.run()

    def _red_phase_build_prompt(self, 
                                task: Todo, 
                                test_suites_info: t.List[TestSuite],
                                tests_exist: bool, 
                                prev_run_fail: t.Optional[PrevRunFail] = None
                                ) -> str:
            action_verb = "Complete" if tests_exist else "Write"
            
            if prev_run_fail is not None:
                error_block = self.RED_PROMPT_ERROR_BLOCK.format(
                    phase="red", 
                    error_message=prev_run_fail.output_tail
                )
            else:
                # The "Ghost" block (invisible)
                error_block = ""

            # 3. Inject everything into the Master Template
            # Note: We use .format() so we can pass arguments by name
            return RED_PHASE_TEMPLATE.format(
                task_info=task.to_prompt_str,
                action_instruction=action_verb,
                test_suites_info="\n\n".join(ts.to_prompt_str for ts in test_suites_info),
                previous_error_section=error_block
            )

    def _red_phase_iteration(self, arg1):
        self.coding_agent.run(
            prompt=self._red_phase_build_prompt(???),
            disallowed_files=??
        )


                    
class Chief:
    def prepare(self, options):
        # TODO: Load chief.toml (see chief.toml.examples)
        # TODO: Validate that all test suite commands can execute. If a suite fails validation and has a test_init command, run the init commands for the suties and retry. Exits with error if any suite fails validation after init attempt.
        # TODO: Load todos.json (see todos.json.example). If all todos are done: log, exit

    def run(self, max_iter = 10):
        # TODO: LOG RUN START
        # Process todos by priority
        completed_count = 0
        while True: #iterate on todos
            # TODO: Reload todos.json each iteration to pick up any external changes
            # TODO: get highest priority todo to process. if empty, log and return 0
            # TODO: if the current todo is the same as the one that made the previous run fail (if it failed)
            # we pass prev_failed_run_data to todoprocessor. if it is not, then we run GitOperations.revert_changes to start 
            # from a clean state (any work done for another todo isn't relevant anymore)
            if prev_todo.uuid && todo.uuid == prev_todo.uuid:
                processor = TodoProcessor(todo, prev_failed_run_data)
            else: 
                GitOperations.revert_changes()
                processor = TodoProcessor(todo)
            success = processor.process()
            if success != 0:
                return success


def main():

    args = parse_args() #TODO
    # --clean-done: removes all todos from todos.json that have non-null "done_at_commit" field
    # if --prev-failed-run-phase is present, read and store the last 500 lines of the log file
    
    # State validation (Fatal if fails)
    try:
        Logger.setup(args.log_file, args.sqlite_file)
        # TODO: Add specific validation logic here (e.g. check API keys)
    except Exception as e:
        print(f"FATAL: System setup failed: {e}")
        sys.exit(1)

    prev_fail_info: t.Optional[PrevRunFail] = None

    for i in range(args.max_chief_retries) # default 10
        # TODO: we need two ways to break out of this loop. the first is,  the currentchief run fails because the coding agent simply failed at its task 
        # - in that case we continue and try again. the second case is there was some deterministic error (that we throw explicitly), like the 
        # sqlite db has wrong schema. In that case we should break this loop and sys.exit with failure (since it makes no sense to try again)
        try:
            chief = Chief(prev_fail_info)
            chief.prepare() # Validates DB, API keys, etc.
            chief.run()
            # If we get here, Success!
            Logger.info("Run completed successfully.")
            sys.exit(0)
        except EnvironmentError as e:
            # CASE A: We know exactly what this is, and it's unrecoverable.
            Logger.critical(f"System Configuration Error: {e}")
            sys.exit(1) # FATAL: Don't retry

        except Exception as e:
            # CASE B: The Catch-All (Agent failure, random crash, weird parsing error)
            # We assume this is due to the Agent's stochastic behavior.
            Logger.warn(f"Run {i+1} failed with exception: {type(e).__name__}: {e}")
            Logger.warn("Assuming transient agent failure. Retrying...")
            
            # 1. Extract the "Tail" for the prompt (last 500 lines)
            # If it was our wrapper error, we have the clean tail. 
            # If it was a random crash, we use the string representation of the crash.      
            if isinstance(e, AgentCapitulationError):
                output_tail = e.output_tail
                phase = Phase(e.phase)
            else:
                output_tail = f"Internal Exception: {str(e)}"
                phase = Phase.red # Default to red if we don't know

            # Capture the failure state for the next loop
            cur_fail_info = PrevRunFail(
                output_tail=output_tail, # last 500 lines of output...???
                failed_at_phase=phase
            )
            
            if cur_fail_info.output_tail == prev_fail_info.output_tail: # TODO: maybe 500 lines is too much to use as it as classifier for "it was the same error"
                GitOperations.revert_changes() # if we are failing just like before, let's start from a fresh slate
            prev_fail_info = cur_fail_info
            continue

    Logger.error("Max retries exceeded.")
    sys.exit(1)


### END CORE LOGIC

### UTILS

class Logger:
    """Centralized logging (log file and sqlite db) and output formatting."""
    _log_handle: t.Optional[t.TextIO] = None
    _db_conn: t.Optional[sqlite3.Connection] = None

    @classmethod
    def setup(cls, log_path: str, sqlite_path: str) -> None:
        """Initialize resources given file paths."""
        try:
            # buffering=1 means line-buffered (good for logs)
            cls._log_handle = open(log_path, "a", encoding="utf-8", buffering=1)
            cls._db_conn = sqlite3.connect(sqlite_path)
            atexit.register(cls.shutdown)
            
        except OSError as e:
            # If we can't open logs, this is a Fatal error (System Failure)
            print(f"CRITICAL: Could not open log files: {e}")
            sys.exit(1)

    @classmethod
    def shutdown(cls):
        """Cleanup resources."""
        if cls._log_handle:
            cls._log_handle.close()
        if cls._db_conn:
            cls._db_conn.close()

    @classmethod
    def write(cls, ??):
        # TODO: call self._write_log and self._write_db 

    @classmethod
    def _write_log(cls, text: str) -> None:
        """Write text to log file (without ANSI codes)."""
        if cls._log_handle:
            cls._log_handle.write(cls.strip_ansi(text))
            cls._log_handle.flush()

    @classmethod
    def _write_db(cls, ??) -> None:
        """Write text to sqlite db"""
        if cls._db_conn:
            cursor = cls.db_conn
            # TODO: append in db    

class GitOperations:
    """Collection of git-related operations."""

    @staticmethod
    def commit_and_tag(message: str) -> str:
        """
        Commit changes and create a tag. Returns: the commit hash
        """


    @staticmethod
    def push_with_tags() -> bool:
        """
        Push commits and tags to remote. Returns True if push succeeded, False if failed (non-fatal)
        """

    @staticmethod
    def revert_changes(exclude_files = ["todos.json", "chief.toml"]):
        """Revert uncommitted changes except: todos.json and chief.toml"""

### END UTILS

if __name__ == "__main__":
    sys.exit(main())
