/// bcachefs-package-ci: self-hosted .deb build orchestrator
///
/// Reconcile loop that builds Debian packages for bcachefs-tools across
/// a matrix of distros and architectures. Runs as `aptbcachefsorg` user
/// on evilpiepirate.org, with arm64 builds dispatched via ssh to farm1.
///
/// State is filesystem-based under $STATE_DIR:
///   desired              — target commit hash (written by post-receive hook)
///   builds/$commit/
///     source/status      — source package build state
///     source/log         — build log
///     $distro-$arch/     — per-job state + log + artifacts
///
/// The reconcile pattern: no queue. We know what we want (latest commit
/// with packages for every distro×arch) and what we have (filesystem state).
/// The loop fills the gap. New push = update desired, loop picks it up.
/// Same pattern as ktest CI and the filesystem's own reconcile pass.

use anyhow::{Context, Result, bail};
use chrono::Local;
use log::{error, info, warn};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Build matrix
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Distro {
    Unstable,
    Forky,
    Trixie,
    Questing,
    Plucky,
}

impl Distro {
    const ALL: &[Distro] = &[
        Distro::Unstable,
        Distro::Forky,
        Distro::Trixie,
        Distro::Questing,
        Distro::Plucky,
    ];

    fn is_ubuntu(self) -> bool {
        matches!(self, Distro::Plucky | Distro::Questing)
    }

    fn as_str(self) -> &'static str {
        match self {
            Distro::Unstable => "unstable",
            Distro::Forky => "forky",
            Distro::Trixie => "trixie",
            Distro::Questing => "questing",
            Distro::Plucky => "plucky",
        }
    }

    /// Mirror URL for sbuild chroot creation
    fn mirror(self) -> &'static str {
        if self.is_ubuntu() {
            "http://archive.ubuntu.com/ubuntu"
        } else {
            "http://deb.debian.org/debian"
        }
    }

    fn container_image(self) -> &'static str {
        match self {
            Distro::Unstable => "debian:unstable-slim",
            Distro::Forky    => "debian:forky-slim",
            Distro::Trixie   => "debian:trixie-slim",
            Distro::Plucky   => "ubuntu:plucky",
            Distro::Questing => "ubuntu:questing",
        }
    }

    fn parse(s: &str) -> Option<Distro> {
        match s {
            "unstable" => Some(Distro::Unstable),
            "forky"    => Some(Distro::Forky),
            "trixie"   => Some(Distro::Trixie),
            "questing" => Some(Distro::Questing),
            "plucky"   => Some(Distro::Plucky),
            _          => None,
        }
    }
}

impl fmt::Display for Distro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Arch {
    Amd64,
    Ppc64el,
    Arm64,
}

impl Arch {
    const ALL: &[Arch] = &[Arch::Amd64, Arch::Ppc64el, Arch::Arm64];

    fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Ppc64el => "ppc64el",
            Arch::Arm64 => "arm64",
        }
    }

    /// Whether this arch is built via cross-compilation on amd64
    fn is_cross(self) -> bool {
        matches!(self, Arch::Ppc64el)
    }

    /// Whether this arch is built on a remote host
    fn is_remote(self) -> bool {
        matches!(self, Arch::Arm64)
    }

    fn parse(s: &str) -> Option<Arch> {
        match s {
            "amd64"   => Some(Arch::Amd64),
            "ppc64el" => Some(Arch::Ppc64el),
            "arm64"   => Some(Arch::Arm64),
            _         => None,
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Job {
    distro: Distro,
    arch:   Arch,
}

impl Job {
    fn name(&self) -> String {
        format!("{}-{}", self.distro, self.arch)
    }
}

impl fmt::Display for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.distro, self.arch)
    }
}

fn build_matrix() -> Vec<Job> {
    let mut jobs = Vec::new();
    for &distro in Distro::ALL {
        for &arch in Arch::ALL {
            // ppc64el cross-build is broken on Ubuntu
            if arch == Arch::Ppc64el && distro.is_ubuntu() {
                continue;
            }
            jobs.push(Job { distro, arch });
        }
    }
    jobs
}

