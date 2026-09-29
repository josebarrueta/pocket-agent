use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};

use crate::domain::ChangedFile;

const DEFAULT_MAX_FILES: usize = 20_000;
const DEFAULT_MAX_BYTES: u64 = 500 * 1024 * 1024;
const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
const DEFAULT_MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_PATCH_FILES: usize = 200;

#[derive(Clone, Debug)]
pub struct WorkspaceLimits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_file_bytes: u64,
    pub max_patch_bytes: usize,
    pub max_patch_files: usize,
}

impl Default for WorkspaceLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_bytes: DEFAULT_MAX_BYTES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
            max_patch_files: DEFAULT_MAX_PATCH_FILES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePatch {
    pub patch: String,
    pub files: Vec<ChangedFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePatchStatus {
    pub state: String,
    pub patch_id: Option<String>,
    pub files: Vec<ChangedFile>,
    pub bytes: usize,
    pub patch: Option<String>,
    pub applied_at_unix_ms: Option<u128>,
}

#[derive(Clone)]
struct CandidatePatch {
    patch_id: String,
    patch: String,
    files: Vec<ChangedFile>,
    applied_at_unix_ms: Option<u128>,
}

#[derive(Clone)]
pub struct WorkspaceManager {
    root: PathBuf,
    repositories: BTreeMap<String, PathBuf>,
    limits: WorkspaceLimits,
    candidates: Arc<Mutex<BTreeMap<String, CandidatePatch>>>,
    capability_operations: Arc<Mutex<()>>,
}

impl WorkspaceManager {
    pub fn new(
        root: PathBuf,
        repositories: BTreeMap<String, PathBuf>,
        limits: WorkspaceLimits,
    ) -> Result<Self> {
        ensure!(
            !repositories.is_empty(),
            "at least one repository is required"
        );
        ensure!(
            limits.max_files > 0 && limits.max_bytes > 0 && limits.max_file_bytes > 0,
            "workspace limits must be positive"
        );
        ensure!(
            limits.max_patch_bytes > 0 && limits.max_patch_files > 0,
            "patch limits must be positive"
        );
        Ok(Self {
            root,
            repositories,
            limits,
            candidates: Arc::new(Mutex::new(BTreeMap::new())),
            capability_operations: Arc::new(Mutex::new(())),
        })
    }

    pub fn aliases(&self) -> Vec<String> {
        self.repositories.keys().cloned().collect()
    }

    pub fn create(&self, job_id: &str, repository: &str) -> Result<DisposableWorkspace> {
        validate_job_id(job_id)?;
        let configured = self
            .repositories
            .get(repository)
            .ok_or_else(|| anyhow!("Unknown repository alias '{repository}'"))?;
        ensure_real_directory(&self.root, true)?;
        let source = validate_repository(configured)?;
        let files = repository_files(&source, &self.limits)?;
        let job_root = self.root.join(format!("job-{job_id}"));
        fs::create_dir(&job_root)
            .with_context(|| format!("create workspace {}", job_root.display()))?;
        fs::set_permissions(&job_root, fs::Permissions::from_mode(0o700))?;
        let control = job_root.join("control");
        let workspace = job_root.join("workspace");
        let result = (|| {
            fs::create_dir(&control)?;
            fs::create_dir(&workspace)?;
            copy_files(&source, &control, &files, self.limits.max_file_bytes)?;
            copy_files(&source, &workspace, &files, self.limits.max_file_bytes)?;
            initialize_baseline(&control)?;
            let metadata = serde_json::json!({
                "version": 1,
                "jobId": job_id,
                "repositoryAlias": repository,
                "createdAtUnixMs": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis(),
            });
            write_new(
                &job_root.join("workspace.json"),
                format!("{metadata}\n").as_bytes(),
                0o600,
            )?;
            Ok(DisposableWorkspace {
                job_id: job_id.to_owned(),
                repository: repository.to_owned(),
                root: job_root.clone(),
                control,
                path: workspace,
                limits: self.limits.clone(),
                candidates: self.candidates.clone(),
                disposed: false,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&job_root);
        }
        result
    }

    pub fn reclaim_all(&self) -> Result<Vec<String>> {
        ensure_real_directory(&self.root, true)?;
        let mut reclaimed = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("job-") || !entry.file_type()?.is_dir() {
                continue;
            }
            let metadata_path = entry.path().join("workspace.json");
            let Ok(contents) = fs::read_to_string(&metadata_path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
                continue;
            };
            let Some(job_id) = value.get("jobId").and_then(|value| value.as_str()) else {
                continue;
            };
            if value.get("version").and_then(|value| value.as_u64()) != Some(1)
                || name != format!("job-{job_id}")
                || validate_job_id(job_id).is_err()
            {
                continue;
            }
            fs::remove_dir_all(entry.path())?;
            reclaimed.push(job_id.to_owned());
        }
        reclaimed.sort();
        let mut candidates = self.candidates.lock().expect("workspace candidate lock");
        for job_id in &reclaimed {
            candidates.remove(job_id);
        }
        Ok(reclaimed)
    }

    pub fn read_metadata(&self, job_id: &str, repository: &str) -> Result<serde_json::Value> {
        let _operation = self
            .capability_operations
            .lock()
            .expect("workspace operation lock");
        self.active_control(job_id, repository)?;
        Ok(serde_json::json!({
            "jobId": job_id,
            "repositoryAlias": repository,
            "patchLimits": {
                "maxBytes": self.limits.max_patch_bytes,
                "maxFiles": self.limits.max_patch_files,
            }
        }))
    }

    pub fn submit_patch(
        &self,
        job_id: &str,
        repository: &str,
        patch: &str,
    ) -> Result<WorkspacePatchStatus> {
        let _operation = self
            .capability_operations
            .lock()
            .expect("workspace operation lock");
        ensure!(
            !patch.is_empty() && !patch.contains('\0'),
            "Patch must be non-empty UTF-8 text"
        );
        ensure!(
            patch.len() <= self.limits.max_patch_bytes,
            "Patch exceeds {} bytes",
            self.limits.max_patch_bytes
        );
        reject_unsafe_patch_directives(patch)?;
        let control = self.active_control(job_id, repository)?;
        reset_control_to_baseline(&control)?;
        git_with_input(
            &control,
            &["apply", "--check", "--binary", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
            64 * 1024,
        )?;
        git_with_input(
            &control,
            &["apply", "--binary", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
            64 * 1024,
        )?;
        scan_control_tree(&control, &self.limits)?;
        git(&control, &["add", "--all", "--", "."], 64 * 1024)?;
        let manifest = git_bytes(
            &control,
            &[
                "diff",
                "--cached",
                "--name-status",
                "-z",
                "--no-renames",
                "HEAD",
                "--",
                ".",
            ],
            self.limits.max_patch_bytes + 1,
        )?;
        let files = parse_manifest(&manifest)?;
        ensure!(!files.is_empty(), "Patch makes no changes");
        ensure!(
            files.len() <= self.limits.max_patch_files,
            "Patch changes {} files; limit is {}",
            files.len(),
            self.limits.max_patch_files
        );
        let canonical = git_bytes(
            &control,
            &[
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "HEAD",
                "--",
                ".",
            ],
            self.limits.max_patch_bytes + 1,
        )?;
        ensure!(
            canonical.len() <= self.limits.max_patch_bytes,
            "Patch exceeds {} bytes",
            self.limits.max_patch_bytes
        );
        let canonical = String::from_utf8(canonical).context("patch is not UTF-8")?;
        ensure!(
            !canonical.contains("GIT binary patch"),
            "Binary patches are not allowed"
        );
        let patch_id = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        let candidate = CandidatePatch {
            patch_id,
            patch: canonical,
            files,
            applied_at_unix_ms: None,
        };
        let status = candidate_status(&candidate, false);
        self.candidates
            .lock()
            .expect("workspace candidate lock")
            .insert(job_id.to_owned(), candidate);
        Ok(status)
    }

    pub fn patch_status(&self, job_id: &str, repository: &str) -> Result<WorkspacePatchStatus> {
        let _operation = self
            .capability_operations
            .lock()
            .expect("workspace operation lock");
        self.active_control(job_id, repository)?;
        Ok(self
            .candidates
            .lock()
            .expect("workspace candidate lock")
            .get(job_id)
            .map_or_else(empty_candidate_status, |candidate| {
                candidate_status(candidate, true)
            }))
    }

    pub fn apply_patch(
        &self,
        job_id: &str,
        repository: &str,
        patch_id: &str,
    ) -> Result<WorkspacePatchStatus> {
        let _operation = self
            .capability_operations
            .lock()
            .expect("workspace operation lock");
        self.active_control(job_id, repository)?;
        let candidate = self
            .candidates
            .lock()
            .expect("workspace candidate lock")
            .get(job_id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown or stale patch ID"))?;
        ensure!(candidate.patch_id == patch_id, "Unknown or stale patch ID");
        if candidate.applied_at_unix_ms.is_some() {
            return Ok(candidate_status(&candidate, false));
        }
        let configured = self
            .repositories
            .get(repository)
            .ok_or_else(|| anyhow!("Unknown repository alias"))?;
        let source = validate_repository(configured)?;
        ensure!(
            source == configured.canonicalize()?,
            "Configured repository identity changed"
        );
        repository_files(&source, &self.limits)?;
        assert_safe_target_paths(&source, &candidate.files)?;
        git_with_input(
            &source,
            &["apply", "--check", "--binary", "--whitespace=nowarn", "-"],
            candidate.patch.as_bytes(),
            64 * 1024,
        )?;
        git_with_input(
            &source,
            &["apply", "--binary", "--whitespace=nowarn", "-"],
            candidate.patch.as_bytes(),
            64 * 1024,
        )?;
        let mut candidates = self.candidates.lock().expect("workspace candidate lock");
        let stored = candidates
            .get_mut(job_id)
            .ok_or_else(|| anyhow!("Unknown or stale patch ID"))?;
        ensure!(
            stored.patch_id == patch_id && stored.applied_at_unix_ms.is_none(),
            "Unknown or stale patch ID"
        );
        stored.applied_at_unix_ms = Some(now_unix_ms()?);
        Ok(candidate_status(stored, false))
    }

    fn active_control(&self, job_id: &str, repository: &str) -> Result<PathBuf> {
        validate_job_id(job_id)?;
        ensure!(
            self.repositories.contains_key(repository),
            "Unknown repository alias"
        );
        let job_root = self.root.join(format!("job-{job_id}"));
        let metadata = fs::symlink_metadata(&job_root)?;
        ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "No active workspace matches this capability lease"
        );
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(job_root.join("workspace.json"))?)?;
        ensure!(
            value["version"] == 1
                && value["jobId"] == job_id
                && value["repositoryAlias"] == repository,
            "No active workspace matches this capability lease"
        );
        Ok(job_root.join("control"))
    }
}

pub struct DisposableWorkspace {
    pub job_id: String,
    pub repository: String,
    root: PathBuf,
    control: PathBuf,
    pub path: PathBuf,
    limits: WorkspaceLimits,
    candidates: Arc<Mutex<BTreeMap<String, CandidatePatch>>>,
    disposed: bool,
}

impl DisposableWorkspace {
    pub fn export_patch(&mut self) -> Result<WorkspacePatch> {
        ensure!(!self.disposed, "Workspace is disposed");
        let files = scan_tree(&self.path, &self.limits)?;
        reset_control(&self.control)?;
        copy_files(
            &self.path,
            &self.control,
            &files,
            self.limits.max_file_bytes,
        )?;
        git(&self.control, &["add", "--all", "--", "."], 64 * 1024)?;
        let manifest = git_bytes(
            &self.control,
            &[
                "diff",
                "--cached",
                "--name-status",
                "-z",
                "--no-renames",
                "HEAD",
                "--",
                ".",
            ],
            self.limits.max_patch_bytes + 1,
        )?;
        let changed = parse_manifest(&manifest)?;
        ensure!(
            changed.len() <= self.limits.max_patch_files,
            "Patch changes {} files; limit is {}",
            changed.len(),
            self.limits.max_patch_files
        );
        let patch = git_bytes(
            &self.control,
            &[
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "HEAD",
                "--",
                ".",
            ],
            self.limits.max_patch_bytes + 1,
        )?;
        ensure!(
            patch.len() <= self.limits.max_patch_bytes,
            "Patch exceeds {} bytes",
            self.limits.max_patch_bytes
        );
        let patch = String::from_utf8(patch).context("patch is not UTF-8")?;
        Ok(WorkspacePatch {
            patch,
            files: changed,
        })
    }

    pub fn dispose(&mut self) -> Result<()> {
        if self.disposed {
            return Ok(());
        }
        self.disposed = true;
        self.candidates
            .lock()
            .expect("workspace candidate lock")
            .remove(&self.job_id);
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        Ok(())
    }
}

impl Drop for DisposableWorkspace {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

fn empty_candidate_status() -> WorkspacePatchStatus {
    WorkspacePatchStatus {
        state: "none".into(),
        patch_id: None,
        files: Vec::new(),
        bytes: 0,
        patch: None,
        applied_at_unix_ms: None,
    }
}

fn candidate_status(candidate: &CandidatePatch, include_patch: bool) -> WorkspacePatchStatus {
    WorkspacePatchStatus {
        state: if candidate.applied_at_unix_ms.is_some() {
            "applied"
        } else {
            "submitted"
        }
        .into(),
        patch_id: Some(candidate.patch_id.clone()),
        files: candidate.files.clone(),
        bytes: candidate.patch.len(),
        patch: include_patch.then(|| candidate.patch.clone()),
        applied_at_unix_ms: candidate.applied_at_unix_ms,
    }
}

fn now_unix_ms() -> Result<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis())
}

#[derive(Clone)]
struct ScannedFile {
    path: PathBuf,
    mode: u32,
    size: u64,
}

fn validate_job_id(job_id: &str) -> Result<()> {
    ensure!(
        !job_id.is_empty()
            && job_id.len() <= 128
            && job_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "Invalid job ID"
    );
    Ok(())
}

fn validate_repository(configured: &Path) -> Result<PathBuf> {
    let source = configured
        .canonicalize()
        .with_context(|| format!("resolve repository {}", configured.display()))?;
    ensure_real_directory(&source, false)?;
    let top = git_bytes(&source, &["rev-parse", "--show-toplevel"], 16 * 1024)?;
    let top = PathBuf::from(String::from_utf8(top)?.trim());
    ensure!(
        top.canonicalize()? == source,
        "Configured repository must be a Git worktree root"
    );
    Ok(source)
}

fn repository_files(source: &Path, limits: &WorkspaceLimits) -> Result<Vec<ScannedFile>> {
    let staged = git_bytes(
        source,
        &["ls-files", "--stage", "-z"],
        limits.max_files.saturating_mul(512).max(64 * 1024),
    )?;
    for entry in staged
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let mode = entry.split(|byte| *byte == b' ').next().unwrap_or_default();
        ensure!(
            mode != b"120000",
            "Symbolic links are not allowed in repository snapshots"
        );
        ensure!(
            mode != b"160000",
            "Git submodules are not allowed in repository snapshots"
        );
    }
    let listed = git_bytes(
        source,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        limits.max_files.saturating_mul(4096).max(64 * 1024),
    )?;
    let paths = listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8(entry.to_vec()).context("repository path is not UTF-8"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        paths.len() <= limits.max_files,
        "Snapshot exceeds {} files",
        limits.max_files
    );
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let relative = validate_relative(Path::new(&path))?;
        ensure!(seen.insert(relative.clone()), "Duplicate repository path");
        let metadata = fs::symlink_metadata(source.join(&relative))?;
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "Only regular files are allowed in repository snapshots"
        );
        ensure!(
            metadata.nlink() == 1,
            "Hard-linked files are not allowed in repository snapshots"
        );
        ensure!(
            metadata.len() <= limits.max_file_bytes,
            "File {} exceeds snapshot limit",
            relative.display()
        );
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| anyhow!("Snapshot size overflow"))?;
        ensure!(
            total <= limits.max_bytes,
            "Snapshot exceeds {} bytes",
            limits.max_bytes
        );
        files.push(ScannedFile {
            path: relative,
            mode: metadata.mode() & 0o777,
            size: metadata.len(),
        });
    }
    Ok(files)
}

