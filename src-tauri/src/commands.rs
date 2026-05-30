use md5::{Digest, Md5};
use serde::Serialize;
use std::collections::BTreeMap;
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
        compute_track_sectors(&mut files[0].tracks, total_sectors);
    }

    Ok(ParsedCue { files, blocksize })
}

/// Fill in each track's sector length from its start offset (first INDEX) and
/// the total image length. Tracks must be in ascending order.
fn compute_track_sectors(tracks: &mut [CueTrack], total_sectors: u64) {
    let n = tracks.len();
    let mut next = total_sectors;
    for i in (0..n).rev() {
        let start = tracks[i].indexes.first().map(|idx| idx.sectors).unwrap_or(0);
        tracks[i].sectors = Some(next.saturating_sub(start));
        next = start;
    }
}

// ── CloneCD (.ccd) layout ───────────────────────────────────────────────────────

/// Parse the `[TRACK N]` sections of a CloneCD `.ccd` sheet into tracks whose
/// INDEX offsets are absolute sectors (LBA) within the raw `.img`. This is the
/// layout source used when splitting a single `.img` that has no single-file CUE.
fn parse_ccd_tracks(ccd_path: &Path) -> Result<Vec<CueTrack>, String> {
    let text = fs::read_to_string(ccd_path).map_err(|e| format!("Cannot read CCD: {e}"))?;

    let mut tracks: Vec<CueTrack> = Vec::new();
    let mut cur: Option<CueTrack> = None;

    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("[TRACK ") {
            if let Some(c) = cur.take() {
                tracks.push(c);
            }
            let num: u32 = rest.trim_end_matches(']').trim().parse().unwrap_or(0);
            // CD+G discs are audio; the raw image is 2352-byte sectors regardless,
            // so AUDIO is the correct track type for the per-track bins we emit.
            cur = Some(CueTrack {
                num,
                track_type: "AUDIO".to_string(),
                indexes: Vec::new(),
                sectors: None,
            });
        } else if let Some(rest) = t.strip_prefix("INDEX ") {
            // Form: "0=0" / "1=33" — id=absolute_sector
            if let Some((id_s, sec_s)) = rest.split_once('=') {
                if let (Ok(id), Ok(sec)) =
                    (id_s.trim().parse::<u32>(), sec_s.trim().parse::<u64>())
                {
                    if let Some(ref mut c) = cur {
                        c.indexes.push(CueIndex { id, sectors: sec });
                    }
                }
            }
        }
    }
    if let Some(c) = cur.take() {
        tracks.push(c);
    }

    for tr in &mut tracks {
        tr.indexes.sort_by_key(|i| i.id);
    }
    tracks.sort_by_key(|t| t.num);

    if tracks.is_empty() {
        return Err("No [TRACK] entries found in CCD".to_string());
    }
    Ok(tracks)
}

// ── Splittable source resolution ──────────────────────────────────────────────

/// A single-file image plus its per-track layout, ready to be split.
struct SplitSource {
    image_path: PathBuf,
    blocksize: u64,
    tracks: Vec<CueTrack>,
}

/// Find the first file in `folder` with the given extension (case-insensitive).
fn find_with_ext(folder: &Path, ext: &str) -> Option<PathBuf> {
    let mut matches: Vec<PathBuf> = fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case(ext))
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// The folder's primary CUE sheet — the first non-underscore-prefixed `.cue`.
/// Underscore-prefixed sheets (e.g. `_base_singlefile_xxx.cue`) are scratch
/// files generated by tooling and are ignored.
fn find_main_cue(folder: &Path) -> Option<PathBuf> {
    let mut cues: Vec<PathBuf> = fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("cue"))
        })
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .map_or(false, |n| n.starts_with('_'))
        })
        .collect();
    cues.sort();
    cues.into_iter().next()
}

