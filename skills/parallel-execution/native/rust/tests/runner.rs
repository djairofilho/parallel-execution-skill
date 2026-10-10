use rusqlite::Connection;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "parallel-rust-test-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let (Ok(path), Ok(parent)) = (self.0.canonicalize(), env::temp_dir().canonicalize()) {
            if path.starts_with(parent) && path != env::temp_dir() {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

fn executable() -> &'static str {
    env!("CARGO_BIN_EXE_parallel-run")
}

fn write_manifest(path: &Path, tasks: Value) {
    fs::write(path, serde_json::to_vec(&json!({"tasks": tasks})).unwrap()).unwrap();
}

fn run(mode: &str, manifest: &Path, out: &Path, extra: &[&str], path: Option<&Path>) -> Output {
    let mut command = Command::new(executable());
    command
        .arg(mode)
        .arg(manifest)
        .args(["--workers", "2", "--out-dir"])
        .arg(out)
        .args(extra);
    if let Some(directory) = path {
        let joined = env::join_paths(
            std::iter::once(directory.to_path_buf())
                .chain(env::split_paths(&env::var_os("PATH").unwrap())),
        )
        .unwrap();
        command.env("PATH", joined);
    }
    command.output().unwrap()
}

fn summary(out: &Path) -> Value {
    serde_json::from_slice(&fs::read(out.join("summary.json")).unwrap()).unwrap()
}

fn quick_command() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["cmd", "/C", "echo OK"]
    } else {
        vec!["sh", "-c", "printf 'OK\\n'"]
    }
}

fn sleep_command() -> Vec<&'static str> {
    if cfg!(windows) {
        vec![
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]
    } else {
        vec!["sleep", "30"]
    }
}

