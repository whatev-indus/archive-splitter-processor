use serde::Serialize;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tauri::{AppHandle, Emitter};

// ── Shared helpers ─────────────────────────────────────────────────────────────

pub fn emit_log(app: &AppHandle, msg: impl Into<String>) {
    let _ = app.emit("log", msg.into());
}

fn extract_quoted(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}

fn msf_to_sectors(s: &str) -> Option<u64> {
    let p: Vec<&str> = s.split(':').collect();
    if p.len() != 3 { return None; }
    let m: u64 = p[0].parse().ok()?;
    let s: u64 = p[1].parse().ok()?;
    let f: u64 = p[2].parse().ok()?;
    Some((m * 60 + s) * 75 + f)
}

fn sectors_to_msf(sectors: u64) -> String {
    let m = sectors / 4500;
    let r = sectors % 4500;
    let s = r / 75;
    let f = r % 75;
    format!("{:02}:{:02}:{:02}", m, s, f)
}

fn blocksize_for_type(t: &str) -> u64 {
    match t {
        "CDG" => 2448,
        "MODE1/2048" => 2048,
        "MODE2/2336" | "CDI/2336" => 2336,
        _ => 2352, // AUDIO, MODE1/2352, MODE2/2352, CDI/2352
    }
}

/// Per-track filename following redump convention.
/// ≤9 tracks: `Base (Track 1).bin`; >9 tracks: `Base (Track 01).bin`; 1 track: `Base.bin`
fn track_bin_name(base: &str, num: u32, total: usize) -> String {
    if total == 1 {
        format!("{}.bin", base)
    } else if total > 9 {
        format!("{} (Track {:02}).bin", base, num)
    } else {
        format!("{} (Track {}).bin", base, num)
    }
}

// ── CUE data model ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct CueIndex {
    id: u32,
    /// Sector offset within this file (parsed directly from MSF).
    sectors: u64,
}

#[derive(Clone)]
struct CueTrack {
    num: u32,
    track_type: String,
    indexes: Vec<CueIndex>,
    /// Sector length of this track (only populated after sector-count pass for single-bin).
    sectors: Option<u64>,
}

struct CueFile {
    path: PathBuf,
    size_bytes: u64,
    tracks: Vec<CueTrack>,
}

struct ParsedCue {
    files: Vec<CueFile>,
    blocksize: u64,
}

/// Full CUE parse. For a single-bin sheet the sector count of each track is
/// also computed so split can use it immediately.
fn parse_cue_full(cue_path: &Path) -> Result<ParsedCue, String> {
    let text =
        fs::read_to_string(cue_path).map_err(|e| format!("Cannot read CUE: {e}"))?;
    let cue_dir = cue_path.parent().unwrap_or(Path::new("."));

    let mut files: Vec<CueFile> = Vec::new();
    let mut cur_file: Option<CueFile> = None;
    let mut cur_track: Option<CueTrack> = None;
    let mut blocksize: Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with("FILE ") && (upper.contains("BINARY") || upper.contains("WAVE")) {
            // flush pending track
            if let (Some(f), Some(t)) = (cur_file.as_mut(), cur_track.take()) {
                f.tracks.push(t);
            }
            // flush pending file
            if let Some(f) = cur_file.take() {
                files.push(f);
            }
            if let Some(name) = extract_quoted(trimmed) {
                let path = cue_dir.join(name);
                let size_bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                cur_file = Some(CueFile { path, size_bytes, tracks: Vec::new() });
            }
        } else if upper.starts_with("TRACK ") {
            if let (Some(f), Some(t)) = (cur_file.as_mut(), cur_track.take()) {
                f.tracks.push(t);
            }
            let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
            let num: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            let track_type = parts.get(2).unwrap_or(&"AUDIO").trim().to_string();
            if blocksize.is_none() {
                blocksize = Some(blocksize_for_type(&track_type));
            }
            cur_track = Some(CueTrack {
                num,
                track_type,
                indexes: Vec::new(),
                sectors: None,
            });
        } else if let Some(rest) = upper.strip_prefix("INDEX ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                if let (Ok(id), Some(s)) = (parts[0].parse::<u32>(), msf_to_sectors(parts[1])) {
                    if let Some(ref mut t) = cur_track {
                        t.indexes.push(CueIndex { id, sectors: s });
                    }
                }
            }
        }
    }

    // flush remainder
    if let (Some(f), Some(t)) = (cur_file.as_mut(), cur_track.take()) {
        f.tracks.push(t);
    }
    if let Some(f) = cur_file.take() {
        files.push(f);
    }

    let blocksize = blocksize.unwrap_or(2352);

    // For a single-bin CUE, calculate per-track sector lengths
    if files.len() == 1 && !files[0].tracks.is_empty() {
        let total_sectors = files[0].size_bytes / blocksize;
        let n = files[0].tracks.len();
        let mut next = total_sectors;
        for i in (0..n).rev() {
            let start = files[0].tracks[i]
                .indexes
                .first()
                .map(|idx| idx.sectors)
                .unwrap_or(0);
            files[0].tracks[i].sectors = Some(next.saturating_sub(start));
            next = start;
        }
    }

    Ok(ParsedCue { files, blocksize })
}

