/* global window */

const { invoke } = window.__TAURI__.core;
const { open: openDialog } = window.__TAURI__.dialog;
const { listen } = window.__TAURI__.event;

// ── State ─────────────────────────────────────────────────────────────────────

let currentFolder = '';
let currentOutputFolder = '';
let currentZipPath = '';
let currentLayoutKind = '';

// ── DOM refs ──────────────────────────────────────────────────────────────────

const folderInput       = document.getElementById('folder');
const browseBtn         = document.getElementById('browse-btn');
const outputFolderInput = document.getElementById('output-folder');
const browseOutputBtn   = document.getElementById('browse-output-btn');
const scanInfo        = document.getElementById('scan-info');
const baseNameInput   = document.getElementById('base-name');
const layoutBadge     = document.getElementById('layout-badge');
const layoutDesc      = document.getElementById('layout-desc');
const layoutActions   = document.getElementById('layout-actions');
const splitBtn        = document.getElementById('split-btn');
const uploadToggle    = document.getElementById('upload-toggle');
const archiveFields   = document.getElementById('archive-fields');
const usernameInput   = document.getElementById('username');
const passwordInput   = document.getElementById('password');
const identifierInput = document.getElementById('identifier');
const previewBtn      = document.getElementById('preview-btn');
const renameBtn       = document.getElementById('rename-btn');
const zipBtn          = document.getElementById('zip-btn');
const uploadBtn       = document.getElementById('upload-btn');
const runAllBtn       = document.getElementById('run-all-btn');
const clearLogBtn     = document.getElementById('clear-log-btn');
const logEl           = document.getElementById('log');

// ── Log helpers ───────────────────────────────────────────────────────────────

function logLine(text, cls = 'line-info') {
  const el = document.createElement('div');
  el.className = cls;
  el.textContent = text;
  logEl.appendChild(el);
  logEl.scrollTop = logEl.scrollHeight;
}

function logHeading(text) { logLine(text, 'line-heading'); }
function logOk(text)      { logLine(text, 'line-ok'); }
function logWarn(text)    { logLine(text, 'line-warn'); }
function logError(text)   { logLine(text, 'line-error'); }
function logDim(text)     { logLine(text, 'line-dim'); }
function logSep()         { logDim('─'.repeat(52)); }

// ── Tauri log events ──────────────────────────────────────────────────────────

// Reused element for live, in-place upload progress (ia's `\r`-updated bar).
let progressEl = null;

listen('log', (event) => {
  const msg = event.payload;
  progressEl = null; // a committed line finalizes any in-place progress line
  if (msg.startsWith('Upload complete'))                  logOk(msg);
  else if (/error|fail/i.test(msg))                       logWarn(msg);
  else                                                    logLine(msg);
});

// Stream the progress bar as in-place updates rather than flooding the log.
listen('upload-progress', (event) => {
  if (!progressEl) {
    progressEl = document.createElement('div');
    progressEl.className = 'line-dim';
    logEl.appendChild(progressEl);
  }
  progressEl.textContent = event.payload;
  logEl.scrollTop = logEl.scrollHeight;
});

// ── Layout badge helpers ───────────────────────────────────────────────────────

const LAYOUT_META = {
  'multi-bin':           { cls: 'badge-multi',  label: 'Multi-bin',        desc: (t, b) => `${b} separate .bin files, ${t} tracks total.` },
  'single-multi-track':  { cls: 'badge-split',  label: 'Single-bin multi', desc: (t)    => `Single .bin, ${t} tracks — can be split into per-track files.` },
  'img-multi-track':     { cls: 'badge-split',  label: 'Single .img',      desc: (t)    => `Raw .img disc image, ${t} tracks — can be split into per-track .bin files.` },
  'single-single-track': { cls: 'badge-single', label: 'Single-bin',       desc: ()     => 'Single .bin, single track.' },
  'no-cue':              { cls: 'badge-error',  label: 'No CUE',           desc: ()     => 'No .cue sheet found in this folder.' },
  'unknown':             { cls: 'badge-dim',    label: 'Unknown',           desc: ()     => 'Could not determine layout.' },
};

function setLayout(kind, trackCount, binCount) {
  currentLayoutKind = kind;
  const meta = LAYOUT_META[kind] || { cls: 'badge-error', label: kind, desc: () => kind };

  layoutBadge.textContent = meta.label;
  layoutBadge.className   = 'badge ' + meta.cls;
  layoutDesc.textContent  = meta.desc(trackCount, binCount);

  layoutActions.style.display = 'flex';
  splitBtn.disabled = (kind !== 'single-multi-track' && kind !== 'img-multi-track');
}

function resetLayout() {
  currentLayoutKind = '';
  layoutBadge.textContent = 'unknown';
  layoutBadge.className   = 'badge badge-dim';
  layoutDesc.textContent  = 'Select a folder to detect the disc layout.';
  layoutActions.style.display = 'none';
  splitBtn.disabled = true;
}

// ── Folder browse ─────────────────────────────────────────────────────────────

