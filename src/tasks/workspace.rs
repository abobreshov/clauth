//! Bounded workspace observations. Two equal captures establish observed
//! stability only; they are not a writer fence or authorization to transfer.

use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_ENTRIES: u64 = 10_000;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_SCOPE: &str = "git_tracked_and_nonignored_untracked";
const DIRECTORY_SCOPE: &str = "directory_entries";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceSnapshot {
    pub(crate) schema: u32,
    pub(crate) scope: String,
    pub(crate) root: PathBuf,
    pub(crate) digest: String,
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) head: Option<String>,
    pub(crate) exclusions: Vec<String>,
}

fn exclusions(git: bool) -> Vec<String> {
    let mut result =
        vec!["Symlink referent contents are excluded; link target text is hashed.".into()];
    if git {
        result.push(
            "Git-ignored untracked entries are excluded; global ignore configuration is disabled."
                .into(),
        );
        result.push("Git metadata other than HEAD identity and index entries is excluded.".into());
    }
    result
}

/// Validate only persisted metadata. This never scans a workspace or invokes Git.
pub(crate) fn validate(snapshot: &WorkspaceSnapshot) -> Result<()> {
    let git = match snapshot.scope.as_str() {
        GIT_SCOPE => true,
        DIRECTORY_SCOPE => false,
        _ => bail!("workspace snapshot has an unsupported scope"),
    };
    if snapshot.schema != 1
        || !snapshot.root.is_absolute()
        || snapshot
            .root
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || snapshot.digest.len() != 64
        || !snapshot.digest.bytes().all(lower_hex)
        || snapshot.files > MAX_ENTRIES
        || snapshot.bytes > MAX_BYTES
        || snapshot.exclusions != exclusions(git)
    {
        bail!("workspace snapshot metadata is invalid or unsupported");
    }
    match (&snapshot.head, git) {
        (None, false) => {}
        (Some(head), true)
            if head == "unborn"
                || ((head.len() == 40 || head.len() == 64) && head.bytes().all(lower_hex)) => {}
        _ => bail!("workspace snapshot has an invalid HEAD identity"),
    }
    Ok(())
}

fn lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

pub(crate) fn capture(root: &Path) -> Result<WorkspaceSnapshot> {
    let root = root
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("workspace cannot be resolved"))?;
    if !root.is_dir() {
        bail!("workspace must be a directory");
    }
    let first = capture_once(&root, MAX_ENTRIES, MAX_BYTES, MAX_FILE_BYTES)?;
    let second = capture_once(&root, MAX_ENTRIES, MAX_BYTES, MAX_FILE_BYTES)?;
    if first != second {
        bail!("workspace changed during capture; wait for writers to stop and retry");
    }
    validate(&first)?;
    Ok(first)
}

fn is_git_root(root: &Path) -> Result<bool> {
    for (index, parent) in root.ancestors().enumerate() {
        if let Ok(metadata) = fs::symlink_metadata(parent.join(".git")) {
            if index != 0 {
                // Some execution sandboxes place an empty protective .git
                // mount at /tmp. Mere ancestor-name presence is not evidence
                // this directory belongs to a Git worktree.
                if let Some(top) = git_command(parent, &["rev-parse", "--show-toplevel"])? {
                    let top = path_from_bytes(top.strip_suffix(b"\n").unwrap_or(&top))?;
                    if top.canonicalize().ok().as_deref() == Some(parent) {
                        bail!("Git workspace must be the repository root");
                    }
                }
                continue;
            }
            if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
                bail!("Git metadata location is ambiguous");
            }
            return Ok(true);
        }
    }
    if root.join("HEAD").is_file() && root.join("objects").is_dir() && root.join("refs").is_dir() {
        bail!("bare repositories are not supported workspace roots");
    }
    Ok(false)
}

