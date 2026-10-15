//! `nookctl sandbox init`: mirrors the real desktop into a sandbox (spec §10).
//! The real desktop is only READ. Nothing is deleted or overwritten — a non-empty
//! sandbox is refused.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
use windows::core::HSTRING;

use crate::paths::Desktops;

pub const FULL_COPY_LIMIT: u64 = 20 * 1024 * 1024;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub root: PathBuf,
    pub copied: usize,
    pub stubs: usize,
    pub folders: usize,
    pub bytes: u64,
    pub skipped: Vec<String>,
}

fn is_empty_or_missing(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(true)
}

fn set_attrs(path: &Path, attrs: u32) {
    // Only hidden/system/readonly matter for Nook; applied to sandbox paths only.
    let keep = attrs & (0x1 | 0x2 | 0x4);
    if keep != 0 {
        // SAFETY: plain Win32 call on a sandbox path we just created.
        let _ = unsafe {
            SetFileAttributesW(
                &HSTRING::from(path.as_os_str()),
                FILE_FLAGS_AND_ATTRIBUTES(keep),
            )
        };
    }
}

/// Copies created/modified times from the source onto a sandbox path (file or dir).
fn copy_times(dest: &Path, src_meta: &fs::Metadata) {
    use std::fs::FileTimes;
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let Ok(f) = fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dest)
    else {
        return;
    };
    let mut t = FileTimes::new();
    if let Ok(c) = src_meta.created() {
        t = t.set_created(c);
    }
    if let Ok(m) = src_meta.modified() {
        t = t.set_modified(m);
    }
    let _ = f.set_times(t);
}

/// Creates an empty file (fails if it exists) with the source's timestamps.
fn stub(dest: &Path, src_meta: &fs::Metadata) -> Result<()> {
    drop(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)?,
    );
    copy_times(dest, src_meta);
    Ok(())
}

fn mirror_dir(src: &Path, dest: &Path, rep: &mut Report) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for e in fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
        .flatten()
    {
        let from = e.path();
        let to = dest.join(e.file_name());
        let res = (|| -> Result<()> {
            let meta = fs::symlink_metadata(&from)?;
            use std::os::windows::fs::MetadataExt;
            let attrs = meta.file_attributes();
            if to.exists() {
                bail!("already exists in sandbox");
            }
            if meta.is_dir() {
                // Folder stub: same name, top-level children as empty stubs.
                fs::create_dir(&to)?;
                if let Ok(rd) = fs::read_dir(&from) {
                    for c in rd.flatten() {
                        let ct = to.join(c.file_name());
                        match c.metadata() {
                            Ok(m) if m.is_dir() => {
                                fs::create_dir(&ct)?;
                                copy_times(&ct, &m);
                            }
                            Ok(m) => stub(&ct, &m)?,
                            Err(_) => {}
                        }
                    }
                }
                set_attrs(&to, attrs);
                copy_times(&to, &meta);
                rep.folders += 1;
            } else if meta.len() <= FULL_COPY_LIMIT {
                // CopyFileEx under the hood: keeps timestamps and attributes.
                rep.bytes += fs::copy(&from, &to)?;
                copy_times(&to, &meta);
                rep.copied += 1;
            } else {
                stub(&to, &meta)?;
                set_attrs(&to, attrs);
                rep.stubs += 1;
            }
            Ok(())
        })();
        if let Err(err) = res {
            rep.skipped.push(format!("{}: {err:#}", from.display()));
        }
    }
    Ok(())
}

pub fn init(root: &Path) -> Result<Report> {
    let sb = Desktops::sandbox(root.to_path_buf());
    for d in [&sb.user, &sb.public] {
        if !is_empty_or_missing(d) {
            bail!(
                "{} is not empty; Nook never overwrites. Pick another NOOK_DESKTOP_ROOT.",
                d.display()
            );
        }
    }
    let (live_user, live_public) = Desktops::live()?;
    for d in [&live_user, &live_public] {
        if sb.user.starts_with(d) || d.starts_with(root) {
            bail!(
                "sandbox {} overlaps the real desktop {}",
                root.display(),
                d.display()
            );
        }
    }
    let mut rep = Report {
        root: root.to_path_buf(),
        ..Default::default()
    };
    mirror_dir(&live_user, &sb.user, &mut rep)?;
    mirror_dir(&live_public, &sb.public, &mut rep)?;
    let mut readme = fs::File::create(root.join("NOOK_SANDBOX.txt"))?;
    writeln!(
        readme,
        "Nook sandbox mirror of the real desktop. Safe to experiment on. Created by `nookctl sandbox init`."
    )?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_and_never_overwrites() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("robot_03.png"), b"px").unwrap();
        fs::create_dir(src.path().join("Dev")).unwrap();
        fs::write(src.path().join("Dev").join("main.rs"), b"fn main(){}").unwrap();
        fs::create_dir(src.path().join("Dev").join("target")).unwrap();

        let mut rep = Report::default();
        mirror_dir(src.path(), dst.path(), &mut rep).unwrap();
        assert_eq!((rep.copied, rep.folders, rep.stubs), (1, 1, 0));
        assert_eq!(fs::read(dst.path().join("robot_03.png")).unwrap(), b"px");
        assert_eq!(
            fs::metadata(dst.path().join("Dev").join("main.rs"))
                .unwrap()
                .len(),
            0
        );
        assert!(dst.path().join("Dev").join("target").is_dir());

        // Second run: everything is skipped, the original copy stays intact.
        fs::write(dst.path().join("robot_03.png"), b"changed").unwrap();
        let mut rep2 = Report::default();
        mirror_dir(src.path(), dst.path(), &mut rep2).unwrap();
        assert_eq!(rep2.skipped.len(), 2);
        assert_eq!(
            fs::read(dst.path().join("robot_03.png")).unwrap(),
            b"changed"
        );
    }
}