// ── Merge: multi-bin → single bin ─────────────────────────────────────────────

/// Generate a merged CUE (single FILE entry, all INDEX absolute in merged bin).
fn gen_merged_cue(base_name: &str, files: &[CueFile], blocksize: u64) -> String {
    let mut out = format!("FILE \"{}.bin\" BINARY\n", base_name);
    let mut sector_offset = 0u64;
    for f in files {
        for t in &f.tracks {
            out += &format!("  TRACK {:02} {}\n", t.num, t.track_type);
            for i in &t.indexes {
                let abs = sectors_to_msf(sector_offset + i.sectors);
                out += &format!("    INDEX {:02} {}\n", i.id, abs);
            }
        }
        sector_offset += f.size_bytes / blocksize;
    }
    out
}

fn merge_blocking(
    app: &AppHandle,
    cue_path: &Path,
    base_name: &str,
    out_dir: &Path,
) -> Result<String, String> {
    let ParsedCue { files, blocksize } = parse_cue_full(cue_path)?;

    if files.is_empty() {
        return Err("No FILE entries found in CUE".to_string());
    }
    if files.len() == 1 {
        return Err("CUE already references a single bin file — nothing to merge".to_string());
    }

    emit_log(app, format!("Merging {} bin files…", files.len()));

    let merged_bin = out_dir.join(format!("{}.bin", base_name));
    if merged_bin.exists() {
        return Err(format!("Output file already exists: {}", merged_bin.display()));
    }

    let mut out =
        fs::File::create(&merged_bin).map_err(|e| format!("Cannot create merged bin: {e}"))?;

    const CHUNK: usize = 1 << 20; // 1 MiB
    let mut buf = vec![0u8; CHUNK];

    for f in &files {
        let fname = f
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        emit_log(app, format!("  appending {}", fname));
        let mut src = fs::File::open(&f.path)
            .map_err(|e| format!("Cannot open {}: {e}", f.path.display()))?;
        loop {
            let n = src.read(&mut buf).map_err(|e| format!("Read error: {e}"))?;
            if n == 0 { break; }
            out.write_all(&buf[..n]).map_err(|e| format!("Write error: {e}"))?;
        }
    }
    drop(out);

    let merged_cue_content = gen_merged_cue(base_name, &files, blocksize);
    let merged_cue = out_dir.join(format!("{}.cue", base_name));
    fs::write(&merged_cue, merged_cue_content)
        .map_err(|e| format!("Cannot write merged CUE: {e}"))?;

    emit_log(app, format!("Merged bin  : {}", merged_bin.display()));
    emit_log(app, format!("Merged CUE  : {}", merged_cue.display()));
    Ok(merged_bin.to_string_lossy().to_string())
}

