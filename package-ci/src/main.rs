/// bcachefs-package-ci: self-hosted .deb build orchestrator
///
/// Reconcile loop that builds Debian packages for bcachefs-tools across
/// a matrix of distros and architectures. Runs as `aptbcachefsorg` user
/// on evilpiepirate.org, with arm64 builds dispatched via ssh to farm1.
///
/// State is filesystem-based under $STATE_DIR:
///   desired              — target commit hash (written by post-receive hook)
///   desired-release      — commit a v* tag was just pushed at (same hook)
///   builds/$id/
///     source/status      — source package build state
///     source/log         — build log
///     $distro-$arch/     — per-job state + log + artifacts
///
/// `$id` is a build id, not necessarily a commit: a snapshot build is keyed by
/// its commit, a release build by its tag. They are separate builds because
/// they produce different artifacts — see `Build`.
///
/// The reconcile pattern: no queue. We know what we want (latest commit
/// with packages for every distro×arch) and what we have (filesystem state).
/// The loop fills the gap. New push = update desired, loop picks it up.
/// Same pattern as ktest CI and the filesystem's own reconcile pass.

use anyhow::{Context, Result};
use chrono::Local;
use log::{error, info, warn};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    Resolute,
    Questing,
    Plucky,
}

impl Distro {
    const ALL: &[Distro] = &[
        Distro::Unstable,
        Distro::Forky,
        Distro::Trixie,
        Distro::Resolute,
        Distro::Questing,
        Distro::Plucky,
    ];

    fn is_ubuntu(self) -> bool {
        matches!(self, Distro::Plucky | Distro::Questing | Distro::Resolute)
    }