browseBtn.addEventListener('click', async () => {
  const selected = await openDialog({
    directory: true,
    multiple: false,
    title: 'Select folder containing .bin / .cue / .cdg files',
  });
  if (!selected) return;

  currentFolder = selected;
  folderInput.value = selected;
  currentZipPath = '';
  resetLayout();

  await Promise.all([scanFolder(selected), detectLayout(selected)]);
});

browseOutputBtn.addEventListener('click', async () => {
  const selected = await openDialog({
    directory: true,
    multiple: false,
    title: 'Select output folder for ZIP (leave empty to use source folder)',
  });
  if (!selected) return;
  currentOutputFolder = selected;
  outputFolderInput.value = selected;
});

async function scanFolder(folder) {
  try {
    const r = await invoke('scan_folder', { folder });
    const parts = [];
    const okBad = (ok, label) =>
      `<span class="${ok ? 'ok' : 'bad'}">${ok ? '✓' : '✗'}</span> ${label}`;

    if (r.img_found && r.bin_count === 0) {
      parts.push(okBad(true, '.img disc image'));
    } else {
      parts.push(okBad(r.bin_count > 0, `${r.bin_count} .bin track${r.bin_count !== 1 ? 's' : ''}`));
    }
    parts.push(okBad(r.cue_found,       '.cue sheet'));
    parts.push(okBad(r.cdg_found,       '.cdg subcode'));

    scanInfo.innerHTML = parts.join('&nbsp;&nbsp;|&nbsp;&nbsp;');
    scanInfo.classList.remove('hidden');

    if (r.detected_base_name && !baseNameInput.value) {
      baseNameInput.value = r.detected_base_name;
      await updateIdentifier(r.detected_base_name);
    }
  } catch (err) {
    scanInfo.innerHTML = `<span class="bad">Could not scan: ${err}</span>`;
    scanInfo.classList.remove('hidden');
  }
}

async function detectLayout(folder) {
  try {
    const info = await invoke('detect_layout', { folder });
    setLayout(info.kind, info.track_count, info.bin_count);
  } catch (err) {
    setLayout('unknown', 0, 0);
  }
}

// ── Base name → identifier ────────────────────────────────────────────────────

baseNameInput.addEventListener('input', async () => {
  await updateIdentifier(baseNameInput.value);
});

async function updateIdentifier(baseName) {
  if (!baseName.trim()) { identifierInput.value = ''; return; }
  try { identifierInput.value = await invoke('derive_identifier', { baseName }); }
  catch (_) {}
}

// ── Upload toggle ─────────────────────────────────────────────────────────────

uploadToggle.addEventListener('change', () => {
  archiveFields.classList.toggle('disabled', !uploadToggle.checked);
  uploadBtn.disabled = !uploadToggle.checked || !currentZipPath;
});
archiveFields.classList.add('disabled');

// ── Validation ────────────────────────────────────────────────────────────────

function validateBase() {
  if (!currentFolder)          { logError('No folder selected.');      return false; }
  if (!baseNameInput.value.trim()) { logError('Base name is required.'); return false; }
  return true;
}

function validateUpload() {
  if (!usernameInput.value.trim())  { logError('Archive.org username is required.');  return false; }
  if (!passwordInput.value.trim())  { logError('Archive.org password is required.');  return false; }
  if (!identifierInput.value.trim()){ logError('Archive.org identifier is required.'); return false; }
  return true;
}

function setWorking(working) {
  [previewBtn, renameBtn, zipBtn, uploadBtn, runAllBtn, browseBtn, splitBtn]
    .forEach(btn => { btn.disabled = working; });
  if (!working) {
    uploadBtn.disabled = !uploadToggle.checked || !currentZipPath;
    // re-evaluate layout buttons
    if (currentFolder) detectLayout(currentFolder).catch(() => {});
  }
}

// ── Split ─────────────────────────────────────────────────────────────────────

splitBtn.addEventListener('click', async () => {
  if (!validateBase()) return;
  logSep();
  logHeading('Splitting disc image → per-track files…');
  setWorking(true);
  try {
    const created = await invoke('bin_split', {
      folder: currentFolder,
      baseName: baseNameInput.value.trim(),
    });
    logOk(`Split into ${created.length} track file(s).`);
    await runVerify();
    await detectLayout(currentFolder);
  } catch (err) {
    logError(`Split failed: ${err}`);
  } finally {
    setWorking(false);
  }
});

// ── Verify split bins against redumper logs ─────────────────────────────────────

// Returns the report. Logs a summary; does not throw on mismatch.
async function runVerify() {
  const report = await invoke('verify_tracks', { folder: currentFolder });
  if (!report.log_found) {
    logDim('  No redumper log found — verification skipped.');
  } else if (report.all_ok) {
    logOk(`  Verified ${report.checked} track(s) against redumper log — all match.`);
  } else {
    const bad = report.results.filter(r => !r.ok).length;
    logError(`  ${bad} of ${report.checked} track(s) do NOT match the redumper log.`);
  }
  return report;
}

// ── Preview rename ────────────────────────────────────────────────────────────

