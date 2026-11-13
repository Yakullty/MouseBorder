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

impl Payload {
    pub fn kind(&self) -> &'static str {
        match self { Payload::Text(_) => "text", Payload::Image(_) => "image", Payload::Files(_) => "files" }
    }
}

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
        Payload::Image(png) => write_image(clip, png),
        Payload::Files(paths) => write_files(clip, paths),
    }
}

/// arboard's macOS image setter builds a CGImage, which can fail and return before the old
/// clipboard is cleared, so a paste yields the previous content. Writing the PNG as an NSImage
/// avoids that path. Other platforms keep arboard's setter, which works there.
fn write_image(clip: &mut Clipboard, png: &[u8]) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = clip;
        macos::write_image(png)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let img = decode_png(png).ok_or("图片解码失败")?;
        clip.set_image(img).map_err(|e| e.to_string())
    }
}

/// arboard's Windows file-list setter never empties the clipboard, so stale formats survive and a
/// paste can return the previous content. Write CF_HDROP ourselves in one empty-then-set session.
fn write_files(clip: &mut Clipboard, paths: &[PathBuf]) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let _ = clip;
        windows::write_files(paths)
    }
    #[cfg(not(target_os = "windows"))]
    {
        clip.set().file_list(paths).map_err(|e| e.to_string())
    }
}

