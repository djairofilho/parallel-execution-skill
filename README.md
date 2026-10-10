# parallel-execution

A Codex skill for independent commands and Codex/OpenCode agents in parallel Git worktrees, with a per-batch worker limit and local CPU, RAM, and NVIDIA GPU admission.

## Install

Ask Codex:

```text
$skill-installer install https://github.com/djairofilho/parallel-execution-skill/tree/main/skills/parallel-execution
```

Or download this repository and copy only `skills/parallel-execution` to `~/.agents/skills/parallel-execution`. Invoke `$parallel-execution`, or describe a matching parallel task. Codex can also select the skill automatically. See the [official skill locations](https://learn.chatgpt.com/docs/build-skills).

## Requirements

- Rust and Cargo to compile the runner locally. From the installed skill folder, run `cargo build --release --locked --manifest-path native/rust/Cargo.toml`.
- Git and a target repository with at least one commit for `agents` mode.
- A configured Codex CLI or OpenCode installation for `agents` mode.
- `nvidia-smi` only for tasks marked `gpu: true`.

Read [SKILL.md](skills/parallel-execution/SKILL.md) to choose `tasks` or `agents`. The runner does not commit or merge automatically.

License: [MIT](LICENSE).
