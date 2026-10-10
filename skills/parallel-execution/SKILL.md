---
name: parallel-execution
description: Coordinate independent agents in isolated Git worktrees and independent subprocesses under a shared resource limit. Use for parallel code edits or bounded terminal jobs.
---

# Parallel execution

Split work into independent deliverables with clear ownership, inputs, outputs, and completion checks. Start all work that fits the chosen limits; do not wait for one task to finish before admitting another.

## Choose a mode

- **Code edits:** the orchestrator creates one Git worktree per subagent, delegates through the agent facilities available in the host, and integrates the results. Each subagent may run independent subprocesses inside its worktree. Read [agents.md](references/agents.md).
- **A few independent commands:** use the host's terminal tools, launch commands that fit the shared limit, and check every exit status and output.
- **Resource-sensitive or repeated batches:** use the optional Rust `tasks` runner when automatic CPU/RAM/GPU admission, per-task timeouts and process-tree cleanup, or persistent logs and history justify it. Read [compute.md](references/compute.md).

```text
orchestrator: global limit, delegation, review, integration
├─ subagent A: worktree A
│  ├─ subprocess A1
│  └─ subprocess A2
└─ subagent B: worktree B
   ├─ subprocess B1
   └─ subprocess B2
```

Choose a shared limit for active work before starting. Count each active subagent and each of its scheduled subprocesses against that limit; assign each subagent a local subprocess allowance so the sum cannot exceed it. The orchestrator must track this budget because separate terminal calls and runner invocations have no shared limit. Also check CPU, RAM, GPU, and external service limits. Run the orchestrator's own commands within the same budget.

Review each subagent's changes and verify the integrated result. Do not commit or merge automatically.
