//! Clipboard payloads beyond plain text, and the chunked transfer that makes a file copied on one
//! machine pastable on the other. arboard already speaks both platforms' file-list and image
//! formats, so only change detection and the transfer live here.

use crate::protocol::FileEntry;
use arboard::{Clipboard, ImageData};
use sha2::{Digest, Sha256};
use std::{fs, io::{Cursor, Read, Write}, path::{Path, PathBuf}};

/// Raw bytes per wire frame. The send channel is unbounded, so the transfer cap below is what
/// keeps a large copy from being held in memory all at once.
pub const CHUNK: usize = 256 * 1024;
pub const MAX_TRANSFER: u64 = 64 * 1024 * 1024;

pub enum Payload { Text(String), Image(Vec<u8>), Files(Vec<PathBuf>) }

fn hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn files_sig(files: &[PathBuf]) -> String {
    let parts: Vec<String> = files.iter()
        .map(|p| format!("{}:{}", p.display(), fs::metadata(p).map(|m| m.len()).unwrap_or(0)))
        .collect();
    format!("files:{}", parts.join("|"))
}

/// What the clipboard holds now, plus a signature that changes whenever the content does. Files
/// win over images and images over text: a file copy also offers a text flavour on some platforms,
/// and forwarding that would paste a path the peer cannot resolve.
pub fn read(clip: &mut Clipboard) -> Option<(String, Payload)> {
    if let Ok(files) = clip.get().file_list() {
        if !files.is_empty() {
            let sig = files_sig(&files);
            return Some((sig, Payload::Files(files)));
        }
    }
    if let Ok(img) = clip.get_image() {
        let sig = format!("image:{}", hash(&img.bytes));
        if let Some(png) = encode_png(&img) { return Some((sig, Payload::Image(png))); }
    }
    if let Ok(text) = clip.get_text() {
        if !text.is_empty() { return Some((format!("text:{text}"), Payload::Text(text))); }
    }
    None
}

/// The signature `read` will produce for this payload. The receiver records it before writing so
/// the poller cannot mistake our own write for a fresh local copy and send it straight back.
pub fn signature(payload: &Payload) -> String {
    match payload {
        Payload::Text(t) => format!("text:{t}"),
        Payload::Image(png) => match decode_png(png) {
            Some(img) => format!("image:{}", hash(&img.bytes)),
            None => String::new(),
        },
        Payload::Files(paths) => files_sig(paths),
    }
}

pub fn write(clip: &mut Clipboard, payload: &Payload) -> Result<(), String> {
    match payload {
        Payload::Text(t) => clip.set_text(t.clone()).map_err(|e| e.to_string()),
        Payload::Image(png) => {
            let img = decode_png(png).ok_or("图片解码失败")?;
            clip.set_image(img).map_err(|e| e.to_string())
        }
        Payload::Files(paths) => clip.set().file_list(paths).map_err(|e| e.to_string()),
    }
}

fn encode_png(img: &ImageData) -> Option<Vec<u8>> {
    let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.to_vec())?;
    let mut out = Vec::new();
    rgba.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

pub fn decode_png(png: &[u8]) -> Option<ImageData<'static>> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(ImageData { width: w as usize, height: h as usize, bytes: std::borrow::Cow::Owned(img.into_raw()) })
}

/// A received name is joined onto our own directory, so it must not be able to escape it.
pub fn safe_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 255 || name == "." || name == ".." { return None }
    if name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':')) { return None }
    Some(name.to_string())
}

fn too_big(total: u64) -> String {
    format!("文件共 {} MiB，超过 {} MiB 上限", total / 1_048_576, MAX_TRANSFER / 1_048_576)
}

/// The transferable part of a copied file list. Directories are skipped: the peer pastes a flat
/// list of files, and walking a tree would need a protocol of its own.
pub fn plan(files: &[PathBuf]) -> Result<(Vec<FileEntry>, Vec<PathBuf>), String> {
    let mut entries = Vec::new();
    let mut paths = Vec::new();
    for p in files {
        let Ok(meta) = fs::metadata(p) else { continue };
        if !meta.is_file() { continue }
        let Some(name) = p.file_name().and_then(|n| n.to_str()).and_then(safe_name) else { continue };
        entries.push(FileEntry { name, size: meta.len() });
        paths.push(p.clone());
    }
    if entries.is_empty() { return Err("没有可传输的文件（文件夹不支持）".into()) }
    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total > MAX_TRANSFER { return Err(too_big(total)) }
    Ok((entries, paths))
}

pub fn read_chunks(path: &Path, mut emit: impl FnMut(Vec<u8>)) -> Result<(), String> {
    let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 { break }
        emit(buf[..n].to_vec());
    }
    Ok(())
}

/// Splits an in-memory blob (a PNG) into the same chunk sizes used for files.
pub fn split_chunks(bytes: &[u8], mut emit: impl FnMut(Vec<u8>)) {
    for part in bytes.chunks(CHUNK) { emit(part.to_vec()); }
}

/// Reassembles one announced transfer. Chunks arrive in list order over an ordered stream, so the
/// declared sizes are enough to know where one file ends and the next begins.
pub struct Incoming {
    pub transfer: u64,
    entries: Vec<FileEntry>,
    index: usize,
    written: u64,
    dir: PathBuf,
    file: Option<fs::File>,
    pub done: Vec<PathBuf>,
}

impl Incoming {
    pub fn new(transfer: u64, entries: Vec<FileEntry>, dir: PathBuf) -> Result<Self, String> {
        if entries.is_empty() { return Err("空的文件清单".into()) }
        let total: u64 = entries.iter().map(|e| e.size).sum();
        if total > MAX_TRANSFER { return Err(too_big(total)) }
        for e in &entries {
            if safe_name(&e.name).is_none() { return Err(format!("文件名不安全：{}", e.name)) }
        }
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut me = Self { transfer, entries, index: 0, written: 0, dir, file: None, done: Vec::new() };
        me.open_next()?;
        Ok(me)
    }

