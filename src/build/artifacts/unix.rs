use anyhow::{Context, Result, ensure};
use rustix::fs::{Dir, Mode, OFlags, open, openat};
use std::{
    ffi::OsString,
    fs::{File, Metadata},
    io::{self, Read},
    os::unix::ffi::OsStringExt,
    path::{Component, Path, PathBuf},
};

const FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

pub(super) struct Root {
    path: PathBuf,
    directory: File,
}

impl Root {
    pub(super) fn open(path: &Path) -> Result<Self> {
        // The private parent belongs to Haps. Do not canonicalize the final
        // component: a surviving build child could have replaced it with a link.
        let name = path
            .file_name()
            .context("build root must name a directory")?;
        let parent = path.parent().context("build root must have a parent")?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let path = parent.canonicalize()?.join(name);
        let directory = File::from(open(&path, FLAGS | OFlags::DIRECTORY, Mode::empty())?);
        Ok(Self { path, directory })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn open_resolved(&self, path: &Path) -> Result<File> {
        let relative = path
            .strip_prefix(&self.path)
            .context("build artifact escapes the checkout")?;
        let mut current = self.directory.try_clone()?;
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            ensure!(
                matches!(component, Component::Normal(_)),
                "invalid build artifact path"
            );
            let flags = if components.peek().is_some() {
                FLAGS | OFlags::DIRECTORY
            } else {
                FLAGS
            };
            current = File::from(openat(
                &current,
                component.as_os_str(),
                flags,
                Mode::empty(),
            )?);
        }
        Ok(current)
    }
}

pub(super) fn names(directory: &File) -> Result<impl Iterator<Item = Result<OsString>>> {
    Ok(Dir::read_from(directory)?.filter_map(|entry| match entry {
        Ok(entry) => {
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                None
            } else {
                Some(Ok(OsString::from_vec(name.to_vec())))
            }
        }
        Err(error) => Some(Err(error.into())),
    }))
}

pub(super) fn copy_file(input: File, destination: &Path, metadata: &Metadata) -> Result<()> {
    let mut output = File::create_new(destination)?;
    // Read the opened inode, never reopen its pathname. A changing file cannot
    // bypass the package byte budget or keep the parent copying indefinitely.
    let copied = io::copy(
        &mut input.take(metadata.len().saturating_add(1)),
        &mut output,
    )?;
    ensure!(
        copied == metadata.len(),
        "build artifact changed size during collection"
    );
    output.set_permissions(metadata.permissions())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(root.join("nested/file"), "built output").unwrap();
        fs::write(outside.join("nested/file"), "host canary").unwrap();
        (temp, root, outside)
    }

    #[test]
    fn rejects_a_replaced_root() {
        let (temp, root, outside) = fixture();
        fs::rename(&root, temp.path().join("old-root")).unwrap();
        symlink(outside, &root).unwrap();
        assert!(Root::open(&root).is_err());
    }

    #[test]
    fn held_root_survives_path_replacement() {
        let (temp, root, outside) = fixture();
        let directory = Root::open(&root).unwrap();
        let resolved = root.join("nested/file").canonicalize().unwrap();
        fs::rename(&root, temp.path().join("old-root")).unwrap();
        symlink(outside, &root).unwrap();
        let mut contents = String::new();
        directory
            .open_resolved(&resolved)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "built output");
    }

    #[test]
    fn rejects_a_leaf_replaced_after_resolution() {
        let (_temp, root, outside) = fixture();
        let directory = Root::open(&root).unwrap();
        let resolved = root.join("nested/file").canonicalize().unwrap();
        fs::remove_file(&resolved).unwrap();
        symlink(outside.join("nested/file"), &resolved).unwrap();
        assert!(directory.open_resolved(&resolved).is_err());
    }

    #[test]
    fn rejects_an_ancestor_replaced_after_resolution() {
        let (temp, root, outside) = fixture();
        let directory = Root::open(&root).unwrap();
        let resolved = root.join("nested/file").canonicalize().unwrap();
        fs::rename(root.join("nested"), temp.path().join("old-nested")).unwrap();
        symlink(outside.join("nested"), root.join("nested")).unwrap();
        assert!(directory.open_resolved(&resolved).is_err());
    }

    #[test]
    fn opened_file_survives_path_replacement() {
        let (temp, root, outside) = fixture();
        let directory = Root::open(&root).unwrap();
        let resolved = root.join("nested/file").canonicalize().unwrap();
        let input = directory.open_resolved(&resolved).unwrap();
        let metadata = input.metadata().unwrap();
        fs::remove_file(&resolved).unwrap();
        symlink(outside.join("nested/file"), &resolved).unwrap();
        let output = temp.path().join("output");
        copy_file(input, &output, &metadata).unwrap();
        assert_eq!(fs::read_to_string(output).unwrap(), "built output");
    }

    #[test]
    fn enumerates_the_opened_directory() {
        let (temp, root, outside) = fixture();
        let directory = Root::open(&root).unwrap();
        let resolved = root.join("nested").canonicalize().unwrap();
        let input = directory.open_resolved(&resolved).unwrap();
        fs::rename(root.join("nested"), temp.path().join("old-nested")).unwrap();
        fs::write(outside.join("nested/host-only"), "host canary").unwrap();
        symlink(outside.join("nested"), root.join("nested")).unwrap();
        let entries: Vec<_> = names(&input).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(entries, [OsString::from("file")]);
    }

    #[test]
    fn rejects_size_changes_and_never_overwrites_an_output() {
        let (temp, root, _outside) = fixture();
        let source = root.join("nested/file");
        let output = temp.path().join("output");
        let input = File::open(&source).unwrap();
        let metadata = input.metadata().unwrap();
        fs::write(&source, "a much longer changing build output").unwrap();
        assert!(copy_file(input, &output, &metadata).is_err());
        assert_eq!(fs::metadata(&output).unwrap().len(), metadata.len() + 1);
        fs::write(&output, "existing output").unwrap();
        let input = File::open(&source).unwrap();
        let metadata = input.metadata().unwrap();
        assert!(copy_file(input, &output, &metadata).is_err());
        assert_eq!(fs::read_to_string(output).unwrap(), "existing output");
    }
}