// ── Split: single bin → per-track bins ────────────────────────────────────────

/// Generate a split CUE (one FILE per track, each INDEX 0-relative).
fn gen_split_cue(base_name: &str, tracks: &[CueTrack]) -> String {
    let total = tracks.len();
    let mut out = String::new();
    for t in tracks {
        let fname = track_bin_name(base_name, t.num, total);
        out += &format!("FILE \"{}\" BINARY\n", fname);
        out += &format!("  TRACK {:02} {}\n", t.num, t.track_type);
        let origin = t.indexes.first().map(|i| i.sectors).unwrap_or(0);
        for i in &t.indexes {
            let rel = sectors_to_msf(i.sectors.saturating_sub(origin));
            out += &format!("    INDEX {:02} {}\n", i.id, rel);
        }
    }
    out
}

fn split_blocking(
    app: &AppHandle,
    cue_path: &Path,
    base_name: &str,
    out_dir: &Path,
) -> Result<Vec<String>, String> {
    let ParsedCue { files, blocksize } = parse_cue_full(cue_path)?;

    if files.is_empty() {
        return Err("No FILE entries found in CUE".to_string());
    }
    if files.len() > 1 {
        return Err(
            "CUE references multiple bin files — run Merge first, then Split".to_string(),
        );
    }
    let merged_file = &files[0];
    if merged_file.tracks.len() <= 1 {
        return Err("Only one track in CUE — nothing to split".to_string());
    }

    emit_log(
        app,
        format!(
            "Splitting {} tracks from {}…",
            merged_file.tracks.len(),
            merged_file.path.display()
        ),
    );

    let total = merged_file.tracks.len();
    let mut src =
        fs::File::open(&merged_file.path).map_err(|e| format!("Cannot open source bin: {e}"))?;

    const CHUNK: usize = 1 << 20; // 1 MiB
    let mut created: Vec<String> = Vec::new();

    for t in &merged_file.tracks {
        let fname = track_bin_name(base_name, t.num, total);
        let out_path = out_dir.join(&fname);
        emit_log(app, format!("  writing {}", fname));

        let sectors = t.sectors.ok_or_else(|| {
            format!("Track {} has no computed sector length", t.num)
        })?;
        let track_bytes = sectors * blocksize;

        // Seek to the start of this track (first index, absolute in merged bin)
        let start_sector = t.indexes.first().map(|i| i.sectors).unwrap_or(0);
        src.seek(SeekFrom::Start(start_sector * blocksize))
            .map_err(|e| format!("Seek error: {e}"))?;

        let mut out_file =
            fs::File::create(&out_path).map_err(|e| format!("Cannot create {fname}: {e}"))?;
        let mut remaining = track_bytes;
        let mut buf = vec![0u8; CHUNK];

        while remaining > 0 {
            let to_read = (remaining as usize).min(CHUNK);
            let n = src
                .read(&mut buf[..to_read])
                .map_err(|e| format!("Read error on track {}: {e}", t.num))?;
            if n == 0 {
                return Err(format!(
                    "Unexpected EOF at track {} — bin file may be truncated",
                    t.num
                ));
            }
            out_file
                .write_all(&buf[..n])
                .map_err(|e| format!("Write error on track {}: {e}", t.num))?;
            remaining -= n as u64;
        }

        created.push(out_path.to_string_lossy().to_string());
    }

    let split_cue_content = gen_split_cue(base_name, &merged_file.tracks);
    let split_cue_path = out_dir.join(format!("{}.cue", base_name));
    fs::write(&split_cue_path, split_cue_content)
        .map_err(|e| format!("Cannot write split CUE: {e}"))?;

    emit_log(app, format!("Split CUE   : {}", split_cue_path.display()));
    Ok(created)
}

// ── Rename helpers ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct RenamePreview {
    pub old_name: String,
    pub new_name: String,
    pub kind: String,
}