// ---------------------------------------------------------------------------
// Job state (filesystem-backed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Pending,
    Building,
    Done,
    Failed,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending  => "pending",
            JobStatus::Building => "building",
            JobStatus::Done     => "done",
            JobStatus::Failed   => "failed",
        }
    }

    fn parse(s: &str) -> Option<JobStatus> {
        match s.trim() {
            "pending"  => Some(JobStatus::Pending),
            "building" => Some(JobStatus::Building),
            "done"     => Some(JobStatus::Done),
            "failed"   => Some(JobStatus::Failed),
            _          => None,
        }
    }
}

/// Filesystem-backed state for one commit's builds
struct BuildState {
    state_dir: PathBuf,
}

impl BuildState {
    fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Read the desired commit hash (written by post-receive hook)
    fn desired_commit(&self) -> Result<Option<String>> {
        let path = self.state_dir.join("desired");
        match fs::read_to_string(&path) {
            Ok(s) => {
                let commit = s.trim().to_string();
                if commit.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(commit))
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading desired commit"),
        }
    }

    fn commit_dir(&self, commit: &str) -> PathBuf {
        self.state_dir.join("builds").join(commit)
    }

    fn job_dir(&self, commit: &str, job_name: &str) -> PathBuf {
        self.commit_dir(commit).join(job_name)
    }

    fn read_status(&self, commit: &str, job_name: &str) -> JobStatus {
        let status_path = self.job_dir(commit, job_name).join("status");
        match fs::read_to_string(&status_path) {
            Ok(s) => JobStatus::parse(&s).unwrap_or(JobStatus::Pending),
            Err(_) => JobStatus::Pending,
        }
    }

    fn write_status(&self, commit: &str, job_name: &str, status: JobStatus) -> Result<()> {
        let dir = self.job_dir(commit, job_name);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating job dir {}", dir.display()))?;
        let path = dir.join("status");
        fs::write(&path, status.as_str())
            .with_context(|| format!("writing status to {}", path.display()))?;
        self.regenerate_html();
        Ok(())
    }

    fn regenerate_html(&self) {
        let script = self.state_dir.join("scripts/generate-status-html.sh");
        if script.exists() {
            let _ = Command::new("bash")
                .arg(&script)
                .env("STATE_DIR", &self.state_dir)
                .status();
        }
    }

    fn log_path(&self, commit: &str, job_name: &str) -> PathBuf {
        self.job_dir(commit, job_name).join("log")
    }

    fn pid_path(&self, commit: &str, job_name: &str) -> PathBuf {
        self.job_dir(commit, job_name).join("pid")
    }

