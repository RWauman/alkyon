// The whole HTTP and WebSocket surface, in one place.

const q = encodeURIComponent;

async function call(method, path, body) {
  const response = await fetch(path, {
    method,
    headers: body ? { 'content-type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  const parsed = text ? JSON.parse(text) : null;
  if (!response.ok) {
    // The backend answers errors as {"error": "..."}; fall back to the status.
    throw new Error(parsed?.error ?? `${response.status} ${response.statusText}`);
  }
  return parsed;
}

/** The workspace file body is text, not JSON. */
async function putText(path, text) {
  const response = await fetch(path, {
    method: 'PUT',
    headers: { 'content-type': 'text/plain; charset=utf-8' },
    body: text,
  });
  if (!response.ok) {
    const body = await response.text();
    let message = `${response.status} ${response.statusText}`;
    try {
      message = JSON.parse(body).error ?? message;
    } catch {
      /* not JSON — keep the status line */
    }
    throw new Error(message);
  }
}

export const api = {
  health: () => call('GET', '/health'),

  workspace: () => call('GET', '/workspace'),
  openWorkspace: (path) => call('PUT', '/workspace', { path }),
  closeWorkspace: () => call('DELETE', '/workspace'),
  readWorkspaceFile: (path) => call('GET', `/workspace/file?path=${q(path)}`),
  writeWorkspaceFile: (path, text) => putText(`/workspace/file?path=${q(path)}`, text),

  shells: () => call('GET', '/shells'),
  sources: () => call('GET', '/sources'),
  addSource: (config) => call('POST', '/sources', config),
  /** Verify credentials without registering anything. */
  testConnection: (config) => call('POST', '/connection-test', config),
  /** Always resolves: `{ ok: false, error }` when the server is unreachable. */
  status: (id) => call('GET', `/sources/${q(id)}/status`),
  removeSource: (id) => call('DELETE', `/sources/${q(id)}`),
  databases: (id) => call('GET', `/sources/${q(id)}/databases`),
  tables: (id, db) => call('GET', `/sources/${q(id)}/tables?db=${q(db)}`),
  columns: (id, db, schema, table) =>
    call('GET', `/sources/${q(id)}/columns?db=${q(db)}&schema=${q(schema)}&table=${q(table)}`),

  /** The whole schema of one database, cached server-side. */
  schema: (id, db, { refresh = false } = {}) =>
    call('GET', `/sources/${q(id)}/schema?db=${q(db)}&refresh=${refresh}`),

  /** Search every indexed schema. An empty query returns coverage only. */
  search: (query, limit = 50) =>
    call('GET', `/search?q=${q(query)}&limit=${limit}`),
};

export function socketUrl(path) {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${location.host}${path}`;
}

/**
 * Run a statement, dispatching each streamed message to the handler of the same
 * name: columns, rows, affected, end, error, cancelled, closed.
 *
 * Returns a handle whose `cancel()` asks the server to stop — the server drops
 * the database stream, which is what actually cancels the query.
 */
export function runQuery({ sourceId, database, sql, maxRows }, handlers) {
  const socket = new WebSocket(socketUrl('/ws/query'));
  const terminal = new Set(['end', 'error', 'cancelled']);

  socket.addEventListener('open', () =>
    socket.send(JSON.stringify({ source_id: sourceId, database, sql, max_rows: maxRows })));

  socket.addEventListener('message', ({ data }) => {
    const message = JSON.parse(data);
    handlers[message.type]?.(message);
    if (terminal.has(message.type)) socket.close();
  });

  // A socket-level failure is reported the same way as a server-side one, so
  // callers have a single error path.
  socket.addEventListener('error', () =>
    handlers.error?.({ message: 'lost the connection to alkyon' }));
  socket.addEventListener('close', () => handlers.closed?.());

  return {
    cancel() {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: 'cancel' }));
      } else {
        socket.close();
      }
    },
  };
}
