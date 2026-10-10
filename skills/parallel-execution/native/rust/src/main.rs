use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const GPU_INTERVAL: Duration = Duration::from_secs(1);
const VRAM_RESERVE_MB: i32 = 256;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Deserialize)]
struct Manifest {
    tasks: Vec<RawTask>,
}

#[derive(Deserialize)]
struct RawTask {
    id: String,
    argv: Option<Vec<String>>,
    cwd: Option<String>,
    workload: Option<String>,
    gpu: Option<bool>,
    gpu_device: Option<i32>,
    prompt_file: Option<String>,
    cpu_cores: Option<f64>,
    ram_mb: Option<f64>,
    owned_paths: Option<Vec<String>>,
}

#[derive(Clone)]
struct Task {
    id: String,
    workload: String,
    argv: Vec<String>,
    cwd: PathBuf,
    gpu: bool,
    gpu_device: Option<i32>,
    stdin: Option<String>,
    branch: Option<String>,
    worktree: Option<PathBuf>,
    agent: bool,
    cpu_cores: Option<f64>,
    ram_mb: Option<f64>,
    owned_paths: Vec<String>,
}

#[derive(Clone)]
struct Estimate {
    cpu: f64,
    ram: f64,
    vram: Option<f64>,
}

#[derive(Clone)]
struct Device {
    index: i32,
    free_mb: i32,
}

#[derive(Serialize, Deserialize, Clone)]
struct ResultItem {
    id: String,
    workload: String,
    returncode: Option<i32>,
    error: Option<String>,
    started_at: f64,
    finished_at: f64,
    duration_seconds: f64,
    peak_cpu_cores: f64,
    avg_cpu_cores: f64,
    resource_samples: u32,
    peak_ram_mb: f64,
    peak_vram_mb: Option<f64>,
    gpu_device: Option<i32>,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_complete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admission: Option<AdmissionRecord>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AdmissionRecord {
    estimated_cpu_cores: f64,
    estimated_ram_mb: f64,
    waiting_seconds: f64,
    wait_reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Summary {
    workers_requested: usize,
    schedule: String,
    started_at: f64,
    finished_at: f64,
    duration_seconds: f64,
    history_db: String,
    results: Vec<ResultItem>,
}

struct Options {
    workers: usize,
    out_dir: PathBuf,
    history_db: PathBuf,
    schedule: String,
    timeout: Option<f64>,
    reserve_ram_mb: usize,
    reserve_cpu_cores: f64,
    wake_mode: String,
    repo: Option<PathBuf>,
    provider: String,
    model: Option<String>,
    codex_profile: Option<String>,
}

struct Job {
    task: Task,
    estimate: Estimate,
    pid: u32,
    start: Instant,
    start_epoch: f64,
    device: Option<i32>,
    initial_gpu_free: Option<i32>,
    peak_cpu: f64,
    cpu_sum: f64,
    cpu_samples: u32,
    peak_ram: f64,
    current_ram: f64,
    peak_vram: Option<f64>,
    last_cpu: f64,
    last_sample: Option<Instant>,
    error: Option<String>,
    admission: AdmissionRecord,
}

struct Completion {
    id: String,
    code: Option<i32>,
}

fn epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn round(value: f64, precision: i32) -> f64 {
    let scale = 10f64.powi(precision);
    (value * scale).round() / scale
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn is_within(path: &Path, parent: &Path) -> bool {
    path.starts_with(parent)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().enumerate().all(|(index, ch)| {
            ch.is_ascii_alphanumeric() || index > 0 && (ch == b'_' || ch == b'-')
        })
}

fn load_tasks(manifest: &Path, agents: bool) -> Result<Vec<Task>> {
    let data = fs::read_to_string(manifest)?;
    let parsed: Manifest = serde_json::from_str(&data)?;
    if parsed.tasks.is_empty() {
        bail!("manifest must contain a nonempty tasks array")
    }
    let mut seen = HashSet::new();
    let mut tasks = Vec::new();
    for raw in parsed.tasks {
        if !valid_id(&raw.id) || !seen.insert(raw.id.clone()) {
            bail!("invalid or duplicate task id: {}", raw.id)
        }
        let gpu = raw.gpu.unwrap_or(false);
        if raw.gpu_device.is_some_and(|device| device < 0 || !gpu) {
            bail!("task {}: invalid gpu_device", raw.id)
        }
        if raw
            .workload
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            bail!("task {}: workload must be a nonempty string", raw.id);
        }
        if raw
            .cpu_cores
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
            || raw
                .ram_mb
                .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            bail!("task {}: cpu_cores and ram_mb must be positive", raw.id);
        }
        let owned_paths = raw.owned_paths.unwrap_or_default();
        if !agents && !owned_paths.is_empty() {
            bail!("task {}: owned_paths requires agents mode", raw.id);
        }
        if owned_paths.iter().any(|path| {
            let value = Path::new(path);
            path.is_empty()
                || value.is_absolute()
                || value
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
        }) {
            bail!(
                "task {}: owned_paths must contain relative paths without traversal",
                raw.id
            );
        }
        let workload = raw.workload.unwrap_or_else(|| {
            if agents {
                format!("agent:{}", raw.id)
            } else {
                raw.id.clone()
            }
        });
        let mut task = Task {
            id: raw.id,
            workload,
            argv: Vec::new(),
            cwd: PathBuf::new(),
            gpu,
            gpu_device: raw.gpu_device,
            stdin: None,
            branch: None,
            worktree: None,
            agent: agents,
            cpu_cores: raw.cpu_cores,
            ram_mb: raw.ram_mb,
            owned_paths,
        };
        if agents {
            let prompt = raw.prompt_file.context("prompt_file is required")?;
            let text = fs::read_to_string(manifest.parent().unwrap().join(prompt))?;
            if text.trim().is_empty() {
                bail!("task {}: empty prompt_file", task.id)
            }
            task.stdin = Some(text);
        } else {
            let argv = raw.argv.context("argv is required")?;
            if argv.is_empty() || argv.iter().any(String::is_empty) {
                bail!("task {}: argv must be nonempty", task.id)
            }
            task.argv = argv;
            if raw.cwd.as_ref().is_some_and(String::is_empty) {
                bail!("task {}: cwd must be a directory path", task.id);
            }
            task.cwd = fs::canonicalize(
                manifest
                    .parent()
                    .unwrap()
                    .join(raw.cwd.unwrap_or_else(|| ".".into())),
            )?;
            if !task.cwd.is_dir() {
                bail!("task {}: cwd does not exist", task.id)
            }
        }
        tasks.push(task);
    }
    Ok(tasks)
}

fn open_history(path: &Path) -> Result<Connection> {
    fs::create_dir_all(path.parent().context("history parent missing")?)?;
    let connection = Connection::open(path)?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS runs (workload TEXT NOT NULL, duration REAL NOT NULL, exit_code INTEGER, cpu_cores REAL NOT NULL, ram_mb REAL NOT NULL, vram_mb REAL, gpu INTEGER NOT NULL, finished_at REAL NOT NULL); CREATE INDEX IF NOT EXISTS runs_workload ON runs(workload, finished_at DESC);")?;
    let mut columns = HashSet::new();
    let mut statement = connection.prepare("PRAGMA table_info(runs)")?;
    for name in statement.query_map([], |row| row.get::<_, String>(1))? {
        columns.insert(name?);
    }
    drop(statement);
    for (name, declaration) in [
        ("avg_cpu_cores", "REAL"),
        ("resource_samples", "INTEGER"),
        ("host_key", "TEXT"),
    ] {
        if !columns.contains(name) {
            connection.execute(
                &format!("ALTER TABLE runs ADD COLUMN {name} {declaration}"),
                [],
            )?;
        }
    }
    Ok(connection)
}

fn host_key(system: &System) -> String {
    format!(
        "{}:{}:{}",
        env::consts::OS,
        thread::available_parallelism().map_or(1, |count| count.get()),
        system.total_memory() / (1024 * 1024)
    )
}

fn estimate(db: &Connection, task: &Task, host: &str) -> Result<Estimate> {
    let mut statement = db.prepare("SELECT avg_cpu_cores, ram_mb, vram_mb FROM runs WHERE workload=? AND gpu=? AND exit_code=0 AND host_key=? AND resource_samples>=2 ORDER BY finished_at DESC LIMIT 20")?;
    let rows = statement.query_map(params![task.workload, i32::from(task.gpu), host], |row| {
        Ok((
            row.get::<_, Option<f64>>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, Option<f64>>(2)?,
        ))
    })?;
    let mut result = Estimate {
        cpu: task.cpu_cores.unwrap_or(if task.agent { 1.5 } else { 1.0 }),
        ram: task
            .ram_mb
            .unwrap_or(if task.agent { 1536.0 } else { 512.0 }),
        vram: None,
    };
    let mut cpu_values = Vec::new();
    let mut ram_values = Vec::new();
    for row in rows {
        let (cpu, ram, vram) = row?;
        if let Some(cpu) = cpu.filter(|value| value.is_finite() && *value > 0.0) {
            cpu_values.push(cpu);
        }
        if ram.is_finite() && ram > 0.0 {
            ram_values.push(ram);
        }
        if task.gpu {
            if let Some(vram) = vram {
                result.vram = Some(result.vram.unwrap_or(0.0).max(vram * 1.25));
            }
        }
    }
    result.cpu = result
        .cpu
        .max(percentile(&mut cpu_values, 0.75).unwrap_or(0.0) * 1.25);
    result.ram = result
        .ram
        .max(percentile(&mut ram_values, 0.90).unwrap_or(0.0) * 1.25);
    Ok(result)
}

fn percentile(values: &mut [f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)])
}