    /// Check if a "building" job's process is actually still running.
    /// If the orchestrator crashed and restarted, a job might be marked
    /// "building" with a stale PID. Detect and recover.
    fn is_process_alive(&self, commit: &str, job_name: &str) -> bool {
        let pid_path = self.pid_path(commit, job_name);
        let pid_str = match fs::read_to_string(&pid_path) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let pid: u32 = match pid_str.trim().parse() {
            Ok(p) => p,
            Err(_) => return false,
        };
        // kill(pid, 0) checks if process exists without sending a signal
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn result_dir(&self, commit: &str, job_name: &str) -> PathBuf {
        self.job_dir(commit, job_name).join("result")
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct Config {
    /// Path to the bare git repo
    git_repo:    PathBuf,
    /// Root of the state directory
    state_dir:   PathBuf,
    /// Path to ci/scripts/ in the repo checkout
    scripts_dir: PathBuf,
    /// SSH target for arm64 builds
    arm64_host:  String,
    /// Aptly root directory
    aptly_root:  PathBuf,
    /// Maximum concurrent local builds
    max_local_jobs: usize,
    /// Maximum concurrent remote (arm64) builds
    max_remote_jobs: usize,
    /// Poll interval when idle
    poll_interval: Duration,
    /// Pinned Rust version for rustup
    rust_version: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            git_repo:       PathBuf::from("/var/www/git/bcachefs-tools.git"),
            state_dir:      PathBuf::from("/home/aptbcachefsorg/package-ci"),
            scripts_dir:    PathBuf::from("/home/aptbcachefsorg/package-ci/scripts"),
            arm64_host:     "farm1.evilpiepirate.org".into(),
            aptly_root:     PathBuf::from("/home/aptbcachefsorg/uploads/aptly"),
            max_local_jobs:  2,
            max_remote_jobs: 1,
            poll_interval:   Duration::from_secs(60),
            rust_version:    "1.89.0".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Podman container helper
// ---------------------------------------------------------------------------

/// RAII wrapper around a podman container. Cleans up on drop.
struct PodmanContainer {
    name: String,
}

impl PodmanContainer {
    /// Start a detached container with the given volumes.
    /// Each volume is (host_path, container_path, mode).
    fn create(
        name: &str,
        image: &str,
        volumes: &[(&str, &str, &str)],
    ) -> Result<Self> {
        let mut cmd = Command::new("podman");
        cmd.args(["run", "--name", name, "--detach", "--init"]);
        cmd.args(["--security-opt", "seccomp=unconfined"]);
        cmd.args(["--security-opt", "apparmor=unconfined"]);
        cmd.args(["--device", "/dev/fuse"]);
        cmd.args(["--cap-add", "SYS_ADMIN"]);
        for &(host, container, mode) in volumes {
            cmd.arg("--volume");
            cmd.arg(format!("{host}:{container}:{mode}"));
        }
        cmd.args(["--tmpfs", "/tmp:exec"]);
        cmd.args([image, "sleep", "infinity"]);

        let output = cmd.output().context("starting podman container")?;
        if !output.status.success() {
            bail!("podman run failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(Self { name: name.to_string() })
    }

    /// Execute a command with proper argv (no shell interpretation).
    fn exec(&self, args: &[&str]) -> Result<()> {
        let status = Command::new("podman")
            .arg("exec")
            .arg(&self.name)
            .args(args)
            .status()
            .with_context(|| format!("podman exec {:?}", args))?;
        if !status.success() {
            bail!("command failed in container: {:?}", args);
        }
        Ok(())
    }

    /// Execute a shell script inside the container.
    /// Use sparingly — prefer exec() with proper argv where possible.
    fn exec_shell(&self, script: &str) -> Result<()> {
        let status = Command::new("podman")
            .args(["exec", &self.name, "bash", "-euxc", script])
            .status()
            .context("podman exec bash")?;
        if !status.success() {
            bail!("shell command failed in container");
        }
        Ok(())
    }

    /// Write a file into the container by piping content through stdin.
    /// No shell quoting involved — content is passed as raw bytes.
    fn write_file(&self, path: &str, content: &str) -> Result<()> {
        let mut child = Command::new("podman")
            .args(["exec", "-i", &self.name, "tee", path])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .context("podman exec tee")?;
        child.stdin.take().unwrap().write_all(content.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("writing file {path} in container failed");
        }
        Ok(())
    }
}

impl Drop for PodmanContainer {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", &self.name])
            .status();
    }
}

// ---------------------------------------------------------------------------
// Binary package build (replaces build-binary.sh)
// ---------------------------------------------------------------------------

fn find_dsc(source_dir: &Path) -> Result<PathBuf> {
    for entry in fs::read_dir(source_dir).context("reading source dir")? {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) == Some("dsc") {
            return Ok(entry.path());
        }
    }
    bail!("no .dsc file found in {}", source_dir.display())
}

/// Check if system Rust in the container is new enough (>= 1.85).
fn container_rust_is_new_enough(container: &PodmanContainer) -> bool {
    let output = Command::new("podman")
        .args(["exec", &container.name, "rustc", "--version"])
        .output();

    let Ok(output) = output else { return false };
    let Ok(ver_str) = String::from_utf8(output.stdout) else { return false };

    // Parse "rustc 1.XX.Y ..." and check minor >= 85
    ver_str.split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').nth(1))
        .and_then(|minor| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor >= 85)
}

/// Install rustup and cargo shim if the distro's Rust is too old.
fn install_rust_if_needed(container: &PodmanContainer, version: &str) -> Result<()> {
    if container_rust_is_new_enough(container) {
        return Ok(());
    }

    info!("distro Rust too old, installing rustup {version}");
    container.exec_shell(&format!(
        "curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | \
         sh -s -- --default-toolchain {version} --profile minimal -y"
    ))?;

    // Write cargo shim to a temp file — no quoting layers, just raw content.
    // The shim handles Ubuntu's "prepare-debian" vendor setup step as a no-op.
    container.write_file("/tmp/cargo-shim", "\
#!/bin/sh
[ \"$1\" = \"prepare-debian\" ] && exit 0
exec /root/.cargo/bin/cargo \"$@\"
")?;

    // Install shim if the Ubuntu cargo wrapper exists
    container.exec_shell("\
        if [ -f /usr/share/cargo/bin/cargo ]; then \
            cp /tmp/cargo-shim /usr/share/cargo/bin/cargo; \
            chmod +x /usr/share/cargo/bin/cargo; \
        fi; \
        ln -sf /root/.cargo/bin/rustc /usr/bin/rustc")?;

    Ok(())
}

/// Build binary .deb packages for a specific distro×arch inside a podman container.
fn build_binary(
    distro: Distro,
    arch: Arch,
    commit: &str,
    source_dir: &Path,
    result_dir: &Path,
    rust_version: &str,
    cache_dir: &Path,
) -> Result<()> {
    let container_name = format!("ci-binary-{distro}-{arch}-{}", std::process::id());
    let apt_cache_dir = cache_dir.join(format!("apt-{distro}-{arch}"));
    fs::create_dir_all(result_dir)?;
    fs::create_dir_all(&apt_cache_dir)?;

    info!("=== Building binary: {distro} {arch} (commit {}) ===", &commit[..commit.len().min(12)]);

    let container = PodmanContainer::create(
        &container_name,
        distro.container_image(),
        &[
            (source_dir.to_str().unwrap(), "/source", "ro"),
            (result_dir.to_str().unwrap(), "/result", "rw"),
            (apt_cache_dir.to_str().unwrap(), "/var/cache/apt", "rw"),
        ],
    )?;

    // Clear stale apt locks from crashed previous containers
    for lock in [
        "/var/cache/apt/archives/lock",
        "/var/lib/apt/lists/lock",
        "/var/lib/dpkg/lock",
        "/var/lib/dpkg/lock-frontend",
    ] {
        container.exec(&["rm", "-f", lock])?;
    }

    // Cross-compilation: add foreign arch before first apt-get update
    if arch.is_cross() {
        container.exec(&["dpkg", "--add-architecture", "ppc64el"])?;
    }

    // Install essential build tools
    container.exec_shell(
        "apt-get update && \
         DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
         build-essential ca-certificates curl devscripts dpkg-dev"
    )?;

    // Cross-compilation tools
    if arch.is_cross() {
        container.exec_shell(
            "DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
             qemu-user-static binfmt-support gcc-powerpc64le-linux-gnu"
        )?;
    }

    // Find .dsc and extract source package
    let dsc = find_dsc(source_dir)?;
    let dsc_name = dsc.file_name().unwrap().to_str().unwrap();
    let cross_dep = if arch.is_cross() { "--host-architecture ppc64el" } else { "" };

    container.exec_shell(&format!(
        "mkdir -p /build && \
         cp /source/* /build/ && \
         cd /build && \
         dpkg-source -x {dsc_name} src && \
         DEBIAN_FRONTEND=noninteractive apt-get build-dep -y {cross_dep} ./src"
    ))?;

    // Install rustup if distro Rust is too old
    install_rust_if_needed(&container, rust_version)?;

    // Build
    let cross_dpkg = if arch.is_cross() { "-a ppc64el" } else { "" };
    container.exec_shell(&format!(
        "export RUSTUP_HOME=\"$HOME/.rustup\" \
                CARGO_HOME=\"$HOME/.cargo\" \
                PATH=\"$HOME/.cargo/bin:$PATH\" && \
         cd /build/src && \
         dpkg-buildpackage -us -uc -b {cross_dpkg}"
    ))?;

    // Copy results out
    for pattern in ["*.deb", "*.ddeb", "*.changes", "*.buildinfo"] {
        container.exec_shell(&format!(
            "find /build -maxdepth 1 -name '{pattern}' -exec cp {{}} /result/ \\;"
        ))?;
    }

    info!("=== Binary build complete: {distro} {arch} ===");
    Ok(())
}

// ---------------------------------------------------------------------------
// Running process tracking
// ---------------------------------------------------------------------------

struct RunningJob {
    child:    Child,
    job_name: String,
    commit:   String,
    remote:   bool,
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

struct Orchestrator {
    config:  Config,
    state:   BuildState,
    running: Vec<RunningJob>,
    last_desired: Option<String>,
}

impl Orchestrator {
    fn new(config: Config) -> Self {
        let state = BuildState::new(config.state_dir.clone());
        Self {
            config,
            state,
            running: Vec::new(),
            last_desired: None,
        }
    }

    /// One iteration of the reconcile loop
    fn reconcile(&mut self) -> Result<()> {
        // Reap finished children first
        self.reap_children()?;

        let commit = match self.state.desired_commit()? {
            Some(c) => c,
            None => return Ok(()),
        };

        // Log when desired commit changes
        if self.last_desired.as_deref() != Some(&commit) {
            info!("[{}] new desired commit", &commit[..12]);
            self.last_desired = Some(commit.clone());
        }

        // Phase 1: source package
        let source_status = self.effective_status(&commit, "source");
        match source_status {
            JobStatus::Done => {}
            JobStatus::Building => {
                // Source still building, can't start binaries yet
                return Ok(());
            }
            JobStatus::Failed => {
                // Nothing to do until new push or manual retry
                return Ok(());
            }
            JobStatus::Pending => {
                info!("[{}] starting source package build", &commit[..12]);
                self.spawn_source_build(&commit)?;
                return Ok(());
            }
        }

        // Phase 2: binary builds
        let matrix = build_matrix();
        let mut still_running = false;
        let mut any_failed = false;

        for job in &matrix {
            let name = job.name();
            let status = self.effective_status(&commit, &name);
            match status {
                JobStatus::Done => {}
                JobStatus::Building => {
                    still_running = true;
                }
                JobStatus::Failed => {
                    any_failed = true;
                    warn!("[{}] build failed: {}", &commit[..12], name);
                }
                JobStatus::Pending => {
                    still_running = true;
                    if self.have_build_slot(job) {
                        info!("[{}] starting binary build: {}", &commit[..12], name);
                        self.spawn_binary_build(&commit, job)?;
                    }
                }
            }
        }

        // Phase 3: publish when nothing is still building/pending
        // Publish even if some builds failed — partial results are better than nothing
        if !still_running {
            let pub_status = self.effective_status(&commit, "publish");
            if pub_status == JobStatus::Pending {
                if any_failed {
                    info!("[{}] some builds failed, publishing successful ones", &commit[..12]);
                } else {
                    info!("[{}] all builds complete, publishing", &commit[..12]);
                }
                self.spawn_publish(&commit)?;
            }
        }

        Ok(())
    }

    /// Get effective status, fixing up stale "building" entries
    fn effective_status(&self, commit: &str, job_name: &str) -> JobStatus {
        let status = self.state.read_status(commit, job_name);
        if status == JobStatus::Building {
            // Check if it's one of our tracked children
            let is_tracked = self.running.iter()
                .any(|r| r.commit == commit && r.job_name == job_name);
            if !is_tracked && !self.state.is_process_alive(commit, job_name) {
                // Stale "building" from a crashed orchestrator run.
                // Reset to failed so it can be retried on next push.
                warn!("[{}] {} marked building but process dead, marking failed",
                      &commit[..12], job_name);
                let _ = self.state.write_status(commit, job_name, JobStatus::Failed);
                return JobStatus::Failed;
            }
        }
        status
    }

    /// Reap finished child processes and update their state
    fn reap_children(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.running.len() {
            match self.running[i].child.try_wait() {
                Ok(Some(exit_status)) => {
                    let job = self.running.remove(i);
                    let status = if exit_status.success() {
                        JobStatus::Done
                    } else {
                        JobStatus::Failed
                    };
                    info!("[{}] {} finished: {:?}",
                          &job.commit[..12], job.job_name, status);
                    self.state.write_status(&job.commit, &job.job_name, status)?;
                    // Clean up PID file
                    let _ = fs::remove_file(self.state.pid_path(&job.commit, &job.job_name));
                }
                Ok(None) => {
                    // Still running
                    i += 1;
                }
                Err(e) => {
                    error!("error waiting on child: {}", e);
                    i += 1;
                }
            }
        }
        Ok(())
    }

    fn have_build_slot(&self, job: &Job) -> bool {
        let (local, remote) = self.running.iter()
            .filter(|r| r.job_name != "source" && r.job_name != "publish")
            .fold((0, 0), |(l, r), j| {
                if j.remote { (l, r + 1) } else { (l + 1, r) }
            });

        if job.arch.is_remote() {
            remote < self.config.max_remote_jobs
        } else {
            local < self.config.max_local_jobs
        }
    }

    fn spawn_source_build(&mut self, commit: &str) -> Result<()> {
        self.state.write_status(commit, "source", JobStatus::Building)?;
        let log_file = fs::File::create(self.state.log_path(commit, "source"))
            .context("creating source build log")?;

        let child = Command::new(&self.config.scripts_dir.join("build-source.sh"))
            .arg(commit)
            .arg(&self.config.git_repo)
            .arg(self.state.result_dir(commit, "source"))
            .arg(&self.config.rust_version)
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file))
            .spawn()
            .context("spawning source build")?;

        self.write_pid(commit, "source", &child)?;
        self.running.push(RunningJob {
            child,
            job_name: "source".into(),
            commit: commit.into(),
            remote: false,
        });
        Ok(())
    }

    fn spawn_binary_build(&mut self, commit: &str, job: &Job) -> Result<()> {
        let name = job.name();
        self.state.write_status(commit, &name, JobStatus::Building)?;
        let log_file = fs::File::create(self.state.log_path(commit, &name))
            .context("creating binary build log")?;

        let source_result = self.state.result_dir(commit, "source");
        let build_result = self.state.result_dir(commit, &name);
        fs::create_dir_all(&build_result)?;

        let child = if job.arch.is_remote() {
            // arm64: scp artifacts to farm1, build there, scp results back
            Command::new(&self.config.scripts_dir.join("build-binary-remote.sh"))
                .arg(&self.config.arm64_host)
                .arg(job.distro.as_str())
                .arg(job.arch.as_str())
                .arg(commit)
                .arg(&source_result)
                .arg(&build_result)
                .arg(&self.config.rust_version)
                .stdout(Stdio::from(log_file.try_clone()?))
                .stderr(Stdio::from(log_file))
                .spawn()
                .with_context(|| format!("spawning remote build for {}", name))?
        } else {
            // Local builds use our own binary — no shell quoting layers
            Command::new(std::env::current_exe().context("getting own binary path")?)
                .arg("build-binary")
                .arg(job.distro.as_str())
                .arg(job.arch.as_str())
                .arg(commit)
                .arg(&source_result)
                .arg(&build_result)
                .arg(&self.config.rust_version)
                .stdout(Stdio::from(log_file.try_clone()?))
                .stderr(Stdio::from(log_file))
                .spawn()
                .with_context(|| format!("spawning local build for {}", name))?
        };

        self.write_pid(commit, &name, &child)?;
        self.running.push(RunningJob {
            child,
            job_name: name,
            commit: commit.into(),
            remote: job.arch.is_remote(),
        });
        Ok(())
    }

    fn spawn_publish(&mut self, commit: &str) -> Result<()> {
        self.state.write_status(commit, "publish", JobStatus::Building)?;
        let log_file = fs::File::create(self.state.log_path(commit, "publish"))
            .context("creating publish log")?;

        let child = Command::new(&self.config.scripts_dir.join("publish.sh"))
            .arg(commit)
            .env("STATE_DIR", &self.config.state_dir)
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file))
            .spawn()
            .context("spawning publish")?;

        self.write_pid(commit, "publish", &child)?;
        self.running.push(RunningJob {
            child,
            job_name: "publish".into(),
            commit: commit.into(),
            remote: false,
        });
        Ok(())
    }

    fn write_pid(&self, commit: &str, job_name: &str, child: &Child) -> Result<()> {
        let pid_path = self.state.pid_path(commit, job_name);
        fs::write(&pid_path, child.id().to_string())
            .with_context(|| format!("writing pid to {}", pid_path.display()))
    }

    /// Kill all running children (for clean shutdown)
    fn kill_all(&mut self) {
        for job in &mut self.running {
            info!("killing {}", job.job_name);
            let _ = job.child.kill();
        }
        // Wait for them to exit
        for job in &mut self.running {
            let _ = job.child.wait();
        }
        self.running.clear();
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

struct Signals {
    shutdown: Arc<AtomicBool>,
    wakeup:   Arc<AtomicBool>,
}

fn setup_signals() -> Result<Signals> {
    let wakeup = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));

    // SIGUSR1 = wake up (new push arrived)
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, Arc::clone(&wakeup))?;
    // SIGTERM/SIGINT = clean shutdown
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;

    Ok(Signals { shutdown, wakeup })
}

fn write_pid_file(state_dir: &Path) -> Result<()> {
    let pid_path = state_dir.join("orchestrator.pid");
    fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("writing PID file {}", pid_path.display()))
}

fn remove_pid_file(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join("orchestrator.pid"));
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn run_build_binary() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // build-binary DISTRO ARCH COMMIT SOURCE_DIR RESULT_DIR RUST_VERSION
    if args.len() != 8 {
        bail!("usage: {} build-binary DISTRO ARCH COMMIT SOURCE_DIR RESULT_DIR RUST_VERSION",
              args[0]);
    }
    let distro = Distro::parse(&args[2])
        .with_context(|| format!("unknown distro: {}", args[2]))?;
    let arch = Arch::parse(&args[3])
        .with_context(|| format!("unknown arch: {}", args[3]))?;
    let commit = &args[4];
    let source_dir = PathBuf::from(&args[5]);
    let result_dir = PathBuf::from(&args[6]);
    let rust_version = &args[7];
    let cache_dir = PathBuf::from(
        std::env::var("CACHE_DIR")
            .unwrap_or_else(|_| "/home/aptbcachefsorg/package-ci/cache".into())
    );

