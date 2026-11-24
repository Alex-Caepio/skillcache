use crate::error::{Result, RuntimeError};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub type FileSnapshot = BTreeMap<PathBuf, Option<Vec<u8>>>;

use crate::custody::{native_identity, python_paths, spelling};
pub use python_paths::python_realpath;
pub use python_paths::python_realpath_spelling;
pub use spelling::{canonicalize, plain_windows_path};

pub fn linked(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Conservatively refuse redirecting/unknown reparse authority paths.
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

/// Absolute lexical spelling; missing tail components need not exist.
pub fn absolute(path: &Path) -> Result<PathBuf> {
    let input = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut result = PathBuf::new();
    for part in input.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(part.as_os_str()),
        }
    }
    Ok(result)
}

pub fn canonical_or_missing(path: &Path) -> Result<PathBuf> {
    let mut current = absolute(path)?;
    let mut missing = Vec::new();
    loop {
        match canonicalize(&current) {
            Ok(mut base) => {
                for item in missing.iter().rev() {
                    base.push(item);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = current
                    .file_name()
                    .ok_or_else(|| RuntimeError::unavailable("Cannot resolve filesystem root"))?
                    .to_os_string();
                missing.push(name);
                if !current.pop() {
                    return Err(RuntimeError::unavailable("Cannot resolve filesystem root"));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn admit_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !linked_at(path, &metadata)? => Ok(true),
        Ok(_) => Err(RuntimeError::policy(
            "Authority directory is linked or not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn ensure_directory(path: &Path, private: bool) -> Result<()> {
    if admit_directory(path)? {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_directory(parent, private)?;
    }
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(if private { 0o700 } else { 0o777 });
    }
    match builder.create(path) {
        Ok(()) => {
            #[cfg(unix)]
            if private {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => admit_directory(path)?
            .then_some(())
            .ok_or_else(|| RuntimeError::unavailable("Directory disappeared during creation")),
        Err(error) => Err(error.into()),
    }
}

/// Reject linked authority components from an already admitted anchor down.
pub fn admit_descendant(anchor: &Path, path: &Path) -> Result<()> {
    let suffix = path
        .strip_prefix(anchor)
        .map_err(|_| RuntimeError::policy("Authority path escapes its anchor"))?;
    admit_directory(anchor)?;
    let mut current = anchor.to_path_buf();
    for part in suffix.components() {
        if !matches!(part, Component::Normal(_)) {
            return Err(RuntimeError::policy("Invalid authority path component"));
        }
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if linked_at(&current, &metadata)? => {
                return Err(RuntimeError::policy("Linked authority path is refused"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn regular_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
}

pub fn read_regular(path: &Path) -> Result<Vec<u8>> {
    let observed = fs::symlink_metadata(path)?;
    if linked_at(path, &observed)? || !observed.is_file() {
        return Err(RuntimeError::policy(
            "Source is linked or not a regular file",
        ));
    }
    let before = native_identity::identity(path)?
        .ok_or_else(|| RuntimeError::unavailable("Source disappeared"))?;
    let mut file = regular_options().open(path)?;
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        let opened = native_identity::from_metadata(&metadata);
        if before != opened || !metadata.is_file() {
            return Err(RuntimeError::conflict("Opened source identity changed"));
        }
    }
    #[cfg(not(unix))]
    {
        let opened = native_identity::handle_identity(&file)?;
        if before != opened || !file.metadata()?.is_file() {
            return Err(RuntimeError::conflict("Opened source identity changed"));
        }
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if native_identity::handle_identity(&file)? != before
        || native_identity::identity(path)?.as_ref() != Some(&before)
    {
        return Err(RuntimeError::conflict("Source changed during read"));
    }
    Ok(bytes)
}

pub fn read_text(path: &Path) -> Result<String> {
    let text = String::from_utf8(read_regular(path)?)
        .map_err(|_| RuntimeError::invalid("Source is not UTF-8"))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeError::invalid("Atomic destination has no parent"))?;
    ensure_directory(parent, false)?;
    if let Ok(image) = fs::symlink_metadata(path)
        && (linked_at(path, &image)? || !image.is_file())
    {
        return Err(RuntimeError::policy(
            "Atomic destination is not an ordinary file",
        ));
    }
    let temporary = parent.join(format!(
        ".{}.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("record"),
        Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

pub fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = crate::custody::json::sorted_pretty(value)?.into_bytes();
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

pub fn snapshot(paths: impl IntoIterator<Item = PathBuf>) -> Result<FileSnapshot> {
    let mut result = BTreeMap::new();
    for path in paths {
        let bytes = match fs::symlink_metadata(&path) {
            Ok(_) => Some(read_regular(&path)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        result.insert(path, bytes);
    }
    Ok(result)
}

pub fn restore(images: &FileSnapshot) -> Result<()> {
    let mut failure = None;
    for (path, bytes) in images {
        let result = match bytes {
            Some(bytes) => atomic_write(path, bytes),
            None => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
        };
        if let Err(error) = result
            && failure.is_none()
        {
            failure = Some(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

pub fn append_whole_line(path: &Path, line: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeError::invalid("Journal has no parent"))?;
    ensure_directory(parent, false)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(RuntimeError::policy("Journal is not a regular file"));
    }
    let previous = file.metadata()?.len();
    if let Err(error) = file.write_all(line).and_then(|_| file.sync_all()) {
        let _ = file.set_len(previous);
        return Err(error.into());
    }
    Ok(())
}

pub use crate::custody::native_identity::NativeIdentity;
pub fn linked_at(path: &Path, metadata: &fs::Metadata) -> Result<bool> {
    native_identity::redirecting(path, metadata)
}
pub fn native_image(path: &Path) -> Result<Option<NativeIdentity>> {
    native_identity::identity(path)
}