    fn as_str(self) -> &'static str {
        match self {
            Distro::Unstable => "unstable",
            Distro::Forky => "forky",
            Distro::Trixie => "trixie",
            Distro::Resolute => "resolute",
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

/// One build: what to check out, and what we are building it *as*.
///
/// `id` is the key for everything — the directory under `builds/`, every status
/// lookup, the log paths, the remote scratch dir, the label on ci.html.
/// `commit` is only what gets checked out.
///
/// A tag and the commit it points at are **separate builds**, deliberately.
/// They produce genuinely different artifacts: different version strings, bound
/// for different suites. The version is baked into the source package that all
/// twelve binary builds descend from, so nothing can relabel them afterwards.
///
/// Modelling them as one build carrying a mutable "is this a release?" flag is
/// what shipped v1.39.1 into the release suite with a snapshot version. The
/// flag was derived twice from ambient git state, 42 minutes apart — at build
/// start (deciding the version) and at publish start (deciding the suite) — and
/// the tag ref landed 9 seconds after the first one looked. Both derivations
/// were individually correct. They simply disagreed, and by then the version
/// was already baked into artifacts nothing could relabel.
///
/// Two builds, each with its identity fixed at creation, removes the question
/// instead of policing the answer. It also means a tagged commit reaches both
/// channels: a commit publishes to exactly one suite, so tagging used to starve
/// the snapshot channel of that commit entirely.
#[derive(Clone, Debug)]
struct Build {
    id:     String,
    commit: String,
    /// Set for a release build; the version comes from it verbatim.
    tag:    Option<String>,
}

impl Build {
    fn snapshot(commit: String) -> Self {
        Self { id: commit.clone(), commit, tag: None }
    }

    fn release(tag: String, commit: String) -> Self {
        Self { id: tag.clone(), commit, tag: Some(tag) }
    }

    fn is_release(&self) -> bool {
        self.tag.is_some()
    }

    /// Short label for logs. Deliberately not `&id[..12]`: a tag id is usually
    /// shorter than that, and slicing a str past its end panics.
    fn short(&self) -> &str {
        short(&self.id)
    }
}

fn short(id: &str) -> &str {
    match id.char_indices().nth(12) {
        Some((i, _)) => &id[..i],
        None => id,
    }
}

/// Filesystem-backed state, keyed by build id (see `Build`)
struct BuildState {
    state_dir: PathBuf,
    /// Web root where ci.html is written. Defaults next to state_dir (the bash
    /// generator's /home/aptbcachefsorg/public_html), overridable via PUBLIC_HTML.
    public_html: PathBuf,
}

impl BuildState {
    fn new(state_dir: PathBuf) -> Self {
        let public_html = std::env::var_os("PUBLIC_HTML")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                state_dir.parent().unwrap_or(Path::new("/")).join("public_html")
            });
        Self { state_dir, public_html }
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

    /// Read the desired *release* commit (written by post-receive for v* tags).
    /// A queued release is built to completion before any snapshot, so a master
    /// push can't preempt it. Mirrors desired_commit().
    fn desired_release(&self) -> Result<Option<String>> {
        let path = self.state_dir.join("desired-release");
        match fs::read_to_string(&path) {
            Ok(s) => {
                let commit = s.trim().to_string();
                Ok(if commit.is_empty() { None } else { Some(commit) })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading desired-release commit"),
        }
    }

    /// Clear the queued release once its build has finished (published or failed).
    fn clear_desired_release(&self) -> Result<()> {
        let path = self.state_dir.join("desired-release");
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("clearing desired-release"),
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

    fn ensure_status(&self, commit: &str, job_name: &str, status: JobStatus) -> Result<bool> {
        let dir = self.job_dir(commit, job_name);
        let path = dir.join("status");
        if path.exists() {
            return Ok(false);
        }

        fs::create_dir_all(&dir)
            .with_context(|| format!("creating job dir {}", dir.display()))?;
        fs::write(&path, status.as_str())
            .with_context(|| format!("writing status to {}", path.display()))?;
        Ok(true)
    }

    /// Render the static status page in-process via the shared ci-dashboard
    /// crate (was scripts/generate-status-html.sh). The build tree is the
    /// filesystem-as-state contract: builds/<commit>/<job>/ each with a `status`
    /// file and a `log`. Best-effort — a failed render must not break a build.
    fn regenerate_html(&self) {
        let root = self.state_dir.join("builds");
        let tmpl = ci_dashboard::Template::parse("{commit}/{job}");
        let cols: Vec<String> = tmpl.column_names().iter().map(|s| s.to_string()).collect();
        let jobs = tmpl.discover(&root, &[]);
        let opts = ci_dashboard::RenderOpts {
            title: "bcachefs-tools CI".into(),
            refresh_secs: 30,
            // httpd maps /ci-builds → builds/, so {commit}/{job}/log resolves.
            log_url_prefix: Some("/ci-builds".into()),
            section_by: None,          // first column (commit) becomes the section
            group_building_by: None,   // no shared toolchain to surface here
            stuck_mins: 120,           // matches build_timeout; pid recovery is primary
        };
        let html = ci_dashboard::render(&jobs, &cols, &[], &opts, std::time::SystemTime::now());

        let out = self.public_html.join("ci.html");
        let tmp = out.with_extension("html.tmp");
        if let Err(e) = std::fs::write(&tmp, &html).and_then(|_| std::fs::rename(&tmp, &out)) {
            warn!("regenerate ci.html ({}): {e}", out.display());
        }
    }

    fn log_path(&self, commit: &str, job_name: &str) -> PathBuf {
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%S");
        let dir = self.job_dir(commit, job_name);
        let timestamped = dir.join(format!("log-{}", now));
        // Symlink "log" → timestamped file so ci.html links work
        let stable = dir.join("log");
        let _ = std::fs::remove_file(&stable);
        let _ = std::os::unix::fs::symlink(format!("log-{}", now), &stable);
        timestamped
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
    /// Maximum time for a single build before it gets killed
    build_timeout: Duration,
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
            build_timeout:   Duration::from_secs(2 * 60 * 60), // 2 hours
        }
    }
}

// ---------------------------------------------------------------------------
// Running process tracking
// ---------------------------------------------------------------------------

struct RunningJob {
    child:      Child,
    job_name:   String,
    commit:     String,
    remote:     bool,
    started_at: Instant,
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

    /// Choose the commit to build: a queued release is built to completion
    /// before any snapshot, so a master push can't preempt it (that's how
    /// v1.38.6 got stranded). Once the release publishes, fall through to the
    /// latest master commit.
    fn pick_build(&self) -> Result<Option<Build>> {
        if let Some(commit) = self.state.desired_release()? {
            // Resolve the tag once, here, and carry it. Unlike deriving
            // release-ness from a commit — which depends on *when* you ask —
            // this lookup cannot race: post-receive only writes desired-release
            // in response to a tag push, and refs are updated before the hook
            // runs, so the tag is already in the repo by definition.
            match self.tag_for(&commit) {
                Some(tag) => {
                    let build = Build::release(tag, commit);
                    if !self.is_build_finished(&build.id) {
                        return Ok(Some(build));
                    }
                    // Released (or unbuildable) — stop pinning it.
                    self.state.clear_desired_release()?;
                }
                None => {
                    // Don't quietly build it as a snapshot: that is precisely
                    // the failure this design exists to prevent. Leave it
                    // queued and visible.
                    error!("[{}] queued as a release but no tag points at it; \
                            not building it as a snapshot. Check the tag reached {}",
                           short(&commit), self.config.git_repo.display());
                }
            }
        }
        Ok(self.state.desired_commit()?.map(Build::snapshot))
    }

    /// The tag naming this commit, if any. Only used when we already know a tag
    /// push queued it — never to *decide* whether something is a release.
    fn tag_for(&self, commit: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["describe", "--exact-match", "--tags", commit])
            .env("GIT_DIR", &self.config.git_repo)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let tag = String::from_utf8(out.stdout).ok()?.trim().to_string();
        if tag.is_empty() { None } else { Some(tag) }
    }

    /// A build is finished once it can make no further progress: its source
    /// failed (nothing to publish), or publish reached a terminal state.
    fn is_build_finished(&self, commit: &str) -> bool {
        if self.state.read_status(commit, "source") == JobStatus::Failed {
            return true;
        }
        matches!(
            self.effective_status(commit, "publish"),
            JobStatus::Done | JobStatus::Failed
        )
    }

    /// One iteration of the reconcile loop
    fn reconcile(&mut self) -> Result<()> {
        // Reap finished children first
        self.reap_children()?;

        let build = match self.pick_build()? {
            Some(b) => b,
            None => return Ok(()),
        };
        let commit = build.id.clone();

        // Log when the desired build changes
        if self.last_desired.as_deref() != Some(&commit) {
            if build.is_release() {
                info!("[{}] new release build for {}", build.short(), short(&build.commit));
            } else {
                info!("[{}] new desired commit", build.short());
            }
            self.last_desired = Some(commit.clone());
        }

        let matrix = build_matrix();
        self.materialize_jobs(&commit, &matrix)?;

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
                info!("[{}] starting source package build", build.short());
                self.spawn_source_build(&build)?;
                return Ok(());
            }
        }

