// Ghost text: the greyed suggestion at the cursor, and the rules about when it
// is allowed to be there.
//
// Hand-rolled, because the vendored CodeMirror is **5**.65.21 and inline
// completion is a CodeMirror 6 idea. The primitive that stands in for it is
// `setBookmark`, which parks a DOM node at a position: the suggestion is
// therefore never in the document. That matters more than it sounds — the tab's
// dirty marker, `getValue()`, a save, a run and the SQL parser all see the
// buffer exactly as it was typed, and there is no state to unwind if a widget
// leaks. Accepting is the only thing that writes text.
//
// **It does not fight the dropdown.** They are different things: the dropdown
// completes a name from the schema alkyon holds, this finishes a clause. While
// the dropdown is open it owns the keyboard — Enter and Tab mean *accept the
// highlighted row* — so nothing is suggested underneath it.
//
// Phase 0 shows **one line**. A model asked to finish a clause mostly answers
// with one; when it answers with more, the first line is shown and the first
// line is what Tab inserts, rather than a widget promising something longer than
// it will deliver.

/**
 * The states where a suggestion would be noise, an interruption, or wrong.
 *
 * Each of these was a decision rather than a default:
 *
 * - **A selection** means the next keystroke replaces it. Suggesting an
 *   insertion at one end of it is answering a question nobody asked.
 * - **The dropdown open** — see above.
 * - **Inside a string or a comment** is prose, not SQL. The mode already knows
 *   which it is, so this asks it rather than guessing.
 * - **An empty buffer** has nothing to go on but the schema, and "what should I
 *   write" is not what ghost text is for.
 */
function suppressed(cm) {
  if (cm.somethingSelected()) return true;
  if (cm.state.completionActive) return true;
  const cursor = cm.getCursor();
  const type = cm.getTokenAt(cursor).type;
  if (type === 'string' || type === 'comment') return true;
  return cm.getRange({ line: 0, ch: 0 }, cursor).trim() === '';
}

const samePlace = (a, b) => a && b && a.line === b.line && a.ch === b.ch;

/**
 * Wire ghost text onto `cm`.
 *
 * `request({ prefix, suffix, signal })` does the asking and resolves to the text
 * to show, or to something falsy for "nothing goes here" — which is an ordinary
 * answer, not a failure.
 */
export function createGhost(cm, { request, onStatus }) {
  let enabled = false;
  let debounceMs = 250;

  let timer = null;
  let controller = null;
  /** Which request is the current one; anything older is dropped on arrival. */
  let generation = 0;
  /** The bookmark on screen, and the position it belongs to. */
  let shown = null;

  function clearShown() {
    shown?.mark.clear();
    shown = null;
  }

  /** Forget the suggestion, the one being waited for, and the one being timed. */
  function cancel() {
    clearTimeout(timer);
    timer = null;
    controller?.abort();
    controller = null;
    // Nothing in flight counts any more. Bumping here rather than only in
    // `fire` is what makes an aborted fetch that still resolves harmless.
    generation += 1;
    clearShown();
  }

  function paint(text, at) {
    const widget = document.createElement('span');
    widget.className = 'cm-ghost';
    widget.textContent = text;
    // The suggestion must not be a click target: clicking it should put the
    // cursor where the character underneath is, as if it were not there.
    widget.addEventListener('mousedown', (event) => event.preventDefault());
    shown = { text, at, mark: cm.setBookmark(at, { widget, insertLeft: false }) };
  }

  async function fire() {
    timer = null;
    if (!enabled || suppressed(cm)) return;

    const at = cm.getCursor();
    const mine = (generation += 1);
    controller = new AbortController();

    let answer;
    try {
      answer = await request({
        prefix: cm.getRange({ line: 0, ch: 0 }, at),
        suffix: cm.getRange(at, { line: cm.lastLine(), ch: Infinity }),
        signal: controller.signal,
      });
    } catch (e) {
      // An abort is this code's own doing and never worth a word. Anything else
      // is worth one, once — a missing key or a refused request should say so
      // rather than leaving the feature quietly dead.
      if (e.name !== 'AbortError' && mine === generation) onStatus?.(e.message);
      return;
    }

    // Superseded while it was in flight, or the cursor has moved on: this
    // suggestion was for a buffer that no longer exists.
    if (mine !== generation || !samePlace(at, cm.getCursor())) return;

    const line = (answer?.text ?? '').split('\n')[0];
    if (!line.trim()) return;
    paint(line, at);
  }

  cm.on('changes', () => {
    cancel();
    if (!enabled) return;
    timer = setTimeout(fire, debounceMs);
  });

  // A cursor that has left the suggestion behind. Typing has already cleared it
  // through `changes`, so this is the click-and-arrow-key case.
  cm.on('cursorActivity', () => {
    if (shown && !samePlace(shown.at, cm.getCursor())) clearShown();
  });

  cm.on('blur', cancel);

  return {
    /** Both from the server's answer, so the folder decides, not the page. */
    configure({ enabled: on, debounce }) {
      enabled = Boolean(on);
      if (debounce > 0) debounceMs = debounce;
      if (!enabled) cancel();
    },

    /** Whether Tab and Esc should mean something other than usual. */
    visible: () => shown !== null,

    /** Insert what is on screen. False when there was nothing to insert. */
    accept() {
      if (!shown) return false;
      const { text, at } = shown;
      clearShown();
      cm.replaceRange(text, at, at, '+ghost');
      return true;
    },

    cancel,
  };
}