fn capture_once(
    root: &Path,
    entries_limit: u64,
    bytes_limit: u64,
    file_limit: u64,
) -> Result<WorkspaceSnapshot> {
    let git = is_git_root(root)?;
    let mut state = Capture {
        hash: Sha256::new(),
        entries: 0,
        files: 0,
        bytes: 0,
        entries_limit,
        bytes_limit,
        file_limit,
    };
    state.field(b"workspace-v1");
    state.field(root.as_os_str().as_encoded_bytes());
    state.field(if git {
        GIT_SCOPE.as_bytes()
    } else {
        DIRECTORY_SCOPE.as_bytes()
    });
    let head = if git {
        let top = git_required(root, &["rev-parse", "--show-toplevel"])?;
        let top = path_from_bytes(top.strip_suffix(b"\n").unwrap_or(&top))?;
        if top.canonicalize().ok().as_deref() != Some(root) {
            bail!("Git repository root is ambiguous");
        }
        let head = match git_command(root, &["rev-parse", "--verify", "HEAD"])? {
            Some(output) => {
                let value = std::str::from_utf8(output.strip_suffix(b"\n").unwrap_or(&output))
                    .map_err(|_| anyhow::anyhow!("Git returned an invalid HEAD identity"))?;
                if !matches!(value.len(), 40 | 64) || !value.bytes().all(lower_hex) {
                    bail!("Git returned an invalid HEAD identity");
                }
                value.to_owned()
            }
            None => {
                // A missing HEAD is only an unborn branch if symbolic-ref
                // succeeds and its referenced branch does not exist.
                let reference = git_required(root, &["symbolic-ref", "-q", "HEAD"])?;
                let reference =
                    std::str::from_utf8(reference.strip_suffix(b"\n").unwrap_or(&reference))
                        .map_err(|_| anyhow::anyhow!("Git returned an invalid unborn HEAD"))?;
                if !reference.starts_with("refs/heads/") || reference.contains('\0') {
                    bail!("Git returned an invalid unborn HEAD");
                }
                if git_command(root, &["show-ref", "--verify", reference])?.is_some() {
                    bail!("Git HEAD cannot be resolved");
                }
                state.field(reference.as_bytes());
                "unborn".to_owned()
            }
        };
        state.field(head.as_bytes());
        // Switching branches at the same commit still changes where the next
        // commit would land. Bind the checkpoint to that identity as well.
        match git_command(root, &["symbolic-ref", "-q", "HEAD"])? {
            Some(reference) => state.field(&reference),
            None if head != "unborn" => state.field(b"detached"),
            None => bail!("Git unborn HEAD cannot be resolved"),
        }
        let index = git_required(root, &["ls-files", "--stage", "-z"])?;
        state.field(&index);
        let mut paths = BTreeSet::new();
        for entry in index.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let tab = entry
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| anyhow::anyhow!("Git returned invalid index entries"))?;
            let header = &entry[..tab];
            if header.starts_with(b"160000 ") {
                bail!("submodules and Git links are not supported in workspace snapshots");
            }
            if !(header.starts_with(b"100644 ")
                || header.starts_with(b"100755 ")
                || header.starts_with(b"120000 "))
            {
                bail!("Git index contains an unsupported entry kind");
            }
            paths.insert(path_from_bytes(&entry[tab + 1..])?);
            if paths.len() as u64 > entries_limit {
                bail!("workspace exceeds the entry limit");
            }
        }
        let untracked = git_required(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for entry in untracked
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
        {
            paths.insert(path_from_bytes(entry)?);
            if paths.len() as u64 > entries_limit {
                bail!("workspace exceeds the entry limit");
            }
        }
        for path in paths {
            safe_relative(&path)?;
            check_git_parents(root, &path)?;
            state.entry(root, &path, false, 0)?;
        }
        Some(head)
    } else {
        state.directory(root, Path::new(""), 0)?;
        None
    };
    Ok(WorkspaceSnapshot {
        schema: 1,
        scope: if git { GIT_SCOPE } else { DIRECTORY_SCOPE }.into(),
        root: root.to_path_buf(),
        digest: hex::encode(state.hash.finalize()),
        files: state.files,
        bytes: state.bytes,
        head,
        exclusions: exclusions(git),
    })
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(std::str::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!("workspace path encoding is unsupported")
        })?))
    }
}

