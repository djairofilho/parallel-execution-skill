# Agents and subprocesses

The orchestrator owns the global concurrency limit, worktree setup, delegation, and integration. Use the host's available subagent mechanism; the workflow does not depend on a particular agent CLI. Each subagent edits only its assigned worktree and paths. Its subprocesses run from that worktree, with distinct output paths.

## Prepare and allocate

1. Divide the work by independently reviewable deliverables and assign explicit path ownership. Avoid concurrent edits to the same file. Ensure the target repository has a commit and a clean starting worktree.
2. Pick `max_active` from the user's limit or the host's available CPU and RAM, leaving capacity for the machine and external work. A subagent consumes one slot. A scheduled subprocess consumes one more. Maintain `active_agents + active_subprocesses + orchestrator_subprocesses <= max_active`.
3. Allocate each subagent a local subprocess cap before delegation. The sum of caps must fit the remaining global slots. Track active work and pause new launches when the budget is full. Account for CPU, RAM, and GPU use across all worktrees, since independent runner invocations have no shared admission controller. Give one agent ownership of a GPU device at a time.
4. Create a separate branch and worktree for each subagent, outside the main repository. For example, from the target repository:

   ```powershell
   git worktree add -b parallel/api ../parallel-api HEAD
   git worktree add -b parallel/ui ../parallel-ui HEAD
   ```

5. Delegate each task with its absolute worktree path, owned paths, expected artifact, completion check, and local subprocess cap. Require the subagent to execute commands in that worktree and return a concise change and verification summary. If the host cannot keep a writing subagent in its worktree, run code-editing agents sequentially.

For example, with `max_active = 6`, two active subagents use two slots. Each may receive two subprocess slots, so `2 agents + 2 A children + 2 B children = 6`. For a few commands, each subagent uses its host's terminal tools and waits for every result. For resource-sensitive batches, it may use the optional runner from [compute.md](compute.md), setting `--workers` to its assigned cap. Give each runner invocation a separate output directory and history database. Reallocate freed slots when launching later batches.

## Review and integrate

Have each subagent report its worktree, changed and new files, verification results, and unfinished work. Inspect `git status`, diffs, untracked files, and subprocess logs in every worktree. Check ownership boundaries, then apply reviewed tracked changes and copy reviewed new files into the main worktree. Resolve any overlaps there and test the combined result. Do not auto-commit, auto-merge, or delete worktrees.
