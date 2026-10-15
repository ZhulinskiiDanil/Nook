//! `peek(id)`: what's inside an item. Thumbnail ≤512 px via shell, text head ≤4 KB,
//! folder/zip listing ≤50, PE version strings. Read-only.

use std::io::Read;
use std::path::Path;

use serde::Serialize;

use crate::intel::text::{TextEncoding, decode_prefix};
use crate::scan::{Item, Kind};
use crate::shell::fileinfo::{VersionStrings, version_strings};
use crate::shell::thumbs::{encode_png, shell_image};

pub const TEXT_LIMIT: usize = 4096;
pub const LIST_LIMIT: usize = 50;
pub const THUMB_MAX: i32 = 512;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Peek {
    pub id: String,
    pub name: String,
    pub text: Option<String>,
    pub text_encoding: Option<TextEncoding>,
    pub text_truncated: bool,
    pub entries: Option<Vec<Entry>>,
    pub entries_total: Option<usize>,
    pub version: Option<VersionStrings>,
    /// PNG bytes; MCP returns them as image content, CLI saves to a file.
    #[serde(skip)]
    pub thumbnail_png: Option<Vec<u8>>,
    pub thumbnail_size: Option<(u32, u32)>,
    pub errors: Vec<String>,
}

fn list_dir(dir: &Path) -> std::io::Result<(Vec<Entry>, usize)> {
    let mut all: Vec<Entry> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| {
            let m = e.metadata().ok();
            Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: m.as_ref().is_some_and(|m| m.is_dir()),
                size: m.filter(|m| m.is_file()).map_or(0, |m| m.len()),
            }
        })
        .collect();
    all.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let total = all.len();
    all.truncate(LIST_LIMIT);
    Ok((all, total))
}

fn list_zip(path: &Path) -> anyhow::Result<(Vec<Entry>, usize)> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let total = z.len();
    let mut out = Vec::new();
    for i in 0..total.min(LIST_LIMIT) {
        let f = z.by_index_raw(i)?;
        out.push(Entry {
            name: f.name()?.into_owned(),
            is_dir: f.is_dir(),
            size: f.size(),
        });
    }
    Ok((out, total))
}

fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "log"
            | "csv"
            | "json"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "yaml"
            | "yml"
            | "xml"
            | "html"
            | "htm"
            | "css"
            | "js"
            | "ts"
            | "py"
            | "rs"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "cs"
            | "java"
            | "lua"
            | "bat"
            | "cmd"
            | "ps1"
            | "sh"
            | "sql"
            | "srt"
            | "url"
            | "reg"
            | "nfo"
            | "properties"
            | "gitignore"
    )
}

/// Requires COM on the calling thread (thumbnails).
pub fn peek(item: &Item) -> Peek {
    let mut p = Peek {
        id: item.id.clone(),
        name: item.name.clone(),
        ..Default::default()
    };
    if item.kind == Kind::Virtual {
        return p;
    }
    let path = Path::new(&item.path);
    let ext = item.ext.clone().unwrap_or_default();

    if item.kind == Kind::Folder {
        match list_dir(path) {
            Ok((e, t)) => {
                p.entries = Some(e);
                p.entries_total = Some(t);
            }
            Err(e) => p.errors.push(format!("list: {e}")),
        }
    } else if ext == "zip" {
        match list_zip(path) {
            Ok((e, t)) => {
                p.entries = Some(e);
                p.entries_total = Some(t);
            }
            Err(e) => p.errors.push(format!("zip: {e:#}")),
        }
    }

    if matches!(item.kind, Kind::File | Kind::Url) {
        let mut head = Vec::with_capacity(TEXT_LIMIT);
        if let Ok(f) = std::fs::File::open(path) {
            let _ = f.take(TEXT_LIMIT as u64).read_to_end(&mut head);
        }
        // Known text types always; anything else only if it decodes as text (no NULs).
        let known_binary = matches!(ext.as_str(), "exe" | "dll" | "msi" | "zip" | "7z" | "rar");
        if (is_text_ext(&ext) || item.kind == Kind::Url || !known_binary)
            && let Some((t, enc)) = decode_prefix(&head, TEXT_LIMIT)
        {
            p.text_truncated = item.size > TEXT_LIMIT as u64;
            p.text = Some(t);
            p.text_encoding = Some(enc);
        }
    }

    if matches!(ext.as_str(), "exe" | "dll" | "msi" | "sys") {
        let v = version_strings(path);
        if !v.is_empty() {
            p.version = Some(v);
        }
    }

    match shell_image(path, THUMB_MAX)
        .and_then(|img| Ok(((img.width, img.height), encode_png(&img)?)))
    {
        Ok((size, png)) => {
            p.thumbnail_size = Some(size);
            p.thumbnail_png = Some(png);
        }
        Err(e) => p.errors.push(format!("thumbnail: {e:#}")),
    }
    p
}
