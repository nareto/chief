import sys

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
    def revert_changes(...):
        """Revert uncommitted changes except todos.json """


# class StabilityLoop:
#     """Manages stability and retry loops for Claude interactions."""


class TodoProcessor:
    def process(self,...):
        try:
            self.green_phase()
        except Exception as e:
            # log exception
            # log "FAILED GREEN PHASE FOR {TEST UUID}"
            return 1
        try:
            self.red_phase()
        except Exception as e:
            # log exception
            # log "FAILED GREEN PHASE FOR {TEST UUID}"
            return 1
        return 0
                    
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
            if prev_todo.uuid && todo.uuid == prev_todo:
                processor = TodoProcessor(todo, prev_failed_run_data)
            else: 
                GitOperations.revert_changes
                processor = TodoProcessor(todo)
            success = processor.process()
            if success != 0:
                return success


def main():
    args = parse_args() #TODO
    # --clean-done: removes all todos from todos.json that have non-null "done_at_commit" field
    # if --prev-failed-run-phase is present, read and store the last 500 lines of the log file

    prev_failed_run_phase = None
    prev_failed_run_logs = None
    for i in range(args.max_chief_retries) # default 10
        chief = Chief(prev_failed_run_phase, prev_failed_run_logs)
        chief.prepare()
        try:
            chief.run()
        except Exception as e:
            # TODO: log e
            # TODO: load last 500 lines of the logs into new_prev_failed_run_logs 
            if new_prev_failed_run_logs == prev_failed_run_logs: # TODO: fuzzy comparison somehow? maybe just check if they have the same tail?
                GitOperations.revert_changes() # if we are failing just like before, let's start from a fresh slate
            # TODO: parse last lines, looking for FAILED RED PHASE / FAILED GREEN PHASE and put "red" or "green" in prev_failed_run_phase
            continue


if __name__ == "__main__":
    sys.exit(main())