fn safe_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("workspace entry has an unsafe relative path");
    }
    Ok(())
}

fn check_git_parents(root: &Path, path: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    let components: Vec<_> = path.components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        if index + 1 < components.len() {
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                _ => bail!("workspace entry crosses a symlink or non-directory parent"),
            }
        }
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.is_dir())
            && fs::symlink_metadata(current.join(".git")).is_ok()
        {
            bail!("nested Git repositories are not supported in workspace snapshots");
        }
    }
    Ok(())
}

struct Capture {
    hash: Sha256,
    entries: u64,
    files: u64,
    bytes: u64,
    entries_limit: u64,
    bytes_limit: u64,
    file_limit: u64,
}

impl Capture {
    fn field(&mut self, bytes: &[u8]) {
        self.hash.update((bytes.len() as u64).to_le_bytes());
        self.hash.update(bytes);
    }

    fn count(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > self.entries_limit {
            bail!("workspace exceeds the entry limit");
        }
        Ok(())
    }

    fn directory(&mut self, root: &Path, relative: &Path, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("workspace exceeds the directory depth limit");
        }
        let mut names = Vec::new();
        let entries = fs::read_dir(root.join(relative))
            .map_err(|_| anyhow::anyhow!("cannot list workspace directory"))?;
        for entry in entries {
            let entry =
                entry.map_err(|_| anyhow::anyhow!("cannot inspect workspace directory entry"))?;
            names.push(entry.file_name());
            if names.len() as u64 + self.entries > self.entries_limit {
                bail!("workspace exceeds the entry limit");
            }
        }
        names.sort();
        for name in names {
            if name == ".git" {
                bail!("nested Git metadata is not supported in directory snapshots");
            }
            self.entry(root, &relative.join(name), true, depth)?;
        }
        Ok(())
    }

    fn entry(&mut self, root: &Path, relative: &Path, recurse: bool, depth: usize) -> Result<()> {
        self.count()?;
        self.field(relative.as_os_str().as_encoded_bytes());
        let path = root.join(relative);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !recurse => {
                self.field(b"missing");
                self.files += 1;
                return Ok(());
            }
            Err(_) => bail!("cannot inspect workspace entry"),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            self.field(&metadata.permissions().mode().to_le_bytes());
        }
        #[cfg(not(unix))]
        self.field(&[u8::from(metadata.permissions().readonly())]);
        if metadata.file_type().is_symlink() {
            self.field(b"symlink");
            let target = fs::read_link(&path)
                .map_err(|_| anyhow::anyhow!("cannot read workspace symlink"))?;
            let bytes = target.as_os_str().as_encoded_bytes();
            self.add_bytes(bytes.len() as u64)?;
            self.field(bytes);
            self.files += 1;
        } else if metadata.is_file() {
            self.field(b"file");
            if metadata.len() > self.file_limit {
                bail!("workspace file exceeds the per-file size limit");
            }
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let file = options
                .open(&path)
                .map_err(|_| anyhow::anyhow!("cannot read workspace file"))?;
            let opened = file
                .metadata()
                .map_err(|_| anyhow::anyhow!("cannot inspect open workspace file"))?;
            if !opened.is_file() || opened.len() > self.file_limit {
                bail!("workspace entry changed or exceeds the file size limit");
            }
            let mut bytes = Vec::new();
            file.take(self.file_limit + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| anyhow::anyhow!("cannot read workspace file"))?;
            if bytes.len() as u64 > self.file_limit {
                bail!("workspace file exceeds the per-file size limit");
            }
            self.add_bytes(bytes.len() as u64)?;
            self.field(&bytes);
            self.files += 1;
        } else if metadata.is_dir() {
            if !recurse
                || path.join(".git").exists()
                || (path.join("HEAD").is_file()
                    && path.join("objects").is_dir()
                    && path.join("refs").is_dir())
            {
                bail!("nested repositories or ambiguous Git directory entries are unsupported");
            }
            self.field(b"directory");
            self.directory(root, relative, depth + 1)?;
        } else {
            bail!("workspace contains an unsupported special file");
        }
        Ok(())
    }

    fn add_bytes(&mut self, amount: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| anyhow::anyhow!("workspace byte count overflow"))?;
        if self.bytes > self.bytes_limit {
            bail!("workspace exceeds the total byte limit");
        }
        Ok(())
    }
}