        // Phase 2: binary builds
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
                }
                JobStatus::Pending => {
                    still_running = true;
                    if self.have_build_slot(job) {
                        info!("[{}] starting binary build: {}", build.short(), name);
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
                    info!("[{}] some builds failed, publishing successful ones", build.short());
                } else {
                    info!("[{}] all builds complete, publishing", build.short());
                }
                self.spawn_publish(&build)?;
            }
        }

        Ok(())
    }

    fn materialize_jobs(&self, commit: &str, matrix: &[Job]) -> Result<()> {
        let mut changed = false;

        changed |= self.state.ensure_status(commit, "source", JobStatus::Pending)?;
        for job in matrix {
            changed |= self.state.ensure_status(commit, &job.name(), JobStatus::Pending)?;
        }
        changed |= self.state.ensure_status(commit, "publish", JobStatus::Pending)?;

        if changed {
            self.state.regenerate_html();
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
                      short(commit), job_name);
                let _ = self.state.write_status(commit, job_name, JobStatus::Failed);
                return JobStatus::Failed;
            }
        }
        status
    }

    /// Reap finished child processes, kill timed-out builds, update state
    fn reap_children(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.running.len() {
            // Kill builds that have exceeded the timeout
            if self.running[i].started_at.elapsed() > self.config.build_timeout {
                let job = self.running.remove(i);
                error!("[{}] {} timed out after {:?}, killing",
                       short(&job.commit), job.job_name,
                       job.started_at.elapsed());
                let mut child = job.child;
                let _ = child.kill();
                let _ = child.wait();
                self.state.write_status(&job.commit, &job.job_name, JobStatus::Failed)?;
                let _ = fs::remove_file(self.state.pid_path(&job.commit, &job.job_name));
                continue;
            }

            match self.running[i].child.try_wait() {
                Ok(Some(exit_status)) => {
                    let job = self.running.remove(i);
                    let status = if exit_status.success() {
                        JobStatus::Done
                    } else {
                        JobStatus::Failed
                    };
                    if status == JobStatus::Failed {
                        let tail = Self::log_tail(&self.state.job_dir(&job.commit, &job.job_name));
                        warn!("[{}] {} failed: {}", short(&job.commit), job.job_name, tail);
                    } else {
                        info!("[{}] {} finished: {:?}", short(&job.commit), job.job_name, status);
                    }
                    self.state.write_status(&job.commit, &job.job_name, status)?;
                    let _ = fs::remove_file(self.state.pid_path(&job.commit, &job.job_name));
                }
                Ok(None) => {
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

    fn log_tail(job_dir: &Path) -> String {
        // Find the most recent log file in the job directory
        let latest = fs::read_dir(job_dir).ok()
            .and_then(|entries| {
                entries.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_str()
                        .map_or(false, |n| n.starts_with("log-")))
                    .max_by_key(|e| e.file_name())
            });
        let Some(entry) = latest else { return "(no log)".into() };
        let content = fs::read_to_string(entry.path()).unwrap_or_default();
        content.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("(empty log)")
            .to_string()
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

    fn spawn_source_build(&mut self, build: &Build) -> Result<()> {
        let id = &build.id;
        self.state.write_status(id, "source", JobStatus::Building)?;
        let log_file = fs::File::create(self.state.log_path(id, "source"))
            .context("creating source build log")?;

        // The tag is an *input*, passed explicitly. build-source.sh no longer
        // asks git whether this is a release — that question, asked at build
        // time, is what produced a snapshot version for v1.39.1. An empty
        // argument means snapshot, and it means it definitively.
        let tag = build.tag.clone().unwrap_or_default();

        let child = Command::new(&self.config.scripts_dir.join("build-source.sh"))
            .arg(&build.commit)
            .arg(&self.config.git_repo)
            .arg(self.state.result_dir(id, "source"))
            .arg(&self.config.rust_version)
            .arg(&tag)
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file))
            .spawn()
            .context("spawning source build")?;

        self.write_pid(id, "source", &child)?;
        self.running.push(RunningJob {
            child,
            job_name: "source".into(),
            commit: id.clone(),
            remote: false,
            started_at: Instant::now(),
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
            Command::new(&self.config.scripts_dir.join("build-binary.sh"))
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
            started_at: Instant::now(),
        });
        Ok(())
    }

    fn spawn_publish(&mut self, build: &Build) -> Result<()> {
        let id = &build.id;
        self.state.write_status(id, "publish", JobStatus::Building)?;
        let log_file = fs::File::create(self.state.log_path(id, "publish"))
            .context("creating publish log")?;

        // The suite comes from the build's own identity, fixed when it was
        // created. There is deliberately no `git describe` here: asking git a
        // second time, 40-odd minutes after the version was stamped, is exactly
        // how v1.39.1's snapshot-versioned artifacts reached the release suite.
        let suite = if build.is_release() { "release" } else { "snapshot" };
        info!("[{}] publishing to {} suite", build.short(), suite);

        let child = Command::new(&self.config.scripts_dir.join("publish.sh"))
            .arg(id)
            .arg(suite)
            .env("STATE_DIR", &self.config.state_dir)
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file))
            .spawn()
            .context("spawning publish")?;

        self.write_pid(id, "publish", &child)?;
        self.running.push(RunningJob {
            child,
            job_name: "publish".into(),
            commit: id.clone(),
            remote: false,
            started_at: Instant::now(),
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            writeln!(buf, "{} [{}] {}",
                     Local::now().format("%Y-%m-%d %H:%M:%S"),
                     record.level(),
                     record.args())
        })
        .init();

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The obvious implementation here is `&id[..12]`, and it panics on any id
    /// shorter than 12 bytes — which every release build has, because its id is
    /// a tag. Ten call sites used to slice like that; they were only safe while
    /// every id was a 40-char sha.
    #[test]
    fn short_does_not_panic_on_a_tag_id() {
        assert_eq!(short("v1.39.1"), "v1.39.1");
        assert_eq!(short("9dc9769fe3d4909cc86eb346514c60eb5d471411"), "9dc9769fe3d4");
        assert_eq!(short(""), "");
        assert_eq!(short("123456789012"), "123456789012");
        assert_eq!(short("1234567890123"), "123456789012");
    }

    /// Slicing by byte index would also panic mid-character on a multi-byte id.
    /// Tags are user-supplied strings, so this is reachable, not theoretical.
    #[test]
    fn short_respects_character_boundaries() {
        assert_eq!(short("ααααααααααααα"), "αααααααααααα");
    }

    #[test]
    fn a_release_and_its_commit_are_different_builds() {
        let commit = "9dc9769fe3d4909cc86eb346514c60eb5d471411".to_string();
        let snap = Build::snapshot(commit.clone());
        let rel  = Build::release("v1.39.1".into(), commit.clone());

        // Same tree, different builds — so different state dirs, and neither
        // can be reclassified into the other.
        assert_ne!(snap.id, rel.id);
        assert_eq!(snap.commit, rel.commit);
        assert!(!snap.is_release());
        assert!(rel.is_release());
    }
}