/// Returns the `(Track ##).bin` tail from a filename, if present.
fn track_suffix(name: &str) -> Option<&str> {
    let lower = name.to_lowercase();
    let pos = lower.find("(track ")?;
    Some(&name[pos..])
}

pub fn compute_renames(folder: &Path, base_name: &str) -> Result<Vec<RenamePreview>, String> {
    let mut previews: Vec<RenamePreview> = Vec::new();

    for entry in fs::read_dir(folder)
        .map_err(|e| format!("Cannot read folder: {e}"))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("")
            .to_ascii_lowercase();

        let (new_name, kind) = match ext.as_str() {
            "bin" => {
                let Some(suffix) = track_suffix(&name) else { continue };
                (format!("{} {}", base_name, suffix), "bin")
            }
            "cue" => (format!("{}.cue", base_name), "cue"),
            "cdg" => {
                let stem =
                    path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let qualifier = stem
                    .find('[')
                    .map(|pos| format!(" {}", stem[pos..].trim()))
                    .unwrap_or_default();
                (format!("{}{}.cdg", base_name, qualifier), "cdg")
            }
            _ => continue,
        };

        previews.push(RenamePreview {
            old_name: name,
            new_name,
            kind: kind.to_string(),
        });
    }

    previews.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.old_name.cmp(&b.old_name)));
    Ok(previews)
}

fn update_cue_content(content: &str, renames: &[RenamePreview]) -> String {
    let mut result = content.to_string();
    for r in renames {
        if r.kind == "bin" {
            result = result.replace(&r.old_name, &r.new_name);
        }
    }
    result
}

// ── Tauri commands ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ScanResult {
    pub bin_count: u32,
    pub cue_found: bool,
    pub cdg_found: bool,
    pub detected_base_name: Option<String>,
}

#[derive(Serialize)]
pub struct LayoutInfo {
    /// "multi-bin" | "single-multi-track" | "single-single-track" | "no-cue" | "unknown"
    pub kind: String,
    pub bin_count: u32,
    pub track_count: u32,
}

#[tauri::command]
pub fn scan_folder(folder: String) -> Result<ScanResult, String> {
    let folder = PathBuf::from(&folder);
    if !folder.is_dir() {
        return Err("Not a valid directory".to_string());
    }

    let mut bin_count = 0u32;
    let mut cue_found = false;
    let mut cdg_found = false;
    let mut detected_base_name: Option<String> = None;

    for entry in fs::read_dir(&folder).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("")
            .to_ascii_lowercase();

        match ext.as_str() {
            "bin" => {
                bin_count += 1;
                // Fall back to extracting from a bin name only if no CUE seen yet
                if detected_base_name.is_none() {
                    let lower = name.to_lowercase();
                    if let Some(pos) = lower.find(" (track ") {
                        detected_base_name = Some(name[..pos].to_string());
                    }
                }
            }
            "cue" => {
                cue_found = true;
                // Prefer the CUE stem as the base name — it's the canonical identifier
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !stem.is_empty() {
                    detected_base_name = Some(stem);
                }
            }
            "cdg" => cdg_found = true,
            _ => {}
        }
    }

    Ok(ScanResult {
        bin_count,
        cue_found,
        cdg_found,
        detected_base_name,
    })
}

#[tauri::command]
pub fn detect_layout(folder: String) -> Result<LayoutInfo, String> {
    let folder = PathBuf::from(&folder);

    // Find .cue file
    let cue_path = fs::read_dir(&folder)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("cue"))
        });

    let bin_count = fs::read_dir(&folder)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("bin"))
        })
        .count() as u32;

    let Some(cue_path) = cue_path else {
        return Ok(LayoutInfo {
            kind: "no-cue".to_string(),
            bin_count,
            track_count: 0,
        });
    };

    match parse_cue_full(&cue_path) {
        Ok(ParsedCue { files, .. }) => {
            let file_count = files.len() as u32;
            let track_count: u32 = files.iter().map(|f| f.tracks.len() as u32).sum();

            let kind = if file_count > 1 {
                "multi-bin"
            } else if track_count > 1 {
                "single-multi-track"
            } else {
                "single-single-track"
            };

            Ok(LayoutInfo {
                kind: kind.to_string(),
                bin_count: file_count,
                track_count,
            })
        }
        Err(e) => Ok(LayoutInfo {
            kind: format!("parse-error: {e}"),
            bin_count,
            track_count: 0,
        }),
    }
}