fn scan_tree(root: &Path, limits: &WorkspaceLimits) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();
    scan_directory(root, root, limits, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    ensure!(
        files.len() <= limits.max_files,
        "Workspace exceeds {} files",
        limits.max_files
    );
    let total = files.iter().try_fold(0u64, |sum, file| {
        sum.checked_add(file.size)
            .ok_or_else(|| anyhow!("Workspace size overflow"))
    })?;
    ensure!(
        total <= limits.max_bytes,
        "Workspace exceeds {} bytes",
        limits.max_bytes
    );
    Ok(files)
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    limits: &WorkspaceLimits,
    files: &mut Vec<ScannedFile>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?.to_owned();
        validate_relative(&relative)?;
        if relative
            .components()
            .next()
            .is_some_and(|part| part.as_os_str() == ".git")
        {
            bail!("Worker workspace cannot contain .git metadata");
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() {
            ensure!(metadata.nlink() >= 1, "Invalid directory");
            scan_directory(root, &path, limits, files)?;
        } else {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "Workspace contains a link or special file: {}",
                relative.display()
            );
            ensure!(
                metadata.nlink() == 1,
                "Workspace contains a hard-linked file: {}",
                relative.display()
            );
            ensure!(
                metadata.len() <= limits.max_file_bytes,
                "File {} exceeds workspace limit",
                relative.display()
            );
            files.push(ScannedFile {
                path: relative,
                mode: metadata.mode() & 0o777,
                size: metadata.len(),
            });
            ensure!(
                files.len() <= limits.max_files,
                "Workspace exceeds {} files",
                limits.max_files
            );
        }
    }
    Ok(())
}

