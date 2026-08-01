// Opening and saving .sql files.
//
// Uses the File System Access API, which gives real Save-in-place without any
// backend: `http://127.0.0.1` counts as a secure context, and the Tauri build
// runs on the same Chromium engine. Served from a plain-http remote host the API
// is absent, so both paths degrade — open through a file input, save through a
// download.

const TYPES = [{ description: 'SQL', accept: { 'application/sql': ['.sql'] } }];

export const canPickFiles = typeof window.showOpenFilePicker === 'function';
export const canSaveInPlace = typeof window.showSaveFilePicker === 'function';

/** `[]` when the user cancels. */
export async function openFiles() {
  if (canPickFiles) {
    let handles;
    try {
      handles = await window.showOpenFilePicker({ types: TYPES, multiple: true });
    } catch (e) {
      if (e.name === 'AbortError') return [];
      throw e;
    }
    return Promise.all(
      handles.map(async (handle) => ({
        name: handle.name,
        text: await (await handle.getFile()).text(),
        handle,
      })),
    );
  }

  return new Promise((resolve) => {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = '.sql,text/plain';
    input.multiple = true;
    input.addEventListener('change', async () => {
      resolve(
        await Promise.all(
          [...input.files].map(async (file) => ({
            name: file.name,
            text: await file.text(),
            // No handle means Save has nowhere to write back to.
            handle: null,
          })),
        ),
      );
    });
    input.addEventListener('cancel', () => resolve([]));
    input.click();
  });
}

/** `null` when the user cancels, or when saving in place is unavailable. */
export async function pickSaveTarget(suggestedName) {
  if (!canSaveInPlace) return null;
  try {
    return await window.showSaveFilePicker({ suggestedName, types: TYPES });
  } catch (e) {
    if (e.name === 'AbortError') return null;
    throw e;
  }
}

/**
 * A handle from the picker is writable straight away, but the grant does not
 * always survive, so ask again rather than failing mid-write.
 */
async function writable(handle) {
  const mode = { mode: 'readwrite' };
  if ((await handle.queryPermission(mode)) === 'granted') return true;
  return (await handle.requestPermission(mode)) === 'granted';
}

export async function writeFile(handle, text) {
  if (!(await writable(handle))) {
    throw new Error(`no permission to write ${handle.name}`);
  }
  const stream = await handle.createWritable();
  await stream.write(text);
  await stream.close();
}

/** The fallback when there is nowhere to write in place. */
export function download(name, text) {
  const url = URL.createObjectURL(new Blob([text], { type: 'application/sql' }));
  const link = document.createElement('a');
  link.href = url;
  link.download = name.endsWith('.sql') ? name : `${name}.sql`;
  link.click();
  URL.revokeObjectURL(url);
}