/// Resolve the single-file image to split and its per-track layout.
///
/// Two layouts are supported:
///   1. A single-file CUE whose referenced image (`.bin` or `.img`) exists.
///   2. A raw `.img` described by a CloneCD `.ccd` sheet — used when the CUE
///      references per-track `.bin` files that don't exist on disk yet.
fn resolve_split_source(folder: &Path, cue_path: Option<&Path>) -> Result<SplitSource, String> {
    // 1. Directly-splittable single-file CUE (covers .img referenced by a
    //    single-file sheet as well as an existing single .bin).
    if let Some(cue_path) = cue_path {
        let mut parsed = parse_cue_full(cue_path)?;
        if parsed.files.len() == 1 {
            let f = &mut parsed.files[0];
            if f.size_bytes > 0 && f.tracks.len() > 1 {
                if f.tracks.iter().any(|t| t.sectors.is_none()) {
                    compute_track_sectors(&mut f.tracks, f.size_bytes / parsed.blocksize);
                }
                return Ok(SplitSource {
                    image_path: f.path.clone(),
                    blocksize: parsed.blocksize,
                    tracks: std::mem::take(&mut f.tracks),
                });
            }
        } else if parsed.files.len() > 1 && parsed.files.iter().all(|f| f.size_bytes > 0) {
            // Multi-bin sheet whose files all exist → already split. Don't offer
            // to split again, and don't fall through to a leftover .img.
            return Err("Disc is already split into per-track bins".to_string());
        }
    }

    // 2. Raw .img described by a .ccd track layout.
    if let Some(img_path) = find_with_ext(folder, "img") {
        let size = fs::metadata(&img_path).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            return Err(format!("Image file is empty: {}", img_path.display()));
        }
        // Raw disc images are always 2352-byte sectors.
        const BLOCKSIZE: u64 = 2352;

        let ccd = find_with_ext(folder, "ccd").ok_or_else(|| {
            "Found .img but no .ccd track layout — cannot determine where to split".to_string()
        })?;
        let mut tracks = parse_ccd_tracks(&ccd)?;
        if tracks.len() <= 1 {
            return Err("Only one track in CCD — nothing to split".to_string());
        }
        compute_track_sectors(&mut tracks, size / BLOCKSIZE);

        return Ok(SplitSource {
            image_path: img_path,
            blocksize: BLOCKSIZE,
            tracks,
        });
    }

    Err("No splittable single-file image found (need a single-bin CUE, or an .img + .ccd)"
        .to_string())
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
    folder: &Path,
    base_name: &str,
    out_dir: &Path,
) -> Result<Vec<String>, String> {
    let cue_path = find_main_cue(folder);
    let SplitSource { image_path, blocksize, tracks } =
        resolve_split_source(folder, cue_path.as_deref())?;

    if tracks.len() <= 1 {
        return Err("Only one track — nothing to split".to_string());
    }

    emit_log(
        app,
        format!(
            "Splitting {} tracks from {}…",
            tracks.len(),
            image_path.display()
        ),
    );

    let total = tracks.len();
    let mut src =
        fs::File::open(&image_path).map_err(|e| format!("Cannot open source image: {e}"))?;

    const CHUNK: usize = 1 << 20; // 1 MiB
    let mut created: Vec<String> = Vec::new();

    for t in &tracks {
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

    let split_cue_content = gen_split_cue(base_name, &tracks);
    let split_cue_path = out_dir.join(format!("{}.cue", base_name));
    fs::write(&split_cue_path, split_cue_content)
        .map_err(|e| format!("Cannot write split CUE: {e}"))?;

    emit_log(app, format!("Split CUE   : {}", split_cue_path.display()));

    // The source sheet (e.g. a single-file or stale multi-bin CUE) is now
    // superseded by the per-track split CUE — remove it so only one authoritative
    // CUE remains for rename/zip.
    if let Some(src_cue) = &cue_path {
        if *src_cue != split_cue_path {
            let _ = fs::remove_file(src_cue);
            emit_log(app, format!("Removed obsolete CUE: {}", src_cue.display()));
        }
    }

    Ok(created)
}

// ── Rename helpers ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct RenamePreview {
    pub old_name: String,
    pub new_name: String,
    pub kind: String,
}

