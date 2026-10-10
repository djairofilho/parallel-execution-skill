---
name: parallel-execution
description: Coordinate independent agents in isolated Git worktrees and independent subprocesses under a shared resource limit. Use for parallel code edits or bounded terminal jobs.
---

# Parallel execution

Split work into independent deliverables with clear ownership, inputs, outputs, and completion checks. Start all work that fits the chosen limits; do not wait for one task to finish before admitting another.

## Choose a mode

- **Code edits:** the orchestrator creates one Git worktree per subagent, delegates through the agent facilities available in the host, and integrates the results. Each subagent may run independent subprocesses inside its worktree. Read [agents.md](references/agents.md).
- **Independent commands:** use the Rust runner's `tasks` mode. Read [compute.md](references/compute.md) for manifests and resource settings.

```text
orchestrator: global limit, delegation, review, integration
├─ subagent A: worktree A
│  ├─ subprocess A1
│  └─ subprocess A2
└─ subagent B: worktree B
   ├─ subprocess B1
   └─ subprocess B2
```

Choose a shared limit for active work before starting. Count each active subagent and each of its scheduled subprocesses against that limit; assign each subagent a local subprocess allowance so the sum cannot exceed it. The orchestrator must track this budget because the Rust runner's `--workers` applies to one invocation only. Also check CPU, RAM, GPU, and external service limits. Run the orchestrator's own commands within the same budget.

Build the optional subprocess runner once from the installed skill directory:

```powershell
cargo build --release --locked --manifest-path native/rust/Cargo.toml
```

Use `native/rust/target/release/parallel-run.exe` on Windows or `parallel-run` on Linux/macOS. [Cargo.lock](native/rust/Cargo.lock) pins dependencies. Adaptive scheduling fills safe slots up to `--workers` from its first pass. Admission estimates are not OS quotas. GPU tasks require `gpu: true` and NVIDIA telemetry.

Review each subagent's changes and verify the integrated result. The runner never commits or merges. Windows and Ubuntu via WSL have been tested; validate locally on other systems.