fn scan_control_tree(root: &Path, limits: &WorkspaceLimits) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Workspace contains a link"
        );
        if metadata.file_type().is_dir() {
            scan_directory(root, &entry.path(), limits, &mut files)?;
        } else {
            ensure!(
                metadata.file_type().is_file() && metadata.nlink() == 1,
                "Workspace contains a linked or special file"
            );
            ensure!(
                metadata.len() <= limits.max_file_bytes,
                "Workspace file exceeds size limit"
            );
            files.push(ScannedFile {
                path: PathBuf::from(entry.file_name()),
                mode: metadata.mode() & 0o777,
                size: metadata.len(),
            });
        }
    }
    ensure!(
        files.len() <= limits.max_files,
        "Workspace exceeds {} files",
        limits.max_files
    );
    let bytes = files.iter().try_fold(0u64, |sum, file| {
        sum.checked_add(file.size)
            .ok_or_else(|| anyhow!("Workspace size overflow"))
    })?;
    ensure!(
        bytes <= limits.max_bytes,
        "Workspace exceeds {} bytes",
        limits.max_bytes
    );
    Ok(files)
}

fn assert_safe_target_paths(root: &Path, files: &[ChangedFile]) -> Result<()> {
    for file in files {
        let relative = validate_relative(Path::new(&file.path))?;
        let mut current = root.to_owned();
        let count = relative.components().count();
        for (index, component) in relative.components().enumerate() {
            current.push(component);
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            };
            ensure!(
                !metadata.file_type().is_symlink(),
                "Symbolic links are not allowed in patch targets: {}",
                file.path
            );
            if index + 1 < count {
                ensure!(
                    metadata.file_type().is_dir(),
                    "Patch target parent is not a directory: {}",
                    file.path
                );
            } else if !metadata.file_type().is_dir() {
                ensure!(
                    metadata.file_type().is_file() && metadata.nlink() == 1,
                    "Patch target is not a regular file: {}",
                    file.path
                );
            }
        }
    }
    Ok(())
}