/// Extract a track number from a filename, supporting multiple conventions:
///   - `(Track 1).bin`  or  `(Track 01).bin`
///   - `01 - Track  1.bin`  (ripped with EAC / similar, variable whitespace)
///   - `Track01.bin`
fn extract_track_num(name: &str) -> Option<u32> {
    let lower = name.to_lowercase();

    // Pattern 1: "(Track N)" — redump / existing format
    if let Some(pos) = lower.find("(track ") {
        let rest = &lower[pos + 7..];
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if end > 0 {
            return rest[..end].parse().ok();
        }
    }

    // Pattern 2: "track" followed by optional whitespace then digits
    if let Some(pos) = lower.find("track") {
        let after = lower[pos + 5..].trim_start().to_string();
        let end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        if end > 0 {
            return after[..end].parse().ok();
        }
    }

    None
}

/// Format a track suffix with zero-padding when total > 9.
fn format_track_suffix(num: u32, total: usize) -> String {
    if total > 9 {
        format!("(Track {:02})", num)
    } else {
        format!("(Track {})", num)
    }
}

pub fn compute_renames(folder: &Path, base_name: &str) -> Result<Vec<RenamePreview>, String> {
    let all_entries: Vec<_> = fs::read_dir(folder)
        .map_err(|e| format!("Cannot read folder: {e}"))?
        .flatten()
        .filter(|e| e.path().is_file())
        .collect();

    // Count extensions to decide formatting modes.
    let bin_count = all_entries.iter().filter(|e| {
        e.path().extension().and_then(|x| x.to_str())
            .map_or(false, |x| x.eq_ignore_ascii_case("bin"))
    }).count();

    let cdg_count = all_entries.iter().filter(|e| {
        e.path().extension().and_then(|x| x.to_str())
            .map_or(false, |x| x.eq_ignore_ascii_case("cdg"))
    }).count();

    // More than one .cdg → per-track sidecar files, not a single monolithic subcode.
    let per_track_cdg = cdg_count > 1;

    // Use the larger of the two as the reference total for zero-padding.
    let track_total = bin_count.max(cdg_count);

    let mut previews: Vec<RenamePreview> = Vec::new();

    for entry in &all_entries {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("")
            .to_ascii_lowercase();

        let (new_name, kind) = match ext.as_str() {
            "bin" => {
                let Some(num) = extract_track_num(&name) else { continue };
                let suffix = format_track_suffix(num, track_total);
                (format!("{} {}.bin", base_name, suffix), "bin")
            }
            "cue" => (format!("{}.cue", base_name), "cue"),
            "cdg" => {
                if per_track_cdg {
                    // Per-track sidecar: rename exactly like its paired .bin.
                    let Some(num) = extract_track_num(&name) else { continue };
                    let suffix = format_track_suffix(num, track_total);
                    (format!("{} {}.cdg", base_name, suffix), "cdg")
                } else {
                    // Single monolithic CDG subcode — keep optional `[variant]` qualifier.
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let qualifier = stem
                        .find('[')
                        .map(|pos| format!(" {}", stem[pos..].trim()))
                        .unwrap_or_default();
                    (format!("{}{}.cdg", base_name, qualifier), "cdg")
                }
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

/// Build a fresh multi-bin CUE from the renamed bin list.
/// Used when the existing CUE references filenames that don't exist on disk.
fn gen_fresh_multi_bin_cue(bin_renames: &[RenamePreview]) -> String {
    let mut sorted: Vec<&RenamePreview> = bin_renames.iter().collect();
    sorted.sort_by_key(|r| extract_track_num(&r.new_name).unwrap_or(0));

    let mut out = String::new();
    for r in &sorted {
        let num = extract_track_num(&r.new_name).unwrap_or(0);
        out += &format!("FILE \"{}\" BINARY\n", r.new_name);
        out += &format!("  TRACK {:02} AUDIO\n", num);
        out += "    INDEX 01 00:00:00\n";
    }
    out
}

// ── Tauri commands ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ScanResult {
    pub bin_count: u32,
    pub cue_found: bool,
    pub cdg_found: bool,
    pub img_found: bool,
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
    let mut img_found = false;
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
                // Try to extract base from existing "(Track N)" style name
                if detected_base_name.is_none() {
                    let lower = name.to_lowercase();
                    if let Some(pos) = lower.find(" (track ") {
                        detected_base_name = Some(name[..pos].to_string());
                    }
                }
            }
            "cue" => {
                cue_found = true;
                // Ignore underscore-prefixed scratch sheets (e.g. `_base_singlefile_…cue`)
                // when picking a title.
                if name.starts_with('_') {
                    continue;
                }
                // Use the CUE stem only if it looks like a real title (not a generic name
                // like "cue", "disc", "track", etc.)
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let is_generic = stem.len() < 4
                    || matches!(stem.to_lowercase().as_str(), "cue" | "disc" | "track" | "cd");
                if !stem.is_empty() && !is_generic {
                    detected_base_name = Some(stem);
                }
            }
            "cdg" => cdg_found = true,
            "img" => img_found = true,
            _ => {}
        }
    }

    // Last resort: use the folder name itself (handles rips where filenames carry
    // no useful title info but the folder is named correctly).
    if detected_base_name.is_none() {
        if let Some(folder_name) = folder.file_name() {
            let name = folder_name.to_string_lossy().to_string();
            if !name.is_empty() {
                detected_base_name = Some(name);
            }
        }
    }

    Ok(ScanResult {
        bin_count,
        cue_found,
        cdg_found,
        img_found,
        detected_base_name,
    })
}