fn record(db: &Connection, task: &Task, result: &ResultItem, host: &str) -> Result<()> {
    if let Some(code) = result.returncode {
        db.execute(
            "INSERT INTO runs (workload,duration,exit_code,cpu_cores,ram_mb,vram_mb,gpu,finished_at,avg_cpu_cores,resource_samples,host_key) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                task.workload,
                result.duration_seconds,
                code,
                result.peak_cpu_cores,
                result.peak_ram_mb,
                result.peak_vram_mb,
                i32::from(task.gpu),
                result.finished_at,
                result.avg_cpu_cores,
                result.resource_samples,
                host
            ],
        )?;
    }
    Ok(())
}

fn gpu_inventory() -> Vec<Device> {
    let executable = find_executable("nvidia-smi").unwrap_or_else(|| "nvidia-smi".into());
    let output = Command::new(executable)
        .args([
            "--query-gpu=index,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() != 3 {
                return None;
            }
            Some(Device {
                index: fields[0].parse().ok()?,
                free_mb: fields[2].parse().ok()?,
            })
        })
        .collect()
}

fn choose_device(
    task: &Task,
    est: &Estimate,
    running: &HashMap<String, Job>,
    inventory: &[Device],
) -> Option<i32> {
    inventory
        .iter()
        .find(|device| {
            if task.gpu_device.is_some_and(|index| index != device.index) {
                return false;
            }
            if running.values().any(|job| job.device == Some(device.index)) {
                return false;
            }
            if device.free_mb <= VRAM_RESERVE_MB {
                return false;
            }
            if est
                .vram
                .is_some_and(|vram| f64::from(device.free_mb - VRAM_RESERVE_MB) < vram)
            {
                return false;
            }
            true
        })
        .map(|device| device.index)
}