fn reject_unsafe_patch_directives(patch: &str) -> Result<()> {
    for line in patch.lines() {
        ensure!(
            !(line.starts_with("rename from ")
                || line.starts_with("rename to ")
                || line.starts_with("copy from ")
                || line.starts_with("copy to ")
                || line.starts_with("similarity index ")),
            "Patch renames and copies are not supported"
        );
        ensure!(
            !matches!(
                line,
                "new file mode 160000" | "old mode 160000" | "deleted file mode 160000"
            ),
            "Patch submodules are not supported"
        );
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty() && !path.is_absolute(),
        "Invalid workspace path"
    );
    for component in path.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Invalid workspace path {}",
            path.display()
        );
        let name = component.as_os_str();
        ensure!(
            name != ".git" && name != ".gitmodules",
            "Repository metadata path is not allowed: {}",
            path.display()
        );
    }
    Ok(path.to_owned())
}

fn copy_files(
    source: &Path,
    destination: &Path,
    files: &[ScannedFile],
    max_file_bytes: u64,
) -> Result<()> {
    for file in files {
        let from = source.join(&file.path);
        let to = destination.join(&file.path);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&from)
            .with_context(|| format!("open {}", from.display()))?;
        let metadata = input.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.len() == file.size
                && metadata.len() <= max_file_bytes,
            "Source file changed during snapshot: {}",
            file.path.display()
        );
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(file.mode)
            .open(&to)?;
        let copied = std::io::copy(
            &mut std::io::Read::by_ref(&mut input).take(max_file_bytes + 1),
            &mut output,
        )?;
        ensure!(
            copied == file.size && copied <= max_file_bytes,
            "Source file changed during snapshot: {}",
            file.path.display()
        );
        output.flush()?;
        fs::set_permissions(&to, fs::Permissions::from_mode(file.mode))?;
    }
    Ok(())
}