fn short_sleep_command() -> Vec<&'static str> {
    if cfg!(windows) {
        vec![
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "Start-Sleep -Seconds 2",
        ]
    } else {
        vec!["sleep", "2"]
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fake_cli(directory: &Path, provider: &str) {
    let event = if provider == "codex" {
        r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":2}}"#
    } else {
        r#"{"type":"step_finish","part":{"tokens":{"input":3,"output":2},"cost":0.01,"reason":"stop"}}"#
    };
    if cfg!(windows) {
        let record_args = if provider == "codex" {
            "echo %* >\"%~3\\argv.txt\"\r\n"
        } else {
            ""
        };
        fs::write(
            directory.join(format!("{provider}.cmd")),
            format!("@echo off\r\n{record_args}echo OK>\"%~3\\marker.txt\"\r\necho {event}\r\n"),
        )
        .unwrap();
    } else {
        let path = directory.join(provider);
        let record_args = if provider == "codex" {
            "printf '%s\\n' \"$*\" > \"$3/argv.txt\"\n"
        } else {
            ""
        };
        fs::write(
            &path,
            format!("#!/bin/sh\n{record_args}printf 'OK\\n' > \"$3/marker.txt\"\nprintf '%s\\n' '{event}'\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

fn slow_cli(directory: &Path) {
    if cfg!(windows) {
        fs::write(
            directory.join("codex.cmd"),
            "@echo off\r\npowershell.exe -NoProfile -Command \"Start-Sleep -Seconds 2\"\r\n",
        )
        .unwrap();
    } else {
        let path = directory.join("codex");
        fs::write(&path, "#!/bin/sh\nsleep 2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

#[test]
fn commands_history_and_failure() {
    let root = TestDir::new();
    let manifest = root.path().join("tasks.json");
    write_manifest(
        &manifest,
        json!([
            {"id": "one", "workload": "same", "cwd": root.path(), "argv": quick_command()},
            {"id": "two", "workload": "same", "argv": quick_command()}
        ]),
    );
    let out = root.path().join("success");
    let output = run("tasks", &manifest, &out, &["--schedule", "fixed"], None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = summary(&out);
    assert_eq!(data["results"].as_array().unwrap().len(), 2);
    assert_eq!(data["results"][0]["returncode"], 0);
    assert_eq!(
        fs::read_to_string(out.join("one.stdout.log"))
            .unwrap()
            .trim(),
        "OK"
    );
    let db = Connection::open(data["history_db"].as_str().unwrap()).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM runs WHERE workload='same'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    let failure = root.path().join("failure.json");
    let command = if cfg!(windows) {
        vec!["cmd", "/C", "exit 7"]
    } else {
        vec!["sh", "-c", "exit 7"]
    };
    write_manifest(&failure, json!([{"id": "fail", "argv": command}]));
    let failed_out = root.path().join("failed");
    let output = run(
        "tasks",
        &failure,
        &failed_out,
        &["--schedule", "fixed"],
        None,
    );
    assert!(!output.status.success());
    assert_eq!(summary(&failed_out)["results"][0]["returncode"], 7);
}

#[test]
fn adaptive_starts_all_fitting_jobs_and_respects_worker_cap() {
    let root = TestDir::new();
    let manifest = root.path().join("tasks.json");
    let tasks: Vec<_> = (0..4)
        .map(|index| {
            json!({
                "id": format!("job{index}"), "argv": short_sleep_command(),
                "cpu_cores": 0.1, "ram_mb": 64.0
            })
        })
        .collect();
    write_manifest(&manifest, json!(tasks));
    let out = root.path().join("out");
    let output = run(
        "tasks",
        &manifest,
        &out,
        &["--workers", "3", "--reserve-cpu-cores", "0"],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = summary(&out);
    let results = data["results"].as_array().unwrap();
    let first_finish = results[..3]
        .iter()
        .map(|r| r["finished_at"].as_f64().unwrap())
        .fold(f64::INFINITY, f64::min);
    assert!(results[..3]
        .iter()
        .all(|r| r["started_at"].as_f64().unwrap() < first_finish));
    assert!(results[3]["started_at"].as_f64().unwrap() >= first_finish - 0.05);
    assert_eq!(results[0]["admission"]["estimated_cpu_cores"], 0.1);
    assert_eq!(results[0]["admission"]["estimated_ram_mb"], 64.0);
    assert!(out.join("admission.jsonl").exists());
}

#[test]
fn legacy_history_is_migrated_without_using_unsampled_peaks() {
    let root = TestDir::new();
    let db_path = root.path().join("old.sqlite3");
    let db = Connection::open(&db_path).unwrap();
    db.execute_batch("CREATE TABLE runs (workload TEXT NOT NULL, duration REAL NOT NULL, exit_code INTEGER, cpu_cores REAL NOT NULL, ram_mb REAL NOT NULL, vram_mb REAL, gpu INTEGER NOT NULL, finished_at REAL NOT NULL); INSERT INTO runs VALUES ('old',1,0,0,1,NULL,0,1);").unwrap();
    drop(db);
    let manifest = root.path().join("tasks.json");
    write_manifest(
        &manifest,
        json!([{"id":"one","workload":"old","argv":quick_command()}]),
    );
    let out = root.path().join("out");
    let output = run(
        "tasks",
        &manifest,
        &out,
        &["--history-db", db_path.to_str().unwrap()],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        summary(&out)["results"][0]["admission"]["estimated_cpu_cores"],
        1.0
    );
    let db = Connection::open(db_path).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM runs WHERE host_key IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn timeout_and_resource_pressure() {
    let root = TestDir::new();
    let manifest = root.path().join("slow.json");
    write_manifest(&manifest, json!([{"id": "slow", "argv": sleep_command()}]));
    let out = root.path().join("timeout");
    let output = run(
        "tasks",
        &manifest,
        &out,
        &["--schedule", "fixed", "--timeout", "0.1"],
        None,
    );
    assert!(!output.status.success());
    assert!(summary(&out)["results"][0]["error"]
        .as_str()
        .unwrap()
        .contains("timeout"));

    let pressure = root.path().join("pressure");
    let output = run(
        "tasks",
        &manifest,
        &pressure,
        &["--reserve-ram-mb", "999999"],
        None,
    );
    assert!(!output.status.success());
    assert!(summary(&pressure)["results"][0]["returncode"].is_null());
}

#[test]
fn gpu_uses_mock_telemetry_and_serializes_jobs() {
    let root = TestDir::new();
    if cfg!(windows) {
        fs::write(
            root.path().join("nvidia-smi.cmd"),
            "@echo off\r\necho called>>\"%~dp0gpu-calls.txt\"\r\necho 0, 4096, 3072\r\n",
        )
        .unwrap();
    } else {
        let path = root.path().join("nvidia-smi");
        fs::write(&path, "#!/bin/sh\nprintf x >> \"$(dirname \"$0\")/gpu-calls.txt\"\nprintf '0, 4096, 3072\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let manifest = root.path().join("gpu.json");
    write_manifest(
        &manifest,
        json!([
            {"id": "one", "gpu": true, "argv": quick_command()},
            {"id": "two", "gpu": true, "argv": quick_command()}
        ]),
    );
    let out = root.path().join("gpu-out");
    let output = run("tasks", &manifest, &out, &[], Some(root.path()));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join("gpu-calls.txt").exists());
    let data = summary(&out);
    assert_eq!(data["results"][0]["gpu_device"], 0);
    assert_eq!(data["results"][1]["gpu_device"], 0);
    assert!(
        data["results"][1]["started_at"].as_f64().unwrap()
            >= data["results"][0]["finished_at"].as_f64().unwrap() - 0.02
    );
}

#[test]
fn agents_leave_reviewable_worktrees() {
    for provider in ["codex", "opencode"] {
        let root = TestDir::new();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "base.txt"]);
        git(&repo, &["commit", "-qm", "base"]);
        fake_cli(root.path(), provider);
        fs::write(root.path().join("prompt.txt"), "Create a marker.").unwrap();
        let manifest = root.path().join("agents.json");
        write_manifest(
            &manifest,
            json!([{"id": "one", "prompt_file": "prompt.txt", "owned_paths": ["allowed"]}]),
        );
        let out = root.path().join("agents-out");
        let output = run(
            "agents",
            &manifest,
            &out,
            &if provider == "codex" {
                vec![
                    "--repo",
                    repo.to_str().unwrap(),
                    "--provider",
                    provider,
                    "--model",
                    "gpt-6-luna",
                    "--codex-profile",
                    "benchmark",
                ]
            } else {
                vec!["--repo", repo.to_str().unwrap(), "--provider", provider]
            },
            Some(root.path()),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(git(&repo, &["status", "--porcelain"]), "");
        assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"]), "1");
        assert!(out.join("review.md").exists());
        let changes: Value =
            serde_json::from_slice(&fs::read(out.join("one.changes.json")).unwrap()).unwrap();
        assert!(changes.as_array().unwrap().iter().any(|change| {
            change["path"] == "marker.txt"
                && change["status"] == "untracked"
                && change["outside_owned_paths"] == true
                && change["sha256"].as_str().unwrap().len() == 64
        }));
        assert!(fs::read_to_string(out.join("review.md"))
            .unwrap()
            .contains("Outside `owned_paths`"));
        assert_eq!(
            fs::read_to_string(out.join("worktrees/one/marker.txt"))
                .unwrap()
                .trim(),
            "OK"
        );
        let result = &summary(&out)["results"][0];
        assert_eq!(result["returncode"], 0);
        assert_eq!(result["token_usage"]["input"], 3);
        assert_eq!(result["token_usage"]["output"], 2);
        assert_eq!(result["usage_complete"], true);
        if provider == "codex" {
            let argv = fs::read_to_string(out.join("worktrees/one/argv.txt")).unwrap();
            assert!(argv.contains("--model gpt-6-luna"), "{argv}");
            assert!(argv.contains("--profile benchmark"), "{argv}");
        }
        if provider == "opencode" {
            assert_eq!(result["cost"], 0.01);
        }
    }
}

#[test]
fn large_prompt_does_not_block_other_agent_launches() {
    let root = TestDir::new();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-qm", "base"]);
    slow_cli(root.path());
    fs::write(root.path().join("prompt.txt"), "a".repeat(1_000_000)).unwrap();
    let manifest = root.path().join("agents.json");
    write_manifest(
        &manifest,
        json!([
            {"id":"one","prompt_file":"prompt.txt","cpu_cores":0.1,"ram_mb":64},
            {"id":"two","prompt_file":"prompt.txt","cpu_cores":0.1,"ram_mb":64}
        ]),
    );
    let out = root.path().join("out");
    let output = run(
        "agents",
        &manifest,
        &out,
        &["--repo", repo.to_str().unwrap(), "--reserve-cpu-cores", "0"],
        Some(root.path()),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = summary(&out);
    assert!(
        data["results"][1]["started_at"].as_f64().unwrap()
            < data["results"][0]["finished_at"].as_f64().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn interrupt_writes_summary_and_ends_child_group() {
    let root = TestDir::new();
    let child_pid = root.path().join("child.pid");
    let manifest = root.path().join("tasks.json");
    write_manifest(
        &manifest,
        json!([{"id":"tree","argv":["sh","-c",
        format!("sleep 30 & echo $! > '{}'; wait", child_pid.display())]}]),
    );
    let out = root.path().join("out");
    let mut runner = Command::new(executable())
        .arg("tasks")
        .arg(&manifest)
        .args(["--workers", "1", "--out-dir"])
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if child_pid.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(child_pid.exists(), "child did not start");
    let pid: i32 = fs::read_to_string(&child_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe {
        libc::kill(runner.id() as i32, libc::SIGINT);
    }
    let exit = runner.wait().unwrap();
    assert!(!exit.success());
    assert!(summary(&out)["results"][0]["error"]
        .as_str()
        .unwrap()
        .contains("interrupted"));
    for _ in 0..40 {
        let state = fs::read_to_string(format!("/proc/{pid}/stat"));
        if state.is_err() || state.unwrap().split_whitespace().nth(2) == Some("Z") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("child process survived the interrupted runner");
}