fn can_launch(
    task: &Task,
    est: &Estimate,
    running: &HashMap<String, Job>,
    opts: &Options,
    inventory: &[Device],
    available_mb: f64,
    cpu_budget: f64,
) -> std::result::Result<Option<i32>, &'static str> {
    if running.len() >= opts.workers {
        return Err("worker limit reached");
    }
    if opts.schedule == "adaptive" {
        let reserved_ram: f64 = running
            .values()
            .map(|job| (job.estimate.ram - job.current_ram).max(0.0))
            .sum();
        let reserved_cpu: f64 = running.values().map(|job| job.estimate.cpu).sum();
        if est.ram > (available_mb - opts.reserve_ram_mb as f64 - reserved_ram).max(0.0) {
            return Err("insufficient available RAM");
        }
        if !running.is_empty() && est.cpu + reserved_cpu > cpu_budget {
            return Err("insufficient CPU budget");
        }
    }
    if task.gpu {
        if inventory.is_empty() {
            Err("NVIDIA GPU unavailable")
        } else {
            choose_device(task, est, running, inventory)
                .map(Some)
                .ok_or("GPU busy or insufficient VRAM")
        }
    } else {
        Ok(None)
    }
}

fn is_descendant(pid: Pid, ancestor: Pid, system: &System) -> bool {
    let mut current = Some(pid);
    let mut visited = HashSet::new();
    while let Some(value) = current {
        if value == ancestor {
            return true;
        }
        if !visited.insert(value) {
            break;
        }
        current = system.process(value).and_then(|process| process.parent());
    }
    false
}

fn tree_metrics(pid: u32, system: &System) -> (f64, f64) {
    let parent = Pid::from_u32(pid);
    let mut ram = 0.0;
    let mut cpu = 0.0;
    for (id, process) in system.processes() {
        if is_descendant(*id, parent, system) {
            ram += process.memory() as f64 / (1024.0 * 1024.0);
            cpu += process.accumulated_cpu_time() as f64 / 1000.0;
        }
    }
    (ram, cpu)
}

fn kill_tree(pid: u32, system: &mut System) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let parent = Pid::from_u32(pid);
    let descendants: Vec<_> = system
        .processes()
        .keys()
        .copied()
        .filter(|id| *id != parent && is_descendant(*id, parent, system))
        .collect();
    for id in descendants {
        if let Some(process) = system.process(id) {
            process.kill();
        }
    }
    if let Some(process) = system.process(parent) {
        process.kill();
    }
}

fn sample(job: &mut Job, system: &System, inventory: &[Device]) {
    let now = Instant::now();
    let (ram, cpu) = tree_metrics(job.pid, system);
    job.peak_ram = job.peak_ram.max(ram);
    job.current_ram = ram;
    if let Some(previous) = job.last_sample {
        let elapsed = now.duration_since(previous).as_secs_f64();
        if elapsed > 0.0 {
            let cpu_rate = ((cpu - job.last_cpu) / elapsed).max(0.0);
            job.peak_cpu = job.peak_cpu.max(cpu_rate);
            job.cpu_sum += cpu_rate;
            job.cpu_samples += 1;
        }
    }
    job.last_sample = Some(now);
    job.last_cpu = cpu;
    if let (Some(index), Some(initial)) = (job.device, job.initial_gpu_free) {
        if let Some(device) = inventory.iter().find(|device| device.index == index) {
            job.peak_vram = Some(
                job.peak_vram
                    .unwrap_or(0.0)
                    .max(f64::from((initial - device.free_mb).max(0))),
            );
        }
    }
}

fn result_for(
    task: &Task,
    out: &Path,
    started: f64,
    duration: f64,
    code: Option<i32>,
    error: Option<String>,
    job: Option<&Job>,
) -> ResultItem {
    ResultItem {
        id: task.id.clone(),
        workload: task.workload.clone(),
        returncode: code,
        error,
        started_at: started,
        finished_at: epoch(),
        duration_seconds: round(duration, 3),
        peak_cpu_cores: job.map_or(0.0, |job| round(job.peak_cpu, 3)),
        avg_cpu_cores: job.map_or(0.0, |job| {
            if job.cpu_samples == 0 {
                0.0
            } else {
                round(job.cpu_sum / f64::from(job.cpu_samples), 3)
            }
        }),
        resource_samples: job.map_or(0, |job| {
            job.cpu_samples + u32::from(job.last_sample.is_some())
        }),
        peak_ram_mb: job.map_or(0.0, |job| round(job.peak_ram, 2)),
        peak_vram_mb: job.and_then(|job| job.peak_vram.map(|value| round(value, 2))),
        gpu_device: job.and_then(|job| job.device),
        stdout: out
            .join(format!("{}.stdout.log", task.id))
            .to_string_lossy()
            .into_owned(),
        stderr: out
            .join(format!("{}.stderr.log", task.id))
            .to_string_lossy()
            .into_owned(),
        token_usage: None,
        cost: None,
        usage_complete: None,
        admission: job.map(|job| job.admission.clone()),
    }
}