    fn open_next(&mut self) -> Result<(), String> {
        while self.index < self.entries.len() {
            let size = self.entries[self.index].size;
            let path = self.dir.join(&self.entries[self.index].name);
            self.file = Some(fs::File::create(&path).map_err(|e| e.to_string())?);
            self.written = 0;
            if size != 0 { return Ok(()) }
            self.finish_file();
        }
        Ok(())
    }

    fn finish_file(&mut self) {
        self.file = None;
        self.done.push(self.dir.join(&self.entries[self.index].name));
        self.index += 1;
        self.written = 0;
    }

    /// Appends one chunk; `true` once every announced file is complete.
    pub fn chunk(&mut self, bytes: &[u8]) -> Result<bool, String> {
        if self.index >= self.entries.len() { return Err("传输已结束却又收到分块".into()) }
        let want = self.entries[self.index].size;
        if self.written + bytes.len() as u64 > want { return Err("分块超出声明的文件大小".into()) }
        self.file.as_mut().ok_or("文件未打开")?.write_all(bytes).map_err(|e| e.to_string())?;
        self.written += bytes.len() as u64;
        if self.written == want {
            self.finish_file();
            self.open_next()?;
        }
        Ok(self.index >= self.entries.len())
    }

    pub fn complete(&self) -> bool { self.index >= self.entries.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mouseborder-clip-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_received_name_cannot_escape_the_receiving_directory() {
        for bad in ["", ".", "..", "../evil", "a/b", "a\\b", "c:evil", "a\0b", "a\nb"] {
            assert!(safe_name(bad).is_none(), "{bad:?} must be rejected");
        }
        assert_eq!(safe_name("截图 2026.png").as_deref(), Some("截图 2026.png"));
        assert!(safe_name(&"x".repeat(256)).is_none());
    }

    #[test]
    fn chunks_reassemble_every_file_in_order_including_an_empty_one() {
        let dir = tmp("reassemble");
        let entries = vec![
            FileEntry { name: "a.bin".into(), size: 5 },
            FileEntry { name: "empty.txt".into(), size: 0 },
            FileEntry { name: "b.bin".into(), size: 3 },
        ];
        let mut inc = Incoming::new(7, entries, dir.clone()).unwrap();
        assert!(!inc.complete(), "an empty file must not finish the whole transfer");
        assert!(!inc.chunk(b"ab").unwrap());
        assert!(!inc.chunk(b"cde").unwrap(), "a file boundary is not the end of the transfer");
        assert!(inc.chunk(b"xyz").unwrap());
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"abcde");
        assert_eq!(fs::read(dir.join("empty.txt")).unwrap(), b"");
        assert_eq!(fs::read(dir.join("b.bin")).unwrap(), b"xyz");
        assert_eq!(inc.done.len(), 3);
        assert!(inc.chunk(b"more").is_err(), "a chunk after the end must be rejected");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chunk_may_not_overrun_the_declared_size() {
        let dir = tmp("overrun");
        let mut inc = Incoming::new(1, vec![FileEntry { name: "a".into(), size: 2 }], dir.clone()).unwrap();
        assert!(inc.chunk(b"abc").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsafe_name_or_an_oversized_manifest_is_refused_before_any_write() {
        let dir = tmp("refuse");
        assert!(Incoming::new(1, vec![FileEntry { name: "../evil".into(), size: 1 }], dir.clone()).is_err());
        assert!(Incoming::new(1, vec![FileEntry { name: "a".into(), size: MAX_TRANSFER + 1 }], dir.clone()).is_err());
        assert!(Incoming::new(1, vec![], dir.clone()).is_err());
        assert!(!dir.join("evil").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_keeps_only_regular_files_and_reports_the_size_cap() {
        let dir = tmp("plan");
        fs::write(dir.join("ok.txt"), b"hello").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        let (entries, paths) = plan(&[dir.join("ok.txt"), dir.join("sub"), dir.join("missing")]).unwrap();
        assert_eq!(entries, vec![FileEntry { name: "ok.txt".into(), size: 5 }]);
        assert_eq!(paths, vec![dir.join("ok.txt")]);
        assert!(plan(&[dir.join("sub")]).is_err(), "a folder alone is not transferable");
        let big = dir.join("big.bin");
        fs::File::create(&big).unwrap().set_len(MAX_TRANSFER + 1).unwrap();
        assert!(plan(&[big]).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunking_a_file_round_trips_as_raw_bytes() {
        let dir = tmp("chunks");
        let path = dir.join("payload.bin");
        let bytes: Vec<u8> = (0..(CHUNK * 2 + 17)).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &bytes).unwrap();
        let mut joined = Vec::new();
        let mut count = 0;
        read_chunks(&path, |part| { joined.extend(part); count += 1; }).unwrap();
        assert_eq!(joined, bytes);
        assert_eq!(count, 3, "two full chunks and a remainder");
        let mut split = Vec::new();
        split_chunks(&bytes, |part| split.extend(part));
        assert_eq!(split, bytes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_png_round_trip_preserves_every_pixel() {
        let img = ImageData { width: 2, height: 2, bytes: std::borrow::Cow::Owned(vec![
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 1, 2, 3, 4]) };
        let png = encode_png(&img).expect("encode");
        let back = decode_png(&png).expect("decode");
        assert_eq!((back.width, back.height), (2, 2));
        assert_eq!(back.bytes.as_ref(), img.bytes.as_ref());
    }
}