#[tauri::command]
pub fn detect_layout(folder: String) -> Result<LayoutInfo, String> {
    let folder = PathBuf::from(&folder);

    let cue_path = find_main_cue(&folder);

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

    // A splittable single-file source (single-bin CUE, or a raw .img + .ccd)
    // takes priority — this is what the Split action operates on.
    if let Ok(src) = resolve_split_source(&folder, cue_path.as_deref()) {
        if src.tracks.len() > 1 {
            let is_img = src
                .image_path
                .extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("img"));
            return Ok(LayoutInfo {
                kind: if is_img { "img-multi-track" } else { "single-multi-track" }.to_string(),
                bin_count: 1,
                track_count: src.tracks.len() as u32,
            });
        }
    }

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
pub async fn bin_split(
    app: AppHandle,
    folder: String,
    base_name: String,
) -> Result<Vec<String>, String> {
    let folder = PathBuf::from(&folder);
    let base_name = base_name.trim().to_string();

    tauri::async_runtime::spawn_blocking(move || {
        split_blocking(&app, &folder, &base_name, &folder)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Verify split bins against redumper logs ────────────────────────────────────

/// Extract an XML-style attribute value: `key="value"`.
fn xml_attr<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{}=\"", key);
    let start = s.find(&pat)? + pat.len();
    let end = s[start..].find('"')? + start;
    Some(&s[start..end])
}

/// Parse redumper/DAT `<rom name=… size=… md5=…>` entries → track_num → (size, md5).
fn parse_rom_hashes(text: &str) -> BTreeMap<u32, (u64, String)> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if !line.contains("<rom ") {
            continue;
        }
        let Some(name) = xml_attr(line, "name") else { continue };
        let size = xml_attr(line, "size").and_then(|s| s.parse::<u64>().ok());
        let md5 = xml_attr(line, "md5").map(|s| s.to_ascii_lowercase());
        if let (Some(num), Some(size), Some(md5)) = (extract_track_num(name), size, md5) {
            map.insert(num, (size, md5));
        }
    }
    map
}