fn launch(
    task: &Task,
    est: Estimate,
    out: &Path,
    device: Option<i32>,
    inventory: &[Device],
    sender: Sender<Completion>,
) -> std::result::Result<Job, ResultItem> {
    let started = Instant::now();
    let start_epoch = epoch();
    let stdout = match File::create(out.join(format!("{}.stdout.log", task.id))) {
        Ok(file) => file,
        Err(error) => {
            return Err(result_for(
                task,
                out,
                start_epoch,
                started.elapsed().as_secs_f64(),
                None,
                Some(error.to_string()),
                None,
            ))
        }
    };
    let stderr = match File::create(out.join(format!("{}.stderr.log", task.id))) {
        Ok(file) => file,
        Err(error) => {
            return Err(result_for(
                task,
                out,
                start_epoch,
                started.elapsed().as_secs_f64(),
                None,
                Some(error.to_string()),
                None,
            ))
        }
    };
    let mut command = Command::new(&task.argv[0]);
    command
        .args(&task.argv[1..])
        .current_dir(&task.cwd)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(input) = &task.stdin {
        command.stdin(Stdio::piped());
        if input.is_empty() {
            command.stdin(Stdio::null());
        }
    }
    if let Some(index) = device {
        command.env("CUDA_VISIBLE_DEVICES", index.to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let child = command.spawn();
    let Ok(mut child) = child else {
        return Err(result_for(
            task,
            out,
            start_epoch,
            started.elapsed().as_secs_f64(),
            None,
            Some(child.unwrap_err().to_string()),
            None,
        ));
    };
    if let Some(input) = &task.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.clone();
            thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
            });
        }
    }
    let pid = child.id();
    let id = task.id.clone();
    thread::spawn(move || {
        let code = child.wait().ok().and_then(|status| status.code());
        let _ = sender.send(Completion { id, code });
    });
    let initial_gpu_free = device.and_then(|index| {
        inventory
            .iter()
            .find(|item| item.index == index)
            .map(|item| item.free_mb)
    });
    Ok(Job {
        task: task.clone(),
        estimate: est.clone(),
        pid,
        start: started,
        start_epoch,
        device,
        initial_gpu_free,
        peak_cpu: 0.0,
        cpu_sum: 0.0,
        cpu_samples: 0,
        peak_ram: 0.0,
        current_ram: 0.0,
        peak_vram: None,
        last_cpu: 0.0,
        last_sample: None,
        error: None,
        admission: AdmissionRecord {
            estimated_cpu_cores: round(est.cpu, 3),
            estimated_ram_mb: round(est.ram, 2),
            waiting_seconds: 0.0,
            wait_reason: None,
        },
    })
}

fn write_summary(out: &Path, summary: &Summary) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(summary)?;
    data.push(b'\n');
    fs::write(out.join("summary.json"), data)?;
    Ok(())
}

fn write_agent_summary(out: &Path, summary: &Summary) -> Result<()> {
    let mut value = serde_json::to_value(summary)?;
    if let Some(results) = value["results"].as_array_mut() {
        for result in results {
            let object = result.as_object_mut().unwrap();
            object
                .entry("token_usage")
                .or_insert(serde_json::Value::Null);
            object.entry("cost").or_insert(serde_json::Value::Null);
            object
                .entry("usage_complete")
                .or_insert(serde_json::Value::Bool(false));
        }
    }
    let mut data = serde_json::to_vec_pretty(&value)?;
    data.push(b'\n');
    fs::write(out.join("summary.json"), data)?;
    Ok(())
}

fn handle_completion(
    completion: Completion,
    running: &mut HashMap<String, Job>,
    results: &mut HashMap<String, ResultItem>,
    db: &Connection,
    out: &Path,
    host: &str,
) -> Result<()> {
    if let Some(job) = running.remove(&completion.id) {
        let result = result_for(
            &job.task,
            out,
            job.start_epoch,
            job.start.elapsed().as_secs_f64(),
            completion.code,
            job.error.clone(),
            Some(&job),
        );
        record(db, &job.task, &result, host)?;
        results.insert(completion.id, result);
    }
    Ok(())
}

