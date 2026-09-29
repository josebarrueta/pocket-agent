use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, bail, ensure};

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

#[derive(Clone)]
pub struct WorkspaceManager {
    root: PathBuf,
    repositories: BTreeMap<String, PathBuf>,
    limits: WorkspaceLimits,
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
        Ok(reclaimed)
    }
}

pub struct DisposableWorkspace {
    pub job_id: String,
    pub repository: String,
    root: PathBuf,
    control: PathBuf,
    pub path: PathBuf,
    limits: WorkspaceLimits,
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

fn reset_control(control: &Path) -> Result<()> {
    git(control, &["reset", "--hard", "--quiet", "HEAD"], 64 * 1024)?;
    git(control, &["clean", "-dffx", "-q"], 64 * 1024)?;
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

fn git_bytes(directory: &Path, arguments: &[&str], max_bytes: usize) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
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
