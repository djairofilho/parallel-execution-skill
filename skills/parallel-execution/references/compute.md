# Independent commands

Use `tasks` for independent subprocesses. Give each task a separate output path. `argv` is an argument array, not a shell command; `cwd` is optional and resolves relative to the manifest. Use the same `workload` for comparable tasks so they can share resource history.

```json
{
  "tasks": [
    {"id": "page-1", "workload": "scrape-page", "argv": ["python", "scrape.py", "--page", "1", "--output", "data/1.json"]},
    {"id": "page-2", "workload": "scrape-page", "argv": ["python", "scrape.py", "--page", "2", "--output", "data/2.json"]}
  ]
}
```

From your project directory, set `$skill` to the installed skill folder:

```powershell
$skill = "path/to/parallel-execution"
$runner = Join-Path $skill "native/rust/target/release/parallel-run.exe"
& $runner tasks tasks.json --workers 4 --out-dir ../parallel-runs/batch-1
```

On Linux/macOS, omit `.exe`. The output directory must be new. SQLite history defaults to `parallel-history.sqlite3` beside it; `--history-db` overrides the location. `--timeout` sets seconds per task.

`--workers` is a hard cap per runner invocation. Adaptive scheduling fills safe slots immediately using CPU/RAM estimates. Cold defaults are 1 CPU core and 512 MiB per command; positive `cpu_cores` and `ram_mb` fields override them per task. Valid samples from the same workload and host may raise these estimates. `--reserve-ram-mb` defaults to 1024 MiB; `--reserve-cpu-cores` defaults to 1. `--schedule fixed` ignores CPU/RAM admission but still honors worker and GPU limits. `--wake-mode polling` reproduces the former 250 ms completion wait for comparisons; resource sampling stays at 250 ms.

When subagents run separate batches, the orchestrator assigns each batch's `--workers` from the shared limit in [agents.md](agents.md). Give every batch its own output directory and history database; do not treat several `--workers` values as one global cap.

`admission.jsonl` records starts and changes in wait reason. `summary.json` contains each task's result, estimate, wait time, duration, and resource peaks; logs use `<id>.stdout.log` and `<id>.stderr.log`. These estimates do not impose OS quotas. Separate runner invocations can compete for the same host. For jobs shorter than about 250 ms, explicitly group independent operations into fewer subprocesses when startup cost dominates.

## GPU

Set `"gpu": true` only for commands that actually use a GPU. On NVIDIA systems, optional `gpu_device` selects its index and the runner sets `CUDA_VISIBLE_DEVICES`. Each GPU admits one runner task at a time. Without NVIDIA telemetry, GPU tasks fail with an explicit reason. Free VRAM reported by `nvidia-smi` may also reflect external processes.

`sysinfo` samples CPU and RAM for the process tree, so it can miss very short peaks. For scraping, choose a worker cap that also respects the target site's, network's, and disk's limits. Judge speed by valid output and total time, not concurrent process count alone.