fn execute(tasks: &[Task], opts: &Options) -> Result<Summary> {
    let db = open_history(&opts.history_db)?;
    let start = Instant::now();
    let started_at = epoch();
    let mut pending = tasks.to_vec();
    let mut running: HashMap<String, Job> = HashMap::new();
    let mut results: HashMap<String, ResultItem> = HashMap::new();
    let (sender, receiver): (Sender<Completion>, Receiver<Completion>) = mpsc::channel();
    let mut system = System::new();
    system.refresh_memory();
    let host = host_key(&system);
    let estimates: HashMap<String, Estimate> = tasks
        .iter()
        .map(|task| Ok((task.id.clone(), estimate(&db, task, &host)?)))
        .collect::<Result<_>>()?;
    let mut wait_reasons: HashMap<String, String> = HashMap::new();
    let mut admission_log = File::create(opts.out_dir.join("admission.jsonl"))?;
    let mut next_sample = Instant::now() + SAMPLE_INTERVAL;
    let mut next_gpu_refresh = Instant::now();
    let mut inventory = Vec::new();
    while !pending.is_empty() || !running.is_empty() {
        if INTERRUPTED.load(Ordering::SeqCst) {
            system.refresh_processes(ProcessesToUpdate::All, true);
            for job in running.values() {
                kill_tree(job.pid, &mut system);
            }
            for (_, job) in running.drain() {
                let result = result_for(
                    &job.task,
                    &opts.out_dir,
                    job.start_epoch,
                    job.start.elapsed().as_secs_f64(),
                    None,
                    Some("interrupted".into()),
                    Some(&job),
                );
                results.insert(job.task.id.clone(), result);
            }
            for task in pending.drain(..) {
                let mut result = result_for(
                    &task,
                    &opts.out_dir,
                    epoch(),
                    0.0,
                    None,
                    Some("interrupted before start".into()),
                    None,
                );
                result.admission = Some(AdmissionRecord {
                    estimated_cpu_cores: estimates[&task.id].cpu,
                    estimated_ram_mb: estimates[&task.id].ram,
                    waiting_seconds: round(start.elapsed().as_secs_f64(), 3),
                    wait_reason: wait_reasons.remove(&task.id),
                });
                results.insert(task.id.clone(), result);
            }
            break;
        }
        let need_gpu =
            pending.iter().any(|task| task.gpu) || running.values().any(|job| job.task.gpu);
        if need_gpu && Instant::now() >= next_gpu_refresh {
            inventory = gpu_inventory();
            next_gpu_refresh = Instant::now() + GPU_INTERVAL;
        }
        system.refresh_memory();
        let available_mb = system.available_memory() as f64 / (1024.0 * 1024.0);
        let cores = thread::available_parallelism().map_or(1, |value| value.get());
        let cpu_budget = (cores as f64 - opts.reserve_cpu_cores).max(1.0);
        let mut remaining = Vec::new();
        for task in pending.drain(..) {
            let est = estimates[&task.id].clone();
            match can_launch(
                &task,
                &est,
                &running,
                opts,
                &inventory,
                available_mb,
                cpu_budget,
            ) {
                Ok(device) => {
                    match launch(
                        &task,
                        est.clone(),
                        &opts.out_dir,
                        device,
                        &inventory,
                        sender.clone(),
                    ) {
                        Ok(mut job) => {
                            job.admission.waiting_seconds = round(start.elapsed().as_secs_f64(), 3);
                            job.admission.wait_reason = wait_reasons.remove(&task.id);
                            writeln!(
                                admission_log,
                                "{}",
                                serde_json::json!({
                                    "task_id": task.id, "event": "started", "at": job.start_epoch,
                                    "running": running.len() + 1, "available_ram_mb": round(available_mb, 2),
                                    "cpu_budget": cpu_budget, "admission": job.admission,
                                })
                            )?;
                            running.insert(task.id.clone(), job);
                        }
                        Err(mut result) => {
                            result.admission = Some(AdmissionRecord {
                                estimated_cpu_cores: est.cpu,
                                estimated_ram_mb: est.ram,
                                waiting_seconds: round(start.elapsed().as_secs_f64(), 3),
                                wait_reason: wait_reasons.remove(&task.id),
                            });
                            results.insert(task.id.clone(), result);
                        }
                    }
                }
                Err(reason) => {
                    if wait_reasons.get(&task.id).map(String::as_str) != Some(reason) {
                        writeln!(
                            admission_log,
                            "{}",
                            serde_json::json!({
                                "task_id": task.id, "event": "waiting", "at": epoch(), "reason": reason,
                                "estimated_cpu_cores": est.cpu, "estimated_ram_mb": est.ram,
                                "running": running.len(), "available_ram_mb": round(available_mb, 2),
                                "cpu_budget": cpu_budget,
                            })
                        )?;
                        wait_reasons.insert(task.id.clone(), reason.into());
                    }
                    remaining.push(task);
                }
            }
        }
        pending = remaining;
        if !pending.is_empty() && running.is_empty() {
            for task in pending.drain(..) {
                let reason = wait_reasons
                    .remove(&task.id)
                    .unwrap_or_else(|| "insufficient resources for admission".into());
                let mut result = result_for(
                    &task,
                    &opts.out_dir,
                    epoch(),
                    0.0,
                    None,
                    Some(reason.clone()),
                    None,
                );
                result.admission = Some(AdmissionRecord {
                    estimated_cpu_cores: estimates[&task.id].cpu,
                    estimated_ram_mb: estimates[&task.id].ram,
                    waiting_seconds: round(start.elapsed().as_secs_f64(), 3),
                    wait_reason: Some(reason),
                });
                results.insert(task.id.clone(), result);
            }
        }
        if running.is_empty() {
            continue;
        }
        let wait = next_sample.saturating_duration_since(Instant::now());
        if opts.wake_mode == "polling" {
            thread::sleep(wait);
            for completion in receiver.try_iter() {
                handle_completion(
                    completion,
                    &mut running,
                    &mut results,
                    &db,
                    &opts.out_dir,
                    &host,
                )?;
            }
        } else if let Ok(completion) = receiver.recv_timeout(wait) {
            if running.get(&completion.id).is_some_and(|job| job.task.gpu) {
                next_gpu_refresh = Instant::now();
            }
            handle_completion(
                completion,
                &mut running,
                &mut results,
                &db,
                &opts.out_dir,
                &host,
            )?;
            for completion in receiver.try_iter() {
                if running.get(&completion.id).is_some_and(|job| job.task.gpu) {
                    next_gpu_refresh = Instant::now();
                }
                handle_completion(
                    completion,
                    &mut running,
                    &mut results,
                    &db,
                    &opts.out_dir,
                    &host,
                )?;
            }
        }
        if Instant::now() >= next_sample {
            system.refresh_processes(ProcessesToUpdate::All, true);
            for job in running.values_mut() {
                sample(job, &system, &inventory);
                if opts
                    .timeout
                    .is_some_and(|timeout| job.start.elapsed().as_secs_f64() > timeout)
                {
                    job.error.get_or_insert_with(|| {
                        format!("timeout after {} seconds", opts.timeout.unwrap())
                    });
                    kill_tree(job.pid, &mut system);
                }
            }
            next_sample = Instant::now() + SAMPLE_INTERVAL;
        }
    }
    let ordered: Vec<_> = tasks
        .iter()
        .map(|task| results.remove(&task.id).unwrap())
        .collect();
    let summary = Summary {
        workers_requested: opts.workers,
        schedule: opts.schedule.clone(),
        started_at,
        finished_at: epoch(),
        duration_seconds: round(start.elapsed().as_secs_f64(), 3),
        history_db: opts.history_db.to_string_lossy().into_owned(),
        results: ordered,
    };
    write_summary(&opts.out_dir, &summary)?;
    Ok(summary)
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn prepare_agents(tasks: &mut [Task], opts: &Options) -> Result<String> {
    let repo = opts.repo.as_ref().context("repo is required")?;
    let root = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"])?.trim());
    if !git(&root, &["status", "--porcelain"])?.trim().is_empty() {
        bail!("base worktree must be clean before starting agents")
    }
    let base = git(&root, &["rev-parse", "HEAD"])?.trim().to_owned();
    let run_id = format!("{:x}{:x}", epoch().to_bits(), std::process::id());
    let executable = if opts.provider == "opencode" && cfg!(windows) {
        "opencode.cmd"
    } else {
        opts.provider.as_str()
    };
    let resolved =
        find_executable(executable).ok_or_else(|| anyhow!("agent CLI not found: {executable}"))?;
    for task in tasks {
        let branch = format!("parallel/{run_id}/{}", task.id);
        let worktree = opts.out_dir.join("worktrees").join(&task.id);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &worktree.to_string_lossy(),
                &base,
            ],
        )?;
        task.branch = Some(branch);
        task.worktree = Some(worktree.clone());
        task.cwd = worktree.clone();
        let scope = if task.owned_paths.is_empty() {
            String::new()
        } else {
            format!(" Edit only these paths: {}.", task.owned_paths.join(", "))
        };
        let prompt = format!(
            "{}\n\nDo not commit or merge. Keep changes in this worktree only.{scope}\n",
            task.stdin.take().unwrap()
        );
        if opts.provider == "codex" {
            task.argv = vec![
                resolved.clone(),
                "exec".into(),
                "-C".into(),
                worktree.to_string_lossy().into_owned(),
                "--json".into(),
                "-s".into(),
                "workspace-write".into(),
            ];
            if let Some(model) = &opts.model {
                task.argv.extend(["--model".into(), model.clone()]);
            }
            if let Some(profile) = &opts.codex_profile {
                task.argv.extend(["--profile".into(), profile.clone()]);
            }
            task.argv.push("-".into());
            task.stdin = Some(prompt);
        } else {
            task.argv = vec![
                resolved.clone(),
                "run".into(),
                "--dir".into(),
                worktree.to_string_lossy().into_owned(),
                "--format".into(),
                "json".into(),
            ];
            if cfg!(windows) && resolved.to_ascii_lowercase().ends_with(".cmd") {
                let prompt_file = opts.out_dir.join(format!("{}.prompt.txt", task.id));
                fs::write(&prompt_file, prompt)?;
                task.argv.extend([
                    "--file".into(),
                    prompt_file.to_string_lossy().into_owned(),
                    "Follow the instructions in the attached file.".into(),
                ]);
            } else {
                task.argv.push(prompt);
            }
        }
    }
    Ok(base)
}

