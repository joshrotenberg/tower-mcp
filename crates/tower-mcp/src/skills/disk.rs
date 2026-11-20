use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};

use super::{SKILL_MANIFEST_FILE, SkillBuilder, SkillError, SkillSet};

#[cfg(windows)]
mod windows;

pub(super) fn load(mut set: SkillSet, root: &Path, prefix: &str) -> Result<SkillSet, SkillError> {
    let prefix = prefix.trim_matches('/');
    let root_dir = Dir::open_ambient_dir(root, ambient_authority())
        .map_err(|source| io_error(root, source))?;
    let files = walk_dir(&root_dir, root)?;

    let manifest_suffix = format!("/{SKILL_MANIFEST_FILE}");
    let skill_dirs: BTreeSet<&str> = files
        .keys()
        .filter_map(|rel| {
            if rel == SKILL_MANIFEST_FILE {
                Some("")
            } else {
                rel.strip_suffix(&manifest_suffix)
            }
        })
        .collect();

    // Resolve the root name only for the public skill path. Descendant I/O
    // uses root_dir regardless of what the caller's path names now.
    let root_segment = if skill_dirs.contains("") {
        let name = root
            .canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .ok_or_else(|| SkillError::InvalidPath {
                path: root.display().to_string(),
                reason: "a skill root needs a directory name to use as its skill path".to_string(),
            })?;
        Some(name)
    } else {
        None
    };

    for dir in skill_dirs {
        let skill_path = join_path(&[prefix, root_segment.as_deref().unwrap_or(""), dir]);
        let mut builder = SkillBuilder::new(skill_path);
        let dir_prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        for (rel, path) in files.range(dir_prefix.clone()..) {
            let Some(within) = rel.strip_prefix(&dir_prefix) else {
                break;
            };
            let bytes = read_file(&root_dir, root, path)?;
            builder = builder.file(within, bytes);
        }
        set = set.skill(builder)?;
    }
    Ok(set)
}

fn join_path(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn io_error(path: &Path, source: io::Error) -> SkillError {
    SkillError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Reopen the indexed path one component at a time, since a no-follow open
/// only covers the last component of its argument.
fn open_dir(root_dir: &Dir, root: &Path, path: &Path) -> Result<Dir, SkillError> {
    let mut dir = root_dir
        .try_clone()
        .map_err(|source| io_error(root, source))?;
    let mut display = root.to_path_buf();
    for component in path.components() {
        let name = component.as_os_str();
        display.push(name);
        dir = open_child_dir(&dir, name).map_err(|source| io_error(&display, source))?;
    }
    Ok(dir)
}

fn open_child_dir(parent: &Dir, name: &OsStr) -> io::Result<Dir> {
    let dir = parent.open_dir_nofollow(name)?;
    #[cfg(windows)]
    windows::check_attributes(&dir.dir_metadata()?)?;
    Ok(dir)
}

fn read_file(root_dir: &Dir, root: &Path, path: &Path) -> Result<Vec<u8>, SkillError> {
    let parent = open_dir(
        root_dir,
        root,
        path.parent().expect("indexed file has a parent"),
    )?;
    read_child_file(&parent, path.file_name().expect("indexed file has a name"))
        .map_err(|source| io_error(&root.join(path), source))
}

fn read_child_file(dir: &Dir, name: &OsStr) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    // On Unix, opening a FIFO in place of a regular file must not wait for a writer.
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = dir.open_with(name, &options)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    windows::check_attributes(&metadata)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

enum EntryKind {
    Dir,
    File,
}

/// Index regular files by protocol path while retaining their original OS paths.
fn walk_dir(root_dir: &Dir, root: &Path) -> Result<BTreeMap<String, PathBuf>, SkillError> {
    let mut files = BTreeMap::new();
    let mut stack = vec![(PathBuf::new(), String::new())];
    while let Some((path, rel)) = stack.pop() {
        let dir = open_dir(root_dir, root, &path)?;
        let display = root.join(&path);
        for (name, kind) in directory_entries(&dir, &display)? {
            let component = Path::new(&name);
            if !matches!(component.components().next(), Some(Component::Normal(n)) if n == name) {
                return Err(io_error(
                    &display,
                    io::Error::new(io::ErrorKind::InvalidData, "invalid directory entry name"),
                ));
            }
            let child_rel = join_path(&[&rel, &name.to_string_lossy()]);
            let child_path = path.join(name);
            match kind {
                EntryKind::Dir => stack.push((child_path, child_rel)),
                EntryKind::File => {
                    files.insert(child_rel, child_path);
                }
            }
        }
    }
    Ok(files)
}

#[cfg(windows)]
use windows::directory_entries;

#[cfg(unix)]
fn directory_entries(dir: &Dir, display: &Path) -> Result<Vec<(OsString, EntryKind)>, SkillError> {
    let mut entries = Vec::new();
    for entry in dir.entries().map_err(|source| io_error(display, source))? {
        let entry = entry.map_err(|source| io_error(display, source))?;
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&display.join(&name), source))?;
        if file_type.is_dir() {
            entries.push((name, EntryKind::Dir));
        } else if file_type.is_file() {
            entries.push((name, EntryKind::File));
        }
    }
    Ok(entries)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn descendant_opens_reject_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        fs::create_dir(path.join("notes")).unwrap();
        fs::write(
            path.join("notes/SKILL.md"),
            "---\nname: notes\ndescription: Notes\n---\n",
        )
        .unwrap();
        symlink("notes", path.join("linked-dir")).unwrap();
        symlink("SKILL.md", path.join("notes/linked-file.md")).unwrap();

        let root = Dir::open_ambient_dir(path, ambient_authority()).unwrap();
        assert!(open_child_dir(&root, OsStr::new("linked-dir")).is_err());
        let notes = open_child_dir(&root, OsStr::new("notes")).unwrap();
        assert!(read_child_file(&notes, OsStr::new("linked-file.md")).is_err());

        let set = SkillSet::from_dir(path).unwrap();
        let skill = set.get("skill://notes/SKILL.md").unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(skill.resources.files().unwrap().len(), 1);

        let selected_root = SkillSet::from_dir(path.join("linked-dir")).unwrap();
        assert!(selected_root.get("skill://notes/SKILL.md").is_some());
    }
}
