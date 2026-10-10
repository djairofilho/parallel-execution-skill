# parallel-execution

An agent skill for coordinating subagents in separate Git worktrees and running independent subprocesses within a shared concurrency limit. The optional Rust runner provides per-batch CPU, RAM, and NVIDIA GPU admission for subprocesses.

## Install

Copy `skills/parallel-execution` into the skills directory used by your agent host. For Codex, you can ask:

```text
$skill-installer install https://github.com/djairofilho/parallel-execution-skill/tree/main/skills/parallel-execution
```

You can also install it at `~/.agents/skills/parallel-execution` in hosts that read that directory. Invoke `$parallel-execution` when your host supports named skills, or describe the task. The workflow uses the host's own subagent facilities; it does not require a specific agent CLI.

## Requirements

- Git and a target repository with at least one commit for isolated code edits.
- Rust and Cargo only when using the optional subprocess runner. From the installed skill folder, run `cargo build --release --locked --manifest-path native/rust/Cargo.toml`.
- A host that can delegate subagents into separate worktrees for parallel code edits. If it cannot, use the subprocess mode or edit sequentially.
- `nvidia-smi` only for tasks marked `gpu: true`.

Read [SKILL.md](skills/parallel-execution/SKILL.md) for the orchestration flow and [agents.md](skills/parallel-execution/references/agents.md) for the shared limit. The Rust runner still includes legacy Codex/OpenCode CLI adapters in `agents` mode; they are optional and are not part of the provider-neutral flow. Nothing commits or merges automatically.

License: [MIT](LICENSE).