fn find_executable(name: &str) -> Option<String> {
    if Path::new(name).is_file() {
        return Some(name.to_owned());
    }
    env::split_paths(&env::var_os("PATH")?)
        .flat_map(|dir| {
            if cfg!(windows) && Path::new(name).extension().is_none() {
                vec![
                    dir.join(format!("{name}.exe")),
                    dir.join(format!("{name}.cmd")),
                ]
            } else {
                vec![dir.join(name)]
            }
        })
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

fn reported_usage(result: &mut ResultItem, provider: &str) -> Result<()> {
    let file = File::open(&result.stdout)?;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cached = 0u64;
    let mut cost = 0.0;
    let mut found = false;
    let mut complete = false;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if provider == "codex" && event["type"] == "turn.completed" {
            let usage = &event["usage"];
            input += usage["input_tokens"].as_u64().unwrap_or(0);
            output += usage["output_tokens"].as_u64().unwrap_or(0);
            cached += usage["cached_input_tokens"].as_u64().unwrap_or(0);
            found = true;
            complete = true;
        } else if provider == "opencode" && event["type"] == "step_finish" {
            let part = &event["part"];
            input += part["tokens"]["input"].as_u64().unwrap_or(0);
            output += part["tokens"]["output"].as_u64().unwrap_or(0);
            cached += part["tokens"]["cache"]["read"].as_u64().unwrap_or(0);
            cost += part["cost"].as_f64().unwrap_or(0.0);
            found = true;
            complete = part["reason"] == "stop";
        }
    }
    if found {
        result.token_usage =
            Some(serde_json::json!({"input": input, "output": output, "cached_input": cached}));
        if provider == "opencode" {
            result.cost = Some(round(cost, 6));
        }
    }
    result.usage_complete = Some(complete);
    Ok(())
}

#[derive(Serialize)]
struct ChangeRecord {
    path: String,
    status: String,
    size_bytes: Option<u64>,
    sha256: Option<String>,
    outside_owned_paths: bool,
}

fn git_paths(repo: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn change_record(worktree: &Path, task: &Task, path: String, status: &str) -> Result<ChangeRecord> {
    let file = worktree.join(&path);
    let (size_bytes, sha256) = if file.is_file() {
        let mut reader = File::open(&file)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let size = reader.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            hasher.update(&buffer[..size]);
        }
        (
            Some(file.metadata()?.len()),
            Some(format!("{:x}", hasher.finalize())),
        )
    } else {
        (None, None)
    };
    let outside_owned_paths = !task.owned_paths.is_empty()
        && !task
            .owned_paths
            .iter()
            .any(|owned| path == *owned || path.starts_with(&format!("{owned}/")));
    Ok(ChangeRecord {
        path,
        status: status.into(),
        size_bytes,
        sha256,
        outside_owned_paths,
    })
}