/// CF_HDROP blob: a 20-byte DROPFILES header (wide-char flag set) then each null-terminated UTF-16
/// path and a final extra null. Plain bytes so the layout is unit-testable on any platform.
#[cfg(any(target_os = "windows", test))]
fn hdrop_bytes(paths: &[Vec<u16>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&20u32.to_le_bytes()); // pFiles: offset of the file list
    out.extend_from_slice(&0i32.to_le_bytes());  // pt.x
    out.extend_from_slice(&0i32.to_le_bytes());  // pt.y
    out.extend_from_slice(&0i32.to_le_bytes());  // fNC
    out.extend_from_slice(&1i32.to_le_bytes());  // fWide: paths are UTF-16
    for w in paths {
        for u in w { out.extend_from_slice(&u.to_le_bytes()); }
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // list terminator (double null)
    out
}

/// canonicalize() on Windows yields a `\\?\` verbatim path; Explorer pastes the plain form.
#[cfg(any(target_os = "windows", test))]
fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") { format!(r"\\{rest}") }
    else if let Some(rest) = s.strip_prefix(r"\\?\") { rest.to_string() }
    else { s.to_string() }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::{runtime::ProtocolObject, AllocAnyThread};
    use objc2_app_kit::{NSImage, NSPasteboard};
    use objc2_foundation::{NSArray, NSData};

    pub fn write_image(png: &[u8]) -> Result<(), String> {
        write_image_to(&NSPasteboard::generalPasteboard(), png)
    }

    pub(super) fn write_image_to(pasteboard: &NSPasteboard, png: &[u8]) -> Result<(), String> {
        let data = NSData::with_bytes(png);
        let image = NSImage::initWithData(NSImage::alloc(), &data)
            .ok_or("NSImage 无法解码该图片")?;
        let objects = NSArray::from_retained_slice(&[ProtocolObject::from_retained(image)]);
        pasteboard.clearContents();
        let ok = pasteboard.writeObjects(&objects);
        if ok { Ok(()) } else { Err("写入图片到剪贴板失败（writeObjects 返回 NO）".into()) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use image::GenericImageView;
        use objc2_app_kit::NSPasteboardTypeTIFF;
        use objc2_foundation::NSString;

        /// Uses a named pasteboard so the user's real clipboard is never touched. Proves the NSImage
        /// write yields a TIFF that decodes back to the same image (arboard reads TIFF on macOS).
        #[test]
        fn writing_a_png_yields_a_tiff_that_decodes_back_to_the_image() {
            let pb = NSPasteboard::pasteboardWithName(&NSString::from_str("MouseBorderSelfTest"));
            let img = arboard::ImageData { width: 2, height: 2, bytes: std::borrow::Cow::Owned(vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]) };
            let png = crate::clipboard::encode_png(&img).expect("encode");
            write_image_to(&pb, &png).expect("write image");
            let tiff = unsafe { pb.dataForType(NSPasteboardTypeTIFF) }
                .expect("TIFF must be present so arboard and apps can read the image back");
            let decoded = image::load_from_memory(&tiff.to_vec()).expect("the written TIFF must decode");
            assert_eq!(decoded.dimensions(), (2, 2), "the pasted image must keep its size");
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::path::{Path, PathBuf};

    pub fn write_files(paths: &[PathBuf]) -> Result<(), String> {
        let mut wide = Vec::new();
        for p in paths {
            if let Some(w) = hdrop_path(p) { wide.push(w); }
        }
        if wide.is_empty() { return Err("没有可写入的文件路径".into()); }
        let bytes = super::hdrop_bytes(&wide);
        let _clip = clipboard_win::Clipboard::new_attempts(5).map_err(|e| format!("打开剪贴板失败：{e}"))?;
        clipboard_win::raw::set(clipboard_win::formats::CF_HDROP, &bytes)
            .map_err(|e| format!("写入文件列表失败：{e}"))
    }

    fn hdrop_path(p: &Path) -> Option<Vec<u16>> {
        let resolved = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = super::strip_verbatim(resolved.to_str()?);
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        wide.push(0);
        Some(wide)
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

/// Deletes every transfer folder under `root` except `keep`. The clipboard only ever points at the
/// newest transfer, so once a new one lands the older folders are unreachable garbage. Best-effort:
/// a folder still open elsewhere simply fails to delete and is retried on the next sweep.
pub fn sweep_received(root: &Path, keep: Option<&Path>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || keep == Some(path.as_path()) { continue }
        let _ = fs::remove_dir_all(&path);
    }
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
        // The transfer counter restarts each run, so an id can collide with a folder left by the
        // previous run; start empty so a paste never yields a mix of old and new files.
        let _ = fs::remove_dir_all(&dir);
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
    fn a_sweep_keeps_only_the_named_transfer_folder() {
        let root = tmp("sweep");
        for id in ["1", "2", "3"] {
            fs::create_dir_all(root.join(id)).unwrap();
            fs::write(root.join(id).join("f"), b"x").unwrap();
        }
        fs::write(root.join("stray.txt"), b"not a folder").unwrap();
        sweep_received(&root, Some(&root.join("2")));
        assert!(!root.join("1").exists());
        assert!(root.join("2").join("f").exists(), "the kept transfer must survive");
        assert!(!root.join("3").exists());
        assert!(root.join("stray.txt").exists(), "a non-folder entry is left alone");
        sweep_received(&root, None);
        assert!(!root.join("2").exists(), "no keep removes every transfer folder");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reused_transfer_id_starts_from_an_empty_folder() {
        let dir = tmp("reuse");
        fs::write(dir.join("stale.bin"), b"old").unwrap();
        let mut inc = Incoming::new(1, vec![FileEntry { name: "new.bin".into(), size: 3 }], dir.clone()).unwrap();
        assert!(inc.chunk(b"abc").unwrap());
        assert!(!dir.join("stale.bin").exists(), "a previous run's file must not linger");
        assert_eq!(fs::read(dir.join("new.bin")).unwrap(), b"abc");
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

    #[test]
    fn the_hdrop_blob_has_a_wide_dropfiles_header_and_double_null_terminator() {
        let path: Vec<u16> = "C:\\a.txt".encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = hdrop_bytes(&[path]);
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20, "pFiles must point just past the header");
        assert_eq!(i32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1, "fWide must mark UTF-16 paths");
        assert_eq!(bytes.len(), 20 + 8 * 2 + 2 * 2, "header + 8 path chars + path null + terminator null");
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0], "the list must end with two null wide chars");
    }

    #[test]
    fn verbatim_windows_paths_are_reduced_to_the_form_explorer_pastes() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\a.txt"), r"C:\Users\a.txt");
        assert_eq!(strip_verbatim(r"\\?\UNC\server\share\a.txt"), r"\\server\share\a.txt");
        assert_eq!(strip_verbatim(r"C:\plain\a.txt"), r"C:\plain\a.txt");
    }

    /// Restores the user's clipboard even if an assertion panics mid-test.
    struct Restore(String);
    impl Drop for Restore {
        fn drop(&mut self) { if let Ok(mut c) = Clipboard::new() { let _ = c.set_text(self.0.clone()); } }
    }

    /// Live check that this OS can actually write a file list / image to the clipboard and read it
    /// back pastable. Ignored by default because it touches the real clipboard; it refuses to run
    /// unless the clipboard holds plain text so the restore is lossless.
    #[test]
    #[ignore = "mutates the real clipboard; backs up and restores plain text"]
    fn the_live_clipboard_round_trips_files_and_images() {
        let mut clip = Clipboard::new().expect("clipboard");
        let backup = match clip.get_text() {
            Ok(t) => t,
            Err(_) => { eprintln!("clipboard is not plain text; skipping to avoid a lossy restore"); return; }
        };
        let _guard = Restore(backup);
        let dir = tmp("live-clip");
        let path = dir.join("hello.txt");
        fs::write(&path, b"hello clipboard").unwrap();

        write(&mut clip, &Payload::Files(vec![path.clone()])).expect("write file list");
        let file_read = read(&mut clip);
        let img = ImageData { width: 2, height: 2, bytes: std::borrow::Cow::Owned(vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255]) };
        let png = encode_png(&img).expect("encode");
        write(&mut clip, &Payload::Image(png)).expect("write image");
        let image_read = read(&mut clip);
        let _ = fs::remove_dir_all(&dir);

        let (_, fp) = file_read.expect("read back the file list");
        match fp {
            Payload::Files(list) => assert!(list.iter().any(|p| p.canonicalize().ok() == path.canonicalize().ok()), "the pasted file must be the one we wrote: {list:?}"),
            _ => panic!("expected a file list back from the clipboard"),
        }
        let (_, ip) = image_read.expect("read back the image");
        assert!(matches!(ip, Payload::Image(_)), "expected an image back from the clipboard");
    }
}