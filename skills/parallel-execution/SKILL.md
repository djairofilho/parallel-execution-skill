---
name: parallel-execution
description: Run independent commands or Codex/OpenCode agents in parallel worktrees with local CPU, RAM, and GPU admission. Use for bounded workers, parallel terminal tasks, or isolated code edits.
---

# Parallel execution

Split work into independent tasks with clear inputs, outputs, and completion checks. Give scraping jobs separate output paths and respect the target service's rate limits.

## Choose a mode

- **Independent commands:** use the Rust runner's `tasks` mode. Read [compute.md](references/compute.md) for manifests and resource settings.
- **Code editing agents:** use `agents` mode, with one Git worktree per task. Read [agents.md](references/agents.md) for setup and review.
- **Read-only investigation:** native subagents may be simpler. They share the environment; do not assume isolated worktrees.

From the installed skill directory, build once with Rust and Cargo:

```powershell
cargo build --release --locked --manifest-path native/rust/Cargo.toml
```

Run `native/rust/target/release/parallel-run.exe` on Windows or `parallel-run` on Linux/macOS, followed by `tasks` or `agents`, a manifest, and the options in the references. Rebuild after source changes; [Cargo.lock](native/rust/Cargo.lock) pins dependencies.

Adaptive scheduling fills every safe slot up to `--workers` from the first pass. It estimates CPU and RAM by workload, samples every 250 ms, and wakes when a process exits. `--workers` limits one batch, not concurrent runner invocations. GPU tasks require `gpu: true` and NVIDIA telemetry. Admission estimates are not OS resource quotas. Use `--schedule fixed` for comparison.

Windows and Ubuntu via WSL have been tested. Validate locally on other systems. Review `summary.json`, logs, and, for agents, `review.md` and worktrees. Report completion time, correctness, resource peaks, and available token/cost data. The runner never commits or merges.