fn write_review(
    repo: &Path,
    base: &str,
    out: &Path,
    tasks: &[Task],
    summary: &Summary,
) -> Result<()> {
    let mut review = format!("# Agent batch review\n\nBase: `{base}`\n\nBranches and worktrees remain available. The runner does not commit or merge.\n");
    let mut all_changes: HashMap<String, Vec<String>> = HashMap::new();
    for task in tasks {
        let worktree = task.worktree.as_ref().unwrap();
        let result = summary
            .results
            .iter()
            .find(|result| result.id == task.id)
            .unwrap();
        let status = git(worktree, &["status", "--short"])?;
        let tracked = git_paths(worktree, &["diff", "--name-only", "-z", base])?;
        let untracked = git_paths(
            worktree,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let mut changes = Vec::new();
        for path in tracked {
            all_changes
                .entry(path.clone())
                .or_default()
                .push(task.id.clone());
            changes.push(change_record(worktree, task, path, "tracked")?);
        }
        for path in untracked {
            all_changes
                .entry(path.clone())
                .or_default()
                .push(task.id.clone());
            changes.push(change_record(worktree, task, path, "untracked")?);
        }
        let patch = out.join(format!("{}.patch", task.id));
        fs::write(&patch, git(worktree, &["diff", "--binary", base])?)?;
        let changes_file = out.join(format!("{}.changes.json", task.id));
        fs::write(&changes_file, serde_json::to_vec_pretty(&changes)?)?;
        let outside: Vec<_> = changes
            .iter()
            .filter(|change| change.outside_owned_paths)
            .map(|change| change.path.as_str())
            .collect();
        review.push_str(&format!("\n## {}\n\n- Branch: `{}`\n- Worktree: `{}`\n- Exit code: `{:?}`\n- Error: `{:?}`\n- Tracked patch: `{}`\n- Complete file inventory (path, size, SHA-256): `{}`\n- Outside `owned_paths`: `{:?}`\n- Reported tokens: `{:?}`\n- Client-reported cost: `{:?}`\n\n```text\n{}```\n", task.id,
            task.branch.as_deref().unwrap(), worktree.display(), result.returncode, result.error,
            patch.display(), changes_file.display(), outside, result.token_usage, result.cost, status));
    }
    let overlaps: Vec<_> = all_changes
        .iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(path, ids)| format!("- `{path}`: {}", ids.join(", ")))
        .collect();
    if !overlaps.is_empty() {
        review.push_str(&format!(
            "\n## Paths changed by multiple agents\n\n{}\n",
            overlaps.join("\n")
        ));
    }
    let status = git(repo, &["status", "--short"])?;
    review.push_str(&format!("\n## Main worktree\n\n```text\n{status}```\n"));
    fs::write(out.join("review.md"), review)?;
    Ok(())
}

fn parse_options(mode: &str, args: &[String]) -> Result<(PathBuf, Options)> {
    if args.is_empty() {
        bail!("manifest path required")
    }
    let manifest = fs::canonicalize(&args[0])?;
    let mut options = Options {
        workers: 0,
        out_dir: PathBuf::new(),
        history_db: PathBuf::new(),
        schedule: "adaptive".into(),
        timeout: None,
        reserve_ram_mb: 1024,
        reserve_cpu_cores: 1.0,
        wake_mode: "event".into(),
        repo: None,
        provider: "codex".into(),
        model: None,
        codex_profile: None,
    };
    let mut index = 1;
    while index < args.len() {
        if index + 1 >= args.len() {
            bail!("missing value for {}", args[index])
        }
        let name = args[index].as_str();
        let value = &args[index + 1];
        match name {
            "--workers" => options.workers = value.parse()?,
            "--out-dir" => options.out_dir = absolute(Path::new(value))?,
            "--history-db" => options.history_db = absolute(Path::new(value))?,
            "--schedule" => options.schedule = value.clone(),
            "--timeout" => options.timeout = Some(value.parse()?),
            "--reserve-ram-mb" => options.reserve_ram_mb = value.parse()?,
            "--reserve-cpu-cores" => options.reserve_cpu_cores = value.parse()?,
            "--wake-mode" => options.wake_mode = value.clone(),
            "--repo" if mode == "agents" => options.repo = Some(fs::canonicalize(value)?),
            "--provider" if mode == "agents" => options.provider = value.clone(),
            "--model" if mode == "agents" => options.model = Some(value.clone()),
            "--codex-profile" if mode == "agents" => options.codex_profile = Some(value.clone()),
            _ => bail!("unknown option: {name}"),
        }
        index += 2;
    }
    if options.workers == 0
        || options.out_dir.as_os_str().is_empty()
        || options.reserve_ram_mb == 0
        || !options.reserve_cpu_cores.is_finite()
        || options.reserve_cpu_cores < 0.0
        || !["adaptive", "fixed"].contains(&options.schedule.as_str())
        || !["event", "polling"].contains(&options.wake_mode.as_str())
        || options
            .timeout
            .is_some_and(|value| value <= 0.0 || !value.is_finite())
    {
        bail!("invalid runner options")
    }
    if mode == "agents"
        && (options.repo.is_none() || !["codex", "opencode"].contains(&options.provider.as_str()))
    {
        bail!("invalid agent options")
    }
    if options
        .model
        .as_ref()
        .is_some_and(|model| model.trim().is_empty())
        || (options.model.is_some() && options.provider != "codex")
        || options
            .codex_profile
            .as_ref()
            .is_some_and(|profile| profile.trim().is_empty())
        || (options.codex_profile.is_some() && options.provider != "codex")
    {
        bail!("--model and --codex-profile require nonempty values and --provider codex")
    }
    if options.out_dir.exists() {
        bail!("output directory already exists")
    }
    if options.history_db.as_os_str().is_empty() {
        options.history_db = options
            .out_dir
            .parent()
            .unwrap()
            .join("parallel-history.sqlite3");
    }
    if is_within(&options.history_db, &options.out_dir) {
        bail!("history database must be outside output directory")
    }
    if let Some(repo) = &options.repo {
        if is_within(&options.out_dir, repo) || is_within(&options.history_db, repo) {
            bail!("output and history must be outside repository")
        }
    }
    Ok((manifest, options))
}

