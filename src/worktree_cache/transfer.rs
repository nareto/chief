use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to inspect existing path before cache hydration {}",
                path.display()
            )
        }),
    }
}

pub(super) fn remove_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed reading {}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed removing directory {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("failed removing {}", path.display()))?;
    }
    Ok(())
}

pub(super) fn copy_path_recursive(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed reading {}", source.display()))?;

    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating directory {}", parent.display()))?;
        }
        let target = fs::read_link(source)
            .with_context(|| format!("failed reading symlink {}", source.display()))?;
        create_raw_symlink(&target, destination)
            .with_context(|| format!("failed creating symlink {}", destination.display()))?;
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed creating directory {}", destination.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("failed reading directory {}", source.display()))?
        {
            let entry = entry.with_context(|| {
                format!("failed reading directory entry under {}", source.display())
            })?;
            let nested_source = entry.path();
            let nested_destination = destination.join(entry.file_name());
            copy_path_recursive(&nested_source, &nested_destination)?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating directory {}", parent.display()))?;
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed copying {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

pub(super) fn copy_path_with_tar_fallback(source: &Path, destination: &Path) -> Result<()> {
    if let Err(_err) = copy_path_with_tar(source, destination) {
        copy_path_recursive(source, destination)?;
    }
    Ok(())
}

fn copy_path_with_tar(source: &Path, destination: &Path) -> Result<()> {
    let source_parent = source.parent().ok_or_else(|| {
        anyhow!(
            "cannot copy {} with tar pipe because parent directory is missing",
            source.display()
        )
    })?;
    let source_name = source.file_name().ok_or_else(|| {
        anyhow!(
            "cannot copy {} with tar pipe because file name is missing",
            source.display()
        )
    })?;
    let destination_parent = destination.parent().ok_or_else(|| {
        anyhow!(
            "cannot copy {} with tar pipe because destination parent is missing",
            destination.display()
        )
    })?;

    fs::create_dir_all(destination_parent).with_context(|| {
        format!(
            "failed creating destination parent directory {}",
            destination_parent.display()
        )
    })?;

    let mut producer = Command::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(source_parent)
        .arg(source_name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed starting tar producer for {}", source.display()))?;
    let producer_stdout = producer
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed attaching tar producer stdout"))?;

    let consumer = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(destination_parent)
        .stdin(Stdio::from(producer_stdout))
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed starting tar consumer for destination {}",
                destination_parent.display()
            )
        })?;

    let consumer_output = consumer
        .wait_with_output()
        .context("failed waiting for tar consumer process")?;
    let producer_output = producer
        .wait_with_output()
        .context("failed waiting for tar producer process")?;

    if !producer_output.status.success() || !consumer_output.status.success() {
        return Err(anyhow!(
            "tar pipe copy failed for {} -> {} (producer_exit={:?}, consumer_exit={:?}, producer_stderr={}, consumer_stderr={})",
            source.display(),
            destination.display(),
            producer_output.status.code(),
            consumer_output.status.code(),
            String::from_utf8_lossy(&producer_output.stderr).trim(),
            String::from_utf8_lossy(&consumer_output.stderr).trim()
        ));
    }

    if !destination.exists() {
        return Err(anyhow!(
            "tar pipe copy reported success but destination is missing: {}",
            destination.display()
        ));
    }
    Ok(())
}

pub(super) fn create_symlink(source: &Path, destination: &Path) -> Result<()> {
    create_raw_symlink(source, destination).with_context(|| {
        format!(
            "failed creating symlink {} -> {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(unix)]
fn create_raw_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, destination)
}

#[cfg(windows)]
fn create_raw_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    }
}
