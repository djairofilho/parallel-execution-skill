# Parallel execution for coding agents

Coordinate independent coding agents in separate Git worktrees, then review and integrate their work. Run a few independent commands with your agent's terminal tools. An optional Rust runner handles repeated or resource-sensitive batches.

```text
orchestrator: choose tasks, set a shared limit, review and integrate
├─ agent A → worktree A → its own commands
└─ agent B → worktree B → its own commands
```

## Install

Install the skill in your project with the [Skills CLI](https://github.com/vercel-labs/skills):

```sh
npx skills add djairofilho/parallel-execution-skill --skill parallel-execution
```

Alternatively, copy [`skills/parallel-execution`](skills/parallel-execution) into the skills directory your agent reads. In Codex, you can ask `$skill-installer` to install [this skill folder](https://github.com/djairofilho/parallel-execution-skill/tree/main/skills/parallel-execution). The workflow uses the host's agent and terminal tools; it is not tied to one agent CLI.

## Try it on a real task

Give your agent a task whose parts can be checked separately. For example, in an event app repository:

> Use `$parallel-execution` to add a city filter to the event feed and an event card component. Assign the filter and card to separate agents in isolated worktrees, with clear file ownership. Limit active agents and commands to four slots total. Integrate the changes, run the project's tests and build, and report any conflicts or unfinished work. Do not commit or merge automatically.

The orchestrator should identify the files and checks in that repository before delegating. If the tasks depend on the same file or the host cannot isolate writing agents, it should work sequentially.

In a small local fixture, one agent owned `src/events.py` (city filtering) and another owned `src/cards.py` (card rendering). They worked in separate worktrees and made no commits. After integration, `python -m unittest discover -s tests -v` passed all five tests, including a filter-to-render integration test. This validates that example's division and integration; it does **not** establish a general speedup.

## When to use it

- **Independent code changes:** give each agent owned paths and an isolated worktree, then review and test the combined result.
- **A few independent commands:** use the host's terminal tools under one shared concurrency limit.
- **Repeated or resource-sensitive jobs:** use the optional Rust runner for CPU/RAM/NVIDIA GPU admission, task timeouts, process-tree cleanup, and persistent logs. See [compute.md](skills/parallel-execution/references/compute.md).

Parallel work has coordination and startup costs. A small dependent change may finish faster with one agent. The skill sets a limit for work it coordinates; it does not impose operating-system resource quotas or a global limit across separate runner invocations.

## Requirements and details

- Git and a repository with at least one commit for worktree-based code edits.
- A host with subagent delegation for parallel code edits. Other tasks can still use its terminal tools.
- Rust and Cargo only for the optional runner; `nvidia-smi` only for NVIDIA GPU tasks.

Read [SKILL.md](skills/parallel-execution/SKILL.md) for the workflow and [agents.md](skills/parallel-execution/references/agents.md) for worktree ownership and the shared limit. The Rust runner retains optional Codex/OpenCode CLI adapters for existing integrations; they are outside the provider-neutral default workflow. The skill never commits or merges automatically.

License: [MIT](LICENSE).