fn reset_control_to_baseline(control: &Path) -> Result<()> {
    git(control, &["reset", "--hard", "--quiet", "HEAD"], 64 * 1024)?;
    git(control, &["clean", "-dffx", "-q"], 64 * 1024)?;
    Ok(())
}

fn reset_control(control: &Path) -> Result<()> {
    reset_control_to_baseline(control)?;
    for entry in fs::read_dir(control)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn initialize_baseline(control: &Path) -> Result<()> {
    git(control, &["init", "--quiet"], 64 * 1024)?;
    git(control, &["config", "user.name", "Pocket Agent"], 64 * 1024)?;
    git(
        control,
        &["config", "user.email", "pocket-agent@localhost"],
        64 * 1024,
    )?;
    git(control, &["add", "--all", "--", "."], 64 * 1024)?;
    git(
        control,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "workspace baseline",
        ],
        64 * 1024,
    )?;
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> Result<Vec<ChangedFile>> {
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    ensure!(fields.len() % 2 == 0, "Malformed Git change manifest");
    fields
        .chunks(2)
        .map(|pair| {
            let status = match pair[0] {
                b"A" => "added",
                b"M" => "modified",
                b"D" => "deleted",
                _ => bail!("Unsupported Git change status"),
            };
            let path = String::from_utf8(pair[1].to_vec()).context("changed path is not UTF-8")?;
            validate_relative(Path::new(&path))?;
            Ok(ChangedFile {
                path,
                status: status.to_owned(),
            })
        })
        .collect()
}

fn git(directory: &Path, arguments: &[&str], max_bytes: usize) -> Result<()> {
    git_bytes(directory, arguments, max_bytes).map(|_| ())
}

fn git_with_input(
    directory: &Path,
    arguments: &[&str],
    input: &[u8],
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut child = git_command(directory, arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || drain_bounded(stderr_pipe, 64 * 1024));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut stdout)?;
    if stdout.len() > max_bytes {
        let _ = child.kill();
    }
    let status = child.wait()?;
    writer
        .join()
        .map_err(|_| anyhow!("Git stdin writer failed"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("Git stderr reader failed"))?;
    ensure!(
        stdout.len() <= max_bytes,
        "Git output exceeds {max_bytes} bytes"
    );
    if !status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(stdout)
}

fn git_command(directory: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "credential.helper=",
        ])
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_bytes(directory: &Path, arguments: &[&str], max_bytes: usize) -> Result<Vec<u8>> {
    let mut child = git_command(directory, arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || drain_bounded(stderr_pipe, 64 * 1024));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut stdout)?;
    if stdout.len() > max_bytes {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("Git stderr reader failed"))?;
    ensure!(
        stdout.len() <= max_bytes,
        "Git output exceeds {max_bytes} bytes"
    );
    if !status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(stdout)
}