/// Gather candidate redumper-log texts: plain `.log` files in the folder, any
/// `.log` in a `redumper/` subdir, and `.log` members inside any `.zip`.
fn collect_log_texts(folder: &Path) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();

    if let Ok(rd) = fs::read_dir(folder) {
        for e in rd.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext == "log" {
                if let Ok(b) = fs::read(&p) {
                    texts.push(String::from_utf8_lossy(&b).into_owned());
                }
            } else if ext == "zip" {
                if let Ok(f) = fs::File::open(&p) {
                    if let Ok(mut zip) = zip::ZipArchive::new(f) {
                        for i in 0..zip.len() {
                            if let Ok(mut zf) = zip.by_index(i) {
                                if zf.name().to_ascii_lowercase().ends_with(".log") {
                                    let mut buf = Vec::new();
                                    if zf.read_to_end(&mut buf).is_ok() {
                                        texts.push(String::from_utf8_lossy(&buf).into_owned());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let sub = folder.join("redumper");
    if let Ok(rd) = fs::read_dir(&sub) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .and_then(|x| x.to_str())
                .map_or(false, |x| x.eq_ignore_ascii_case("log"))
            {
                if let Ok(b) = fs::read(&p) {
                    texts.push(String::from_utf8_lossy(&b).into_owned());
                }
            }
        }
    }

    texts
}

/// The first log source that yields per-track `<rom>` hashes, if any.
fn find_expected_hashes(folder: &Path) -> Option<BTreeMap<u32, (u64, String)>> {
    collect_log_texts(folder)
        .into_iter()
        .map(|t| parse_rom_hashes(&t))
        .find(|m| !m.is_empty())
}

fn file_md5(path: &Path) -> Result<(String, u64), String> {
    let mut f = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

#[derive(Serialize, Clone)]
pub struct VerifyEntry {
    pub track: u32,
    pub ok: bool,
    pub detail: String,
}

#[derive(Serialize)]
pub struct VerifyReport {
    pub log_found: bool,
    pub all_ok: bool,
    pub checked: u32,
    pub results: Vec<VerifyEntry>,
}

#[tauri::command]
pub async fn verify_tracks(app: AppHandle, folder: String) -> Result<VerifyReport, String> {
    let folder = PathBuf::from(&folder);
    tauri::async_runtime::spawn_blocking(move || verify_blocking(&app, &folder))
        .await
        .map_err(|e| e.to_string())?
}

fn verify_blocking(app: &AppHandle, folder: &Path) -> Result<VerifyReport, String> {
    let Some(expected) = find_expected_hashes(folder) else {
        emit_log(app, "No redumper log with track hashes found — skipping verification.");
        return Ok(VerifyReport { log_found: false, all_ok: true, checked: 0, results: Vec::new() });
    };

    // Map split bins by track number (names carry "(Track NN)").
    let mut bins: BTreeMap<u32, PathBuf> = BTreeMap::new();
    for e in fs::read_dir(folder).map_err(|e| e.to_string())?.flatten() {
        let p = e.path();
        if p.extension()
            .and_then(|x| x.to_str())
            .map_or(false, |x| x.eq_ignore_ascii_case("bin"))
        {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if let Some(num) = extract_track_num(&name) {
                bins.insert(num, p);
            }
        }
    }

    let to_check: Vec<u32> = expected.keys().copied().filter(|n| *n != 0).collect();
    emit_log(app, format!("Verifying {} track(s) against redumper log…", to_check.len()));

    let mut results: Vec<VerifyEntry> = Vec::new();
    let mut all_ok = true;

    for num in to_check {
        let (exp_size, exp_md5) = &expected[&num];
        match bins.get(&num) {
            None => {
                all_ok = false;
                emit_log(app, format!("  Track {:02}: MISSING split file", num));
                results.push(VerifyEntry { track: num, ok: false, detail: "missing split file".to_string() });
            }
            Some(path) => {
                let (md5, size) = file_md5(path)?;
                let ok = &md5 == exp_md5 && size == *exp_size;
                if ok {
                    emit_log(app, format!("  Track {:02}: OK", num));
                    results.push(VerifyEntry { track: num, ok: true, detail: String::new() });
                } else {
                    all_ok = false;
                    let detail = format!("expected {} bytes / {}, got {} / {}", exp_size, exp_md5, size, md5);
                    emit_log(app, format!("  Track {:02}: FAIL — {}", num, detail));
                    results.push(VerifyEntry { track: num, ok: false, detail });
                }
            }
        }
    }

    if all_ok {
        emit_log(app, "All tracks match the redumper log ✓");
    } else {
        emit_log(app, "Some tracks do NOT match the redumper log ✗");
    }

    let checked = results.len() as u32;
    Ok(VerifyReport { log_found: true, all_ok, checked, results })
}

/// The per-track file names a Split would produce for `base_name`, plus the
/// regenerated split CUE. Lets the UI preview the result before splitting.
#[tauri::command]
pub fn preview_split(folder: String, base_name: String) -> Result<Vec<String>, String> {
    let folder = PathBuf::from(&folder);
    let base_name = base_name.trim();
    if base_name.is_empty() {
        return Err("Base name cannot be empty".to_string());
    }
    let cue_path = find_main_cue(&folder);
    let src = resolve_split_source(&folder, cue_path.as_deref())?;
    let total = src.tracks.len();
    let mut names: Vec<String> = src
        .tracks
        .iter()
        .map(|t| track_bin_name(base_name, t.num, total))
        .collect();

    // Include the .cdg subcode under the name the Rename step will give it, so
    // the preview reflects the complete resulting file set.
    if let Ok(renames) = compute_renames(&folder, base_name) {
        for r in renames.iter().filter(|r| r.kind == "cdg") {
            names.push(r.new_name.clone());
        }
    }

    names.push(format!("{}.cue", base_name));
    Ok(names)
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

        let bin_renames: Vec<RenamePreview> =
            renames.iter().filter(|r| r.kind == "bin").cloned().collect();

        // If the CUE doesn't reference any of the actual files on disk, the sheet
        // is stale/wrong (e.g. ripped under a different name). Regenerate from scratch.
        let cue_references_actual_files =
            bin_renames.iter().any(|r| content.contains(&r.old_name));

        let new_content = if !cue_references_actual_files && bin_renames.len() > 1 {
            emit_log(&app, "  CUE references unknown filenames — regenerating from actual files");
            gen_fresh_multi_bin_cue(&bin_renames)
        } else {
            update_cue_content(&content, &renames)
        };

        fs::write(&new_path, &new_content)
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

/// Read a child stream, treating `\n` as a committed line and `\r` as an
/// in-place progress update (the convention CLI progress bars like `ia`'s use).
/// `on_line` is called for committed lines, `on_progress` for `\r` updates.
fn stream_cr_lf<R: Read>(
    reader: R,
    mut on_line: impl FnMut(&str),
    mut on_progress: impl FnMut(&str),
) {
    let mut reader = BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    let flush = |buf: &[u8]| String::from_utf8_lossy(buf).trim_end().to_string();

    while let Ok(n) = reader.read(&mut byte) {
        if n == 0 {
            break;
        }
        match byte[0] {
            b'\n' => {
                let s = flush(&buf);
                if !s.is_empty() {
                    on_line(&s);
                }
                buf.clear();
            }
            b'\r' => {
                let s = flush(&buf);
                if !s.is_empty() {
                    on_progress(&s);
                }
                buf.clear();
            }
            b => buf.push(b),
        }
    }
    let s = flush(&buf);
    if !s.is_empty() {
        on_line(&s);
    }
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
    // `ia` writes status + a `\r`-updated progress bar to stderr. Surface
    // committed lines normally, and stream the progress bar as live, in-place
    // updates (throttled) so it doesn't look stalled then dump all at once.
    let app_b = app.clone();
    let t_err = std::thread::spawn(move || {
        let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
        stream_cr_lf(
            stderr,
            |line| {
                let _ = app_b.emit("log", line.to_string());
            },
            |prog| {
                if last.elapsed() >= std::time::Duration::from_millis(120) {
                    let _ = app_b.emit("upload-progress", prog.to_string());
                    last = std::time::Instant::now();
                }
            },
        );
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