previewBtn.addEventListener('click', async () => {
  if (!validateBase()) return;
  logSep();

  // Not yet split (single .bin / .img disc image) → preview the per-track files
  // the Split step will produce, so you can see how they'll be named.
  const splittable = currentLayoutKind === 'img-multi-track'
                  || currentLayoutKind === 'single-multi-track';
  if (splittable) {
    logHeading('Preview — files after Split:');
    try {
      const names = await invoke('preview_split', {
        folder: currentFolder,
        baseName: baseNameInput.value.trim(),
      });
      names.forEach(n => logOk(`  + ${n}`));
    } catch (err) {
      logError(`Preview failed: ${err}`);
    }
    return;
  }

  logHeading('Preview rename:');
  try {
    const renames = await invoke('preview_rename', {
      folder: currentFolder,
      baseName: baseNameInput.value.trim(),
    });
    if (renames.length === 0) { logWarn('No matching files found.'); return; }
    renames.forEach(r => {
      if (r.old_name === r.new_name) logDim(`  (unchanged)  ${r.old_name}`);
      else { logLine(`  ${r.old_name}`); logOk(`    → ${r.new_name}`); }
    });
  } catch (err) {
    logError(`Preview failed: ${err}`);
  }
});

// ── Rename ────────────────────────────────────────────────────────────────────

renameBtn.addEventListener('click', async () => {
  if (!validateBase()) return;
  logSep();
  logHeading('Renaming files…');
  setWorking(true);
  try {
    const log = await invoke('do_rename', {
      folder: currentFolder,
      baseName: baseNameInput.value.trim(),
    });
    logOk(`Done — ${log.length} file(s) renamed.`);
  } catch (err) {
    logError(`Rename failed: ${err}`);
  } finally {
    setWorking(false);
  }
});

// ── Create ZIP ────────────────────────────────────────────────────────────────

zipBtn.addEventListener('click', async () => {
  if (!validateBase()) return;
  logSep();
  logHeading('Creating ZIP…');
  setWorking(true);
  try {
    const zipPath = await invoke('create_zip', {
      folder: currentFolder,
      baseName: baseNameInput.value.trim(),
      outputFolder: currentOutputFolder || null,
    });
    currentZipPath = zipPath;
    logOk(`ZIP ready: ${zipPath}`);
    if (uploadToggle.checked) uploadBtn.disabled = false;
  } catch (err) {
    logError(`ZIP failed: ${err}`);
  } finally {
    setWorking(false);
  }
});

// ── Upload ────────────────────────────────────────────────────────────────────

uploadBtn.addEventListener('click', async () => {
  if (!validateUpload()) return;
  if (!currentZipPath) { logError('No ZIP file available. Create a ZIP first.'); return; }
  logSep();
  logHeading('Uploading to Archive.org…');
  setWorking(true);
  try {
    await invoke('upload_to_archive', {
      zipPath: currentZipPath,
      identifier: identifierInput.value.trim(),
      username: usernameInput.value.trim(),
      password: passwordInput.value,
    });
  } catch (err) {
    logError(`Upload failed: ${err}`);
  } finally {
    setWorking(false);
  }
});

// ── Run All ───────────────────────────────────────────────────────────────────

runAllBtn.addEventListener('click', async () => {
  if (!validateBase()) return;
  if (uploadToggle.checked && !validateUpload()) return;

  logSep();
  logHeading('Running full pipeline…');
  setWorking(true);

  const baseName = baseNameInput.value.trim();
  const splittable = currentLayoutKind === 'img-multi-track'
                  || currentLayoutKind === 'single-multi-track';
  const totalSteps = splittable ? 5 : 3;
  let step = 0;
  const stepHead = (label) => logHeading(`Step ${++step}/${totalSteps} — ${label}`);

  try {
    if (splittable) {
      stepHead('Split');
      const created = await invoke('bin_split', { folder: currentFolder, baseName });
      logOk(`Split into ${created.length} track file(s).`);

      stepHead('Verify against redumper log');
      const report = await runVerify();
      if (report.log_found && !report.all_ok) {
        throw new Error('track hashes do not match the redumper log — aborting before upload.');
      }
    }

    stepHead('Rename');
    await invoke('do_rename', { folder: currentFolder, baseName });
    logOk('Rename complete.');

    stepHead('Create ZIP');
    const zipPath = await invoke('create_zip', {
      folder: currentFolder,
      baseName,
      outputFolder: currentOutputFolder || null,
    });
    currentZipPath = zipPath;
    logOk(`ZIP complete: ${zipPath}`);

    if (uploadToggle.checked) {
      stepHead('Upload to Archive.org');
      await invoke('upload_to_archive', {
        zipPath,
        identifier: identifierInput.value.trim(),
        username: usernameInput.value.trim(),
        password: passwordInput.value,
      });
    } else {
      stepHead('Upload skipped (toggle is off)');
    }

    logSep();
    logOk('Pipeline complete.');
  } catch (err) {
    logError(`Pipeline error: ${err}`);
  } finally {
    setWorking(false);
    if (uploadToggle.checked) uploadBtn.disabled = !currentZipPath;
  }
});

// ── Clear log ─────────────────────────────────────────────────────────────────

clearLogBtn.addEventListener('click', () => { logEl.innerHTML = ''; });