#[tauri::command]
pub async fn bin_merge(
    app: AppHandle,
    folder: String,
    base_name: String,
) -> Result<String, String> {
    let folder = PathBuf::from(&folder);
    let base_name = base_name.trim().to_string();

    // Find the .cue in the folder
    let cue_path = fs::read_dir(&folder)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("cue"))
        })
        .ok_or_else(|| "No .cue file found in folder".to_string())?;

    tauri::async_runtime::spawn_blocking(move || {
        merge_blocking(&app, &cue_path, &base_name, &folder)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn bin_split(
    app: AppHandle,
    folder: String,
    base_name: String,
) -> Result<Vec<String>, String> {
    let folder = PathBuf::from(&folder);
    let base_name = base_name.trim().to_string();

    let cue_path = fs::read_dir(&folder)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("cue"))
        })
        .ok_or_else(|| "No .cue file found in folder".to_string())?;

    tauri::async_runtime::spawn_blocking(move || {
        split_blocking(&app, &cue_path, &base_name, &folder)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn preview_rename(folder: String, base_name: String) -> Result<Vec<RenamePreview>, String> {
    let folder = PathBuf::from(&folder);
    if !folder.is_dir() {
        return Err("Not a valid directory".to_string());
    }
    if base_name.trim().is_empty() {
        return Err("Base name cannot be empty".to_string());
    }
    compute_renames(&folder, base_name.trim())
}

#[tauri::command]
pub fn do_rename(
    app: AppHandle,
    folder: String,
    base_name: String,
) -> Result<Vec<String>, String> {
    let folder = PathBuf::from(&folder);
    let base_name = base_name.trim().to_string();
    let renames = compute_renames(&folder, &base_name)?;
    let mut log: Vec<String> = Vec::new();

    // Update and rename the .cue first (so old bin names are still readable)
    if let Some(cue) = renames.iter().find(|r| r.kind == "cue") {
        let old_path = folder.join(&cue.old_name);
        let new_path = folder.join(&cue.new_name);

        let content = fs::read_to_string(&old_path)
            .map_err(|e| format!("Cannot read {}: {e}", cue.old_name))?;
        let updated = update_cue_content(&content, &renames);

        fs::write(&new_path, &updated)
            .map_err(|e| format!("Cannot write {}: {e}", cue.new_name))?;
        if old_path != new_path {
            fs::remove_file(&old_path)
                .map_err(|e| format!("Cannot remove {}: {e}", cue.old_name))?;
        }

        let msg = format!("  {} → {}", cue.old_name, cue.new_name);
        emit_log(&app, &msg);
        log.push(msg);
    }

    for r in renames.iter().filter(|r| r.kind != "cue") {
        let old_path = folder.join(&r.old_name);
        let new_path = folder.join(&r.new_name);
        if old_path == new_path { continue; }

        fs::rename(&old_path, &new_path)
            .map_err(|e| format!("Cannot rename {}: {e}", r.old_name))?;

        let msg = format!("  {} → {}", r.old_name, r.new_name);
        emit_log(&app, &msg);
        log.push(msg);
    }

    Ok(log)
}

#[tauri::command]
pub async fn create_zip(
    app: AppHandle,
    folder: String,
    base_name: String,
    output_folder: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        create_zip_blocking(&app, &PathBuf::from(&folder), base_name.trim(), output_folder.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn create_zip_blocking(app: &AppHandle, folder: &Path, base_name: &str, output_folder: Option<&str>) -> Result<String, String> {
    let zip_dir = output_folder
        .map(PathBuf::from)
        .unwrap_or_else(|| folder.to_path_buf());
    let zip_path = zip_dir.join(format!("{}.zip", base_name));

    emit_log(app, format!("Creating {}.zip …", base_name));

    let zip_file =
        fs::File::create(&zip_path).map_err(|e| format!("Cannot create zip file: {e}"))?;
    let mut zip = zip::ZipWriter::new(zip_file);

    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);

    zip.add_directory(format!("{}/", base_name), stored)
        .map_err(|e| format!("Cannot add directory entry to zip: {e}"))?;

    // Only include files whose name starts with base_name — handles mixed-content folders
    let base_lower = base_name.to_lowercase();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(folder)
        .map_err(|e| format!("Cannot read folder: {e}"))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() { continue; }
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "bin" | "cue" | "cdg") { continue; }
        let fname = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if fname.to_lowercase().starts_with(&base_lower) {
            files.push((fname, path));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (fname, path) in &files {
        let zip_entry = format!("{}/{}", base_name, fname);
        emit_log(app, format!("  adding {}", fname));

        zip.start_file(&zip_entry, stored)
            .map_err(|e| format!("Cannot add {fname} to zip: {e}"))?;

        let mut src = fs::File::open(path).map_err(|e| format!("Cannot open {fname}: {e}"))?;
        std::io::copy(&mut src, &mut zip)
            .map_err(|e| format!("Cannot write {fname} into zip: {e}"))?;
    }

    zip.finish().map_err(|e| format!("Cannot finalize zip: {e}"))?;

    let zip_str = zip_path.to_string_lossy().to_string();
    emit_log(app, format!("ZIP created: {}", zip_str));
    Ok(zip_str)
}

#[tauri::command]
pub async fn upload_to_archive(
    app: AppHandle,
    zip_path: String,
    identifier: String,
    username: String,
    password: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        upload_blocking(&app, &zip_path, &identifier, &username, &password)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn upload_blocking(
    app: &AppHandle,
    zip_path: &str,
    identifier: &str,
    username: &str,
    password: &str,
) -> Result<(), String> {
    Command::new("ia")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| {
            "The 'ia' CLI is not installed.\nInstall it with:  pip install internetarchive"
                .to_string()
        })?;

    emit_log(app, "Configuring archive.org credentials…");
    let cfg = Command::new("ia")
        .args([
            "configure",
            &format!("--username={}", username),
            &format!("--password={}", password),
        ])
        .output()
        .map_err(|e| format!("ia configure failed: {e}"))?;

    if !cfg.status.success() {
        return Err(format!(
            "ia configure failed: {}",
            String::from_utf8_lossy(&cfg.stderr)
        ));
    }

    emit_log(app, format!("Uploading '{}' to archive.org…", identifier));
    emit_log(app, format!("  identifier : {}", identifier));
    emit_log(app, format!("  file       : {}", zip_path));

    let mut child = Command::new("ia")
        .args([
            "upload",
            identifier,
            zip_path,
            "--metadata=mediatype:audio",
            "--metadata=subject:CD+G",
            "--checksum",
            "--retries=10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ia upload: {e}"))?;

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    let app_a = app.clone();
    let t_out = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().flatten() {
            emit_log(&app_a, line);
        }
    });
    let app_b = app.clone();
    let t_err = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().flatten() {
            emit_log(&app_b, format!("[stderr] {}", line));
        }
    });

    let status = child.wait().map_err(|e| format!("ia wait failed: {e}"))?;
    t_out.join().ok();
    t_err.join().ok();

    if status.success() {
        emit_log(
            app,
            format!("Upload complete!  https://archive.org/details/{}", identifier),
        );
        Ok(())
    } else {
        Err("Upload failed — see log for details".to_string())
    }
}

#[tauri::command]
pub fn derive_identifier(base_name: String) -> String {
    base_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
