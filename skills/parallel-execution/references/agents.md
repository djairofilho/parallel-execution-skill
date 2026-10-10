# Parallel agents in Git worktrees

Use `agents` for independent code edits. The target repository needs at least one commit and a clean main worktree. Put the new output directory and history database outside that repository. Install this skill in the target repository or at user scope if child agents must read it.

Each manifest task has a unique `id` and a `prompt_file` relative to the manifest. Reuse `workload` for comparable tasks. Cold admission defaults to 1.5 CPU cores and 1536 MiB per agent; positive `cpu_cores` and `ram_mb` override these values. Optional `owned_paths` lists relative paths the agent should edit; the review flags changes outside that scope. Set `gpu: true` only if the agent runs local GPU tools.

```json
{
  "tasks": [
    {"id": "api", "prompt_file": "prompts/api.txt", "workload": "code-edit", "owned_paths": ["api"]},
    {"id": "ui", "prompt_file": "prompts/ui.txt", "workload": "code-edit", "owned_paths": ["ui"]}
  ]
}
```

After building the runner, from your project directory:

```powershell
$skill = Join-Path $HOME ".codex/skills/parallel-execution"
$runner = Join-Path $skill "native/rust/target/release/parallel-run.exe"
& $runner agents agents.json --repo . --out-dir ../parallel-runs/batch-1 --workers 2 --provider codex
```

On Linux/macOS, omit `.exe`. For Codex, `--model MODEL` and `--codex-profile NAME` forward the model and profile to each child. `--provider opencode` uses OpenCode instead. The runner creates a branch and worktree per task and saves the clients' JSONL and error logs. It requests Codex `workspace-write` and does not bypass client approvals. The OpenCode adapter uses the client's configured permissions; on Windows it resolves `opencode.cmd` and passes the prompt through `--file`.

## Review and integration

Inspect `review.md`, `summary.json`, logs, tracked patches, `<id>.changes.json`, and each worktree. The changes inventory includes new files with path, size, and SHA-256; inspect their content in the worktree. The review flags out-of-scope paths and paths changed by multiple agents. Check integration and tests before merging manually. The runner leaves branches and worktrees intact and never commits, merges, or removes them.

Token and cost fields reflect only events reported by the client. Codex may report tokens without monetary cost; `usage_complete: false` means the reported total may be incomplete. The adapters have been tested with simulated clients on Windows and Ubuntu via WSL; real provider calls were not verified in that test.