fn drain_bounded(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(read) = reader.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    kept
}

fn ensure_real_directory(path: &Path, create: bool) -> Result<()> {
    if create {
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "{} must be a real directory",
        path.display()
    );
    if create {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_new(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn fixture() -> (PathBuf, PathBuf, WorkspaceManager) {
        let root = std::env::temp_dir().join(format!(
            "pocket-agent-rust-workspace-{}",
            uuid::Uuid::new_v4()
        ));
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        command(&source, &["init", "--quiet"]);
        command(&source, &["config", "user.name", "Test"]);
        command(&source, &["config", "user.email", "test@example.com"]);
        fs::write(source.join("tracked.txt"), "baseline\n").unwrap();
        command(&source, &["add", "."]);
        command(&source, &["commit", "--quiet", "-m", "baseline"]);
        fs::write(source.join("untracked.txt"), "untracked\n").unwrap();
        let manager = WorkspaceManager::new(
            root.join("storage"),
            BTreeMap::from([("app".into(), source.clone())]),
            WorkspaceLimits::default(),
        )
        .unwrap();
        (root, source, manager)
    }

    #[test]
    fn creates_disposable_snapshot_and_exports_patch() {
        let (root, source, manager) = fixture();
        let mut workspace = manager.create("job-1", "app").unwrap();
        assert_ne!(workspace.path, source);
        assert_eq!(
            fs::read_to_string(workspace.path.join("untracked.txt")).unwrap(),
            "untracked\n"
        );
        fs::write(workspace.path.join("tracked.txt"), "changed\n").unwrap();
        fs::write(workspace.path.join("added.txt"), "added\n").unwrap();
        let patch = workspace.export_patch().unwrap();
        assert_eq!(
            patch.files,
            vec![
                ChangedFile {
                    path: "added.txt".into(),
                    status: "added".into()
                },
                ChangedFile {
                    path: "tracked.txt".into(),
                    status: "modified".into()
                }
            ]
        );
        assert!(
            patch
                .patch
                .contains("diff --git a/tracked.txt b/tracked.txt")
        );
        assert_eq!(
            fs::read_to_string(source.join("tracked.txt")).unwrap(),
            "baseline\n"
        );
        workspace.dispose().unwrap();
        assert!(!workspace.path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn submits_reviews_and_applies_only_the_bound_candidate() {
        let (root, source, manager) = fixture();
        let mut workspace = manager.create("capability", "app").unwrap();
        let patch = "diff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-baseline\n+approved\n";
        let submitted = manager.submit_patch("capability", "app", patch).unwrap();
        assert_eq!(submitted.state, "submitted");
        assert!(submitted.patch.is_none());
        let status = manager.patch_status("capability", "app").unwrap();
        assert!(status.patch.as_deref().unwrap().contains("diff --git"));
        assert!(manager.apply_patch("capability", "app", "0").is_err());
        let applied = manager
            .apply_patch("capability", "app", submitted.patch_id.as_deref().unwrap())
            .unwrap();
        assert_eq!(applied.state, "applied");
        assert_eq!(
            fs::read_to_string(source.join("tracked.txt")).unwrap(),
            "approved\n"
        );
        workspace.dispose().unwrap();
        assert!(manager.patch_status("capability", "app").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unknown_repositories_links_hardlinks_and_submodules() {
        let (root, source, manager) = fixture();
        assert!(manager.create("job", "other").is_err());
        std::os::unix::fs::symlink("tracked.txt", source.join("link")).unwrap();
        command(&source, &["add", "link"]);
        assert!(manager.create("symlink", "app").is_err());
        command(&source, &["reset", "--hard", "--quiet", "HEAD"]);
        let _ = fs::remove_file(source.join("link"));
        fs::hard_link(source.join("tracked.txt"), source.join("hard")).unwrap();
        assert!(manager.create("hardlink", "app").is_err());
        fs::remove_file(source.join("hard")).unwrap();
        fs::write(source.join(".gitmodules"), "[submodule \"unsafe\"]\n").unwrap();
        assert!(manager.create("submodule", "app").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_worker_links_and_reclaims_recognized_workspaces() {
        let (root, _, manager) = fixture();
        let mut workspace = manager.create("stale", "app").unwrap();
        std::os::unix::fs::symlink("/tmp", workspace.path.join("escape")).unwrap();
        assert!(workspace.export_patch().is_err());
        std::mem::forget(workspace);
        assert_eq!(manager.reclaim_all().unwrap(), vec!["stale"]);
        let _ = fs::remove_dir_all(root);
    }
}
