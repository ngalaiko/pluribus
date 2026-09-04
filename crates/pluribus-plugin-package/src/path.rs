use crate::PackageError;
use std::fs::{self, File};
use std::io::{Read, Take};
use std::path::{Component, Path};

pub(crate) fn validate_package_root(root: &Path) -> Result<(), PackageError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| PackageError::new(format!("cannot inspect package root: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PackageError::new(
            "package root must be a directory, not a symlink",
        ));
    }
    Ok(())
}

pub(crate) fn read_package_file(
    root: &Path,
    relative: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, PackageError> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(PackageError::new(format!(
                "unsafe package path: {}",
                relative.display()
            )));
        };
        current.push(segment);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            PackageError::new(format!("cannot inspect {}: {error}", relative.display()))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::new(format!(
                "package path contains a symlink: {}",
                relative.display()
            )));
        }
    }

    let metadata = fs::metadata(&current).map_err(|error| {
        PackageError::new(format!("cannot inspect {}: {error}", relative.display()))
    })?;
    if !metadata.is_file() {
        return Err(PackageError::new(format!(
            "package path is not a regular file: {}",
            relative.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(too_large(relative, max_bytes));
    }

    let file = File::open(&current).map_err(|error| {
        PackageError::new(format!("cannot open {}: {error}", relative.display()))
    })?;
    read_bounded(file.take(max_bytes + 1), relative, max_bytes)
}

pub(crate) fn validate_relative_path(path: &Path) -> Result<(), PackageError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(PackageError::new(format!(
            "unsafe package path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn read_bounded(
    mut reader: Take<File>,
    relative: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, PackageError> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|error| {
        PackageError::new(format!("cannot read {}: {error}", relative.display()))
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        Err(too_large(relative, max_bytes))
    } else {
        Ok(bytes)
    }
}

fn too_large(path: &Path, max_bytes: u64) -> PackageError {
    PackageError::new(format!(
        "package file exceeds {max_bytes} bytes: {}",
        path.display()
    ))
}