    build_binary(distro, arch, commit, &source_dir, &result_dir, rust_version, &cache_dir)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            writeln!(buf, "{} [{}] {}",
                     Local::now().format("%Y-%m-%d %H:%M:%S"),
                     record.level(),
                     record.args())
        })
        .init();

    // Subcommand dispatch
    if std::env::args().nth(1).as_deref() == Some("build-binary") {
        if let Err(e) = run_build_binary() {
            error!("build-binary failed: {e:?}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    if let Err(e) = run_orchestrator() {
        error!("orchestrator failed: {e:?}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_orchestrator() -> Result<()> {
    let config = Config::default();
    info!("bcachefs-ci starting");
    info!("  git repo:    {}", config.git_repo.display());
    info!("  state dir:   {}", config.state_dir.display());
    info!("  aptly root:  {}", config.aptly_root.display());
    info!("  arm64 host:  {}", config.arm64_host);
    info!("  rust version: {}", config.rust_version);

    // Ensure state directory exists
    fs::create_dir_all(&config.state_dir)
        .context("creating state directory")?;

    write_pid_file(&config.state_dir)?;

    let poll_interval = config.poll_interval;
    let state_dir = config.state_dir.clone();
    let mut orchestrator = Orchestrator::new(config);

    let signals = setup_signals()?;

    info!("entering reconcile loop (poll every {}s, SIGUSR1 for immediate wake)",
          poll_interval.as_secs());

    while !signals.shutdown.load(Ordering::Relaxed) {
        if let Err(e) = orchestrator.reconcile() {
            error!("reconcile error: {:?}", e);
        }

        // Sleep in small increments; break early on SIGUSR1 or shutdown
        let start = Instant::now();
        while start.elapsed() < poll_interval
            && !signals.shutdown.load(Ordering::Relaxed)
            && !signals.wakeup.swap(false, Ordering::Relaxed)
        {
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    info!("shutting down, killing running builds");
    orchestrator.kill_all();
    remove_pid_file(&state_dir);
    info!("bcachefs-ci stopped");
    Ok(())
}