fn run(mode: &str, args: &[String]) -> Result<i32> {
    let (manifest, options) = parse_options(mode, args)?;
    let mut tasks = load_tasks(&manifest, mode == "agents")?;
    fs::create_dir_all(&options.out_dir)?;
    let base = if mode == "agents" {
        Some(prepare_agents(&mut tasks, &options)?)
    } else {
        None
    };
    let mut summary = execute(&tasks, &options)?;
    if mode == "agents" {
        for result in &mut summary.results {
            reported_usage(result, &options.provider)?;
        }
        write_agent_summary(&options.out_dir, &summary)?;
        write_review(
            options.repo.as_ref().unwrap(),
            base.as_deref().unwrap(),
            &options.out_dir,
            &tasks,
            &summary,
        )?;
    }
    Ok(i32::from(summary.results.iter().any(|result| {
        result.returncode != Some(0) || result.error.is_some()
    })))
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if args.len() < 3 || !["tasks", "agents"].contains(&args[1].as_str()) {
        eprintln!("usage: parallel-run tasks|agents MANIFEST --workers N --out-dir PATH [options]");
        std::process::exit(2);
    }
    if let Err(error) = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst)) {
        eprintln!("cannot install interrupt handler: {error}");
        std::process::exit(2);
    }
    match run(&args[1], &args[2..]) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(if INTERRUPTED.load(Ordering::SeqCst) {
                130
            } else {
                2
            });
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn task(gpu: bool) -> Task {
        Task {
            id: "test".into(),
            workload: "test".into(),
            argv: vec![],
            cwd: PathBuf::new(),
            gpu,
            gpu_device: None,
            stdin: None,
            branch: None,
            worktree: None,
            agent: false,
            cpu_cores: None,
            ram_mb: None,
            owned_paths: vec![],
        }
    }

    fn options(workers: usize) -> Options {
        Options {
            workers,
            out_dir: PathBuf::new(),
            history_db: PathBuf::new(),
            schedule: "adaptive".into(),
            timeout: None,
            reserve_ram_mb: 100,
            reserve_cpu_cores: 1.0,
            wake_mode: "event".into(),
            repo: None,
            provider: "codex".into(),
            model: None,
            codex_profile: None,
        }
    }

    fn job(cpu: f64, ram: f64, current_ram: f64, device: Option<i32>) -> Job {
        Job {
            task: task(device.is_some()),
            estimate: Estimate {
                cpu,
                ram,
                vram: None,
            },
            pid: 0,
            start: Instant::now(),
            start_epoch: 0.0,
            device,
            initial_gpu_free: None,
            peak_cpu: 0.0,
            cpu_sum: 0.0,
            cpu_samples: 0,
            peak_ram: 0.0,
            current_ram,
            peak_vram: None,
            last_cpu: 0.0,
            last_sample: None,
            error: None,
            admission: AdmissionRecord {
                estimated_cpu_cores: cpu,
                estimated_ram_mb: ram,
                waiting_seconds: 0.0,
                wait_reason: None,
            },
        }
    }

    #[test]
    fn cpu_ram_and_worker_boundaries() {
        let mut running = HashMap::new();
        let opts = options(2);
        let candidate = task(false);
        assert!(can_launch(
            &candidate,
            &Estimate {
                cpu: 4.0,
                ram: 100.0,
                vram: None
            },
            &running,
            &opts,
            &[],
            500.0,
            2.0
        )
        .is_ok());
        running.insert("first".into(), job(1.0, 100.0, 40.0, None));
        assert_eq!(
            can_launch(
                &candidate,
                &Estimate {
                    cpu: 0.5,
                    ram: 450.0,
                    vram: None
                },
                &running,
                &opts,
                &[],
                600.0,
                2.0
            )
            .unwrap_err(),
            "insufficient available RAM"
        );
        assert!(can_launch(
            &candidate,
            &Estimate {
                cpu: 0.5,
                ram: 430.0,
                vram: None
            },
            &running,
            &opts,
            &[],
            600.0,
            2.0
        )
        .is_ok());
        assert_eq!(
            can_launch(
                &candidate,
                &Estimate {
                    cpu: 1.5,
                    ram: 100.0,
                    vram: None
                },
                &running,
                &opts,
                &[],
                600.0,
                2.0
            )
            .unwrap_err(),
            "insufficient CPU budget"
        );
        running.insert("second".into(), job(0.1, 10.0, 0.0, None));
        assert_eq!(
            can_launch(
                &candidate,
                &Estimate {
                    cpu: 0.1,
                    ram: 10.0,
                    vram: None
                },
                &running,
                &opts,
                &[],
                600.0,
                2.0
            )
            .unwrap_err(),
            "worker limit reached"
        );
    }

    #[test]
    fn occupied_gpu_can_use_another_device() {
        let mut running = HashMap::new();
        running.insert("first".into(), job(0.1, 10.0, 0.0, Some(0)));
        let inventory = vec![
            Device {
                index: 0,
                free_mb: 4096,
            },
            Device {
                index: 1,
                free_mb: 4096,
            },
        ];
        let candidate = task(true);
        let est = Estimate {
            cpu: 0.1,
            ram: 10.0,
            vram: Some(100.0),
        };
        assert_eq!(
            can_launch(
                &candidate,
                &est,
                &running,
                &options(2),
                &inventory,
                1000.0,
                2.0
            ),
            Ok(Some(1))
        );
    }
}
