# `chief review`

`chief review` asks an agent to review the current branch and record what it finds in a YAML file. It runs the same convergence loop as the other flows, but the agent may only write the findings file, and chief never commits.

```bash
chief review --agent claude --model claude-sonnet-5-5 --agent-sandbox native --max-loop-iterations 5
```

It works in any Git repository and does not need `.chief/chief.yaml`.

## What gets reviewed

Chief resolves a base ref, then reviews everything since `git merge-base <base> HEAD`. That includes commits on the branch and uncommitted changes to tracked files. The agent reads the change with `git diff <merge-base>`.

Untracked files do not show up in `git diff`. Run `git add -N <file>` (intent to add) to include a new file without staging its content.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--base <ref>` | first that exists of `origin/HEAD`, `origin/main`, `origin/master`, `main`, `master` | Ref to compare against. In CI, pass the PR base branch, for example `origin/$GITHUB_BASE_REF`. |
| `--findings <path>` | `.chief/review/findings.yaml` | Findings file. Relative paths are relative to `--project-dir`. Missing parent directories are created. |
| `--max-loop-iterations <n>` | `20`, or `chief.max_loop_iterations` from `.chief/chief.yaml` | Most passes to run. |
| `--required-stable-iterations <n>` | `1` | Consecutive passes without new findings needed to stop. `required_stable_iterations` in `chief.yaml` does not apply to review; only this flag does. |

The other global flags work as usual, for example `--agent`, `--model`, `--model-reasoning-effort`, `--agent-timeout-seconds`, `--agent-sandbox` and `--verbosity`. See [configuration.md](./configuration.md).

When the findings file is the default path, chief writes `.chief/review/.gitignore` containing `*`, so review output never shows up in `git status`. With a custom `--findings` path, ignoring the file is up to you.

Findings accumulate across runs: a second `chief review` keeps the existing entries and only adds new ones. Delete the file to start from scratch.

## How a review converges

1. Each pass starts a fresh agent with no memory of earlier passes. It gets the base, the merge-base commit and the findings path.
2. The agent reads the findings file, reviews the change and appends only findings that are not already listed.
3. If it has nothing new, it leaves the file untouched.
4. Chief compares the file before and after the pass. If the file did not change, the pass counts as stable. One stable pass ends the review (raise `--required-stable-iterations` to demand more).
5. If the file no longer parses, chief passes the parse error to the next agent and asks it to repair the file. An invalid file never counts as stable.

The fresh context matters: a new agent re-reads the change without the previous agent's assumptions, so later passes catch what earlier ones missed, and the findings file is the only memory between passes.

## Findings file

```yaml
findings:
  - severity: P1            # P0, P1, P2 or P3
    file: src/example.rs    # path relative to the repository root
    line: 42                # optional, 1-based line in the new version of the file
    title: One-line summary of the problem
    why: What goes wrong, for whom, and under which conditions.
    suggestion: Optional concrete fix or direction.
```

- `severity`, `file`, `title` and `why` are required and must not be empty.
- `line` is optional; when present it must be 1 or more.
- `suggestion` is optional.
- A missing file, an empty file and `findings: []` all mean "no findings".

### Severity

The rule the agent follows: should this stop the merge? If yes, P0 or P1. If no, P2 or P3.

| Severity | Blocks the merge | Typical examples |
| --- | --- | --- |
| P0 | yes | data loss, security hole, crash or corruption in a common path, broken build or release |
| P1 | yes | a real bug, a broken contract, a missing test for risky behavior |
| P2 | no | maintainability problems, unclear behavior in rare cases, gaps in docs or tests |
| P3 | no | naming, wording, small style points |

## Exit status and errors

`chief review` exits 0 when the review converges, whatever the findings say. Decide what blocks a merge by reading the findings file (the GitHub example fails on any P0/P1).

It exits 1 on real errors:

- the base ref does not resolve, or shares no history with `HEAD`;
- the existing findings file is invalid before the review starts (fix it or delete it);
- the agent fails, times out, or is not installed;
- the agent edits a tracked file other than the findings file, or moves `HEAD` (for example by committing). Chief stops and leaves those changes for you to inspect, and the message lists the files;
- the loop runs out of passes before a pass adds nothing new. Findings recorded so far stay in the file.

Your own uncommitted edits are fine. Chief snapshots the content of every modified tracked file before and after each pass and only complains about changes made during the pass, including further edits to files that were already dirty. Untracked files are not watched, so this is a contract check, not a sandbox.

## Sandboxing

A review needs only read access plus write access to one file inside the repository, so it is a good fit for `--agent-sandbox native` with Codex, Claude Code or Cursor. See [safety.md](./safety.md) for what each agent does in that mode.

## GitHub Actions

[`examples/github-review.yml`](../examples/github-review.yml) is a ready-to-copy workflow:

1. installs Claude Code, chief (`cargo install --locked chief --git https://github.com/nareto/chief`), and `bubblewrap` and `socat` for Claude Code's sandbox, then lets `bubblewrap` create user namespaces under Ubuntu's AppArmor policy;
2. runs `chief review --agent claude --model claude-sonnet-5-5 --agent-sandbox native --base origin/<PR base> --max-loop-iterations 5` with only `ANTHROPIC_API_KEY` in the environment;
3. renders the findings to Markdown and posts a new PR comment on every run with `gh pr comment`, using the job's `GITHUB_TOKEN` (`pull-requests: write`);
4. fails the job if any finding is P0 or P1.

Pull requests from forks get no secrets and a read-only token, so steps 2 and 3 fail there. Do not switch to `pull_request_target` to work around that, because it runs untrusted code with write access.

## Explore from the CLI

```bash
chief review --help
chief explain review
chief schema --json   # the "review" command entry lists options, defaults and notes
```