fn git_required(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_command(root, args)?.ok_or_else(|| anyhow::anyhow!("Git workspace inspection failed"))
}

fn git_command(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let mut command = Command::new("git");
    command.current_dir(root).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.preloadIndex=false",
        ])
        .arg("-c")
        .arg(format!("core.hooksPath={null}"))
        .arg("-c")
        .arg(format!("core.excludesFile={null}"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("cannot start Git workspace inspection"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("cannot read Git inspection output"))?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_COMMAND_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let start = Instant::now();
    let mut output = None;
    let status = loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(Ok(bytes)) if bytes.len() <= MAX_COMMAND_BYTES => output = Some(bytes),
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    bail!("Git inspection output failed or exceeded its size limit");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < COMMAND_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                bail!("Git workspace inspection timed out or failed");
            }
        }
    };
    let _ = reader.join();
    let bytes = match output {
        Some(bytes) => bytes,
        None => receiver
            .recv()
            .map_err(|_| anyhow::anyhow!("Git inspection output failed"))?
            .map_err(|_| anyhow::anyhow!("Git inspection output failed"))?,
    };
    if bytes.len() > MAX_COMMAND_BYTES {
        bail!("Git inspection output exceeded its size limit");
    }
    Ok(status.success().then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::HomeSandbox;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    fn repo(home: &HomeSandbox) -> PathBuf {
        let root = home.home().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked"), "original").unwrap();
        git(&root, &["add", "tracked"]);
        git(&root, &["commit", "-qm", "initial"]);
        root
    }

    #[test]
    fn tracked_untracked_deleted_and_renamed_content_changes_are_observed() {
        let home = HomeSandbox::new();
        let root = repo(&home);
        let initial = capture(&root).unwrap();
        assert_eq!(capture(&root).unwrap(), initial);
        fs::write(root.join("tracked"), "modified").unwrap();
        let edited = capture(&root).unwrap();
        assert_ne!(initial.digest, edited.digest);
        fs::write(root.join("untracked"), "new").unwrap();
        let added = capture(&root).unwrap();
        assert_ne!(edited.digest, added.digest);
        fs::rename(root.join("tracked"), root.join("renamed")).unwrap();
        let renamed = capture(&root).unwrap();
        assert_ne!(added.digest, renamed.digest);
        fs::remove_file(root.join("renamed")).unwrap();
        assert_ne!(renamed.digest, capture(&root).unwrap().digest);
    }

    #[test]
    fn index_only_change_is_observed_without_worktree_change() {
        let home = HomeSandbox::new();
        let root = repo(&home);
        let initial = capture(&root).unwrap();
        git(&root, &["update-index", "--chmod=+x", "tracked"]);
        assert_ne!(initial.digest, capture(&root).unwrap().digest);
        assert_eq!(fs::read(root.join("tracked")).unwrap(), b"original");
    }

    #[test]
    fn branch_switch_at_the_same_commit_changes_the_fingerprint() {
        let home = HomeSandbox::new();
        let root = repo(&home);
        let initial = capture(&root).unwrap();
        git(&root, &["checkout", "-qb", "other-branch"]);
        let other = capture(&root).unwrap();
        assert_eq!(initial.head, other.head);
        assert_ne!(initial.digest, other.digest);
    }

    #[test]
    fn ignored_content_is_explicitly_excluded_and_index_is_not_written() {
        let home = HomeSandbox::new();
        let root = repo(&home);
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        fs::write(root.join("ignored"), "one").unwrap();
        let index = fs::read(root.join(".git/index")).unwrap();
        let first = capture(&root).unwrap();
        fs::write(root.join("ignored"), "two").unwrap();
        assert_eq!(first, capture(&root).unwrap());
        assert!(first.exclusions.iter().any(|x| x.contains("Git-ignored")));
        assert_eq!(index, fs::read(root.join(".git/index")).unwrap());
    }

    #[test]
    fn submodules_nested_repositories_and_nonroot_workspaces_are_rejected() {
        let home = HomeSandbox::new();
        let root = repo(&home);
        let head = capture(&root).unwrap().head.unwrap();
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head},submodule"),
            ],
        );
        assert!(capture(&root).is_err());
        git(&root, &["update-index", "--force-remove", "submodule"]);
        fs::create_dir(root.join("nested")).unwrap();
        git(&root.join("nested"), &["init", "-q"]);
        assert!(capture(&root).is_err());
        let plain = home.home().join("plain");
        fs::create_dir(&plain).unwrap();
        fs::create_dir(plain.join("nested")).unwrap();
        git(&plain.join("nested"), &["init", "-q"]);
        assert!(capture(&plain).is_err());
        assert!(capture(&root.join(".git")).is_err());
    }

    #[test]
    fn directory_capture_unborn_head_and_limits_are_bounded() {
        let home = HomeSandbox::new();
        let root = home.home().join("plain");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("one"), "1234").unwrap();
        let snapshot = capture(&root).unwrap();
        assert_eq!(snapshot.scope, DIRECTORY_SCOPE);
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.bytes, 4);
        assert!(capture_once(&root, 0, 100, 100).is_err());
        assert!(capture_once(&root, 100, 3, 100).is_err());
        assert!(capture_once(&root, 100, 100, 3).is_err());
        git(&root, &["init", "-q"]);
        assert_eq!(capture(&root).unwrap().head.as_deref(), Some("unborn"));
    }

    #[test]
    fn empty_git_named_ancestor_does_not_imply_repository_membership() {
        let home = HomeSandbox::new();
        let parent = home.home().join("protective-parent");
        fs::create_dir_all(parent.join(".git")).unwrap();
        let root = parent.join("plain");
        fs::create_dir(&root).unwrap();
        assert_eq!(capture(&root).unwrap().scope, DIRECTORY_SCOPE);
    }

    #[test]
    fn structural_validation_has_no_disk_dependency_and_rejects_invalid_metadata() {
        let home = HomeSandbox::new();
        let root = repo(&home);
        let snapshot = capture(&root).unwrap();
        let mut absent_root = snapshot.clone();
        absent_root.root = home.home().join("does-not-exist");
        assert!(validate(&absent_root).is_ok());
        for change in [0, 1, 2, 3, 4] {
            let mut invalid = snapshot.clone();
            match change {
                0 => invalid.schema = 2,
                1 => invalid.root = "relative".into(),
                2 => invalid.digest = "invalid".into(),
                3 => invalid.exclusions.clear(),
                _ => invalid.head = None,
            }
            assert!(validate(&invalid).is_err());
        }
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["extra"] = true.into();
        assert!(serde_json::from_value::<WorkspaceSnapshot>(value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn executable_mode_and_symlink_target_are_hashed_without_following() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = HomeSandbox::new();
        let root = repo(&home);
        let before = capture(&root).unwrap();
        fs::set_permissions(root.join("tracked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(before.digest, capture(&root).unwrap().digest);
        let outside = home.home().join("outside");
        fs::write(&outside, "one").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let link = capture(&root).unwrap();
        fs::write(&outside, "two").unwrap();
        assert_eq!(link, capture(&root).unwrap());
        fs::remove_file(root.join("link")).unwrap();
        symlink("missing-target", root.join("link")).unwrap();
        assert_ne!(link.digest, capture(&root).unwrap().digest);
    }
}
