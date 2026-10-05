//! Runtime resource paths for relocated Linux packages.
use anyhow::{Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    path::Path,
};

pub fn linux_data_dirs(directory: &Path, inherited: Option<&OsStr>) -> Result<OsString> {
    // XDG paths are colon-delimited, including when rendered on another host.
    ensure!(
        !directory.as_os_str().as_encoded_bytes().contains(&b':'),
        "Linux package path cannot contain ':'"
    );
    let mut value = directory.join("usr/share").into_os_string();
    value.push(":");
    value.push(directory.join("share"));
    value.push(":");
    value.push(
        inherited
            .filter(|s| !s.is_empty())
            .unwrap_or(OsStr::new("/usr/local/share:/usr/share")),
    );
    Ok(value)
}
