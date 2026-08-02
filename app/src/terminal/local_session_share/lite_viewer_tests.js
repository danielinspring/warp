// Checks for the local-share guest viewer's terminal emulation, mainly the
// alternate-screen grid that mirrors full-screen host apps. The viewer is a
// browser script embedded in the Rust binary, so it cannot be covered by
// `cargo test`; this loads it against a stub DOM instead.
//
//   node app/src/terminal/local_session_share/lite_viewer_tests.js
const fs = require("fs");
const vm = require("vm");
const path = require("path");

const html = fs.readFileSync(path.join(__dirname, "lite_viewer.html"), "utf8");
let script = /<script>([\s\S]*)<\/script>/.exec(html)[1];
script = script.replace(
  "boot();",
  "globalThis.__exports = { Screen: Screen, Terminal: Terminal, renderLineHtml: renderLineHtml, session: session, terminal: terminal, render: render, styleCss: styleCss, history: history, view: view, ingestScrollback: ingestScrollback, loadOlderHistory: loadOlderHistory, INITIAL_VISIBLE_BLOCKS: INITIAL_VISIBLE_BLOCKS, HISTORY_PAGE_SIZE: HISTORY_PAGE_SIZE, blocksEl: blocksEl, sendExecuteCommand: sendExecuteCommand, sendWriteToPty: sendWriteToPty, transport: transport, submitGuestCommand: submitGuestCommand, renderGuestBar: renderGuestBar, guest: guest, guestBarEl: guestBarEl, ginputEl: ginputEl, renderMarkdown: renderMarkdown, upsertAgentExchange: upsertAgentExchange };"
);

function makeEl(tag) {
  const el = {
    tagName: tag,
    childNodes: [],
    listeners: {},
    style: { cssText: "", setProperty() {} },
    dataset: {},
    className: "",
    _text: "",
    _html: "",
    clientWidth: 800,
    clientHeight: 600,
    scrollTop: 0,
    scrollHeight: 600,
    get textContent() {
      return this._text;
    },
    set textContent(v) {
      this._text = v;
      this.childNodes = [];
    },
    get innerHTML() {
      return this._html;
    },
    set innerHTML(v) {
      this._html = v;
    },
    get firstChild() {
      return this.childNodes[0] || null;
    },
    get lastChild() {
      return this.childNodes[this.childNodes.length - 1] || null;
    },
    get lastElementChild() {
      return this.childNodes[this.childNodes.length - 1] || null;
    },
    appendChild(child) {
      const i = this.childNodes.indexOf(child);
      if (i !== -1) this.childNodes.splice(i, 1);
      this.childNodes.push(child);
      child.parentNode = this;
      return child;
    },
    removeChild(child) {
      const i = this.childNodes.indexOf(child);
      if (i !== -1) this.childNodes.splice(i, 1);
      child.parentNode = null;
      return child;
    },
    insertBefore(child, ref) {
      const i = this.childNodes.indexOf(ref);
      this.childNodes.splice(i === -1 ? this.childNodes.length : i, 0, child);
      child.parentNode = this;
      return child;
    },
    setAttribute() {},
    addEventListener(type, handler) {
      (this.listeners[type] || (this.listeners[type] = [])).push(handler);
    },
    dispatch(type, event) {
      (this.listeners[type] || []).forEach((handler) => handler(event || {}));
    },
    select() {},
    blur() {},
    focus() {},
    getBoundingClientRect() {
      return { width: 600, height: 400 };
    },
  };
  Object.defineProperty(el, "contentEditable", {
    get() {
      return this._contentEditable || "inherit";
    },
    set(v) {
      this._contentEditable = String(v);
    },
    configurable: true,
  });
  return el;
}

const byId = {};
const document = {
  body: makeEl("body"),
  createElement: makeEl,
  getElementById(id) {
    if (!byId[id]) byId[id] = makeEl("div");
    return byId[id];
  },
  execCommand() {},
  addEventListener() {},
};

const sandbox = {
  document,
  console,
  performance: { now: () => Date.now() },
  requestAnimationFrame: (fn) => setTimeout(fn, 0),
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval,
  TextDecoder,
  TextEncoder,
  atob: (s) => Buffer.from(s, "base64").toString("binary"),
  Date,
  Math,
  JSON,
  parseInt,
  isNaN,
  Uint8Array,
  Array,
  Error,
  navigator: {},
  location: { pathname: "/local-session/secret", protocol: "http:", host: "x" },
  fetch: () => Promise.reject(new Error("no network in harness")),
  WebSocket: function () {
    this.addEventListener = () => {};
    this.send = () => {};
  },
  globalThis: null,
  addEventListener() {},
};
sandbox.WebSocket.OPEN = 1;
sandbox.WebSocket.CLOSED = 3;
sandbox.window = sandbox;
sandbox.globalThis = sandbox;
vm.createContext(sandbox);
vm.runInContext(script, sandbox);

const {
  Screen,
  Terminal,
  renderLineHtml,
  sendExecuteCommand,
  sendWriteToPty,
  transport,
  submitGuestCommand,
  renderGuestBar,
  guest,
  guestBarEl,
  ginputEl,
  renderMarkdown,
  upsertAgentExchange,
} = sandbox.__exports;

/* --------------------------------------------------------------------- */

let failures = 0;
function check(name, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (!ok) {
    failures++;
    console.log("FAIL " + name);
    console.log("  actual   " + JSON.stringify(actual));
    console.log("  expected " + JSON.stringify(expected));
  } else {
    console.log("ok   " + name);
  }
}

function screenText(screen) {
  return screen.lines.map((l) => l.c.join("").replace(/\s+$/, ""));
}

function driver(rows, cols) {
  const screen = new Screen(rows, cols);
  const term = new Terminal({ getBuffer: () => screen });
  return { screen, write: (s) => term.write(s), term };
}

// Absolute cursor addressing (CUP) — the thing a block list cannot do.
{
  const { screen, write } = driver(5, 10);
  write("\u001b[3;4Hxy");
  check("CUP places text at row 3 col 4", screenText(screen), [
    "",
    "",
    "   xy",
    "",
    "",
  ]);
}

// Full repaint: home, erase display, draw.
{
  const { screen, write } = driver(4, 8);
  write("junk\r\nmore");
  write("\u001b[H\u001b[2Jclean");
  check("ED 2 clears the grid", screenText(screen), ["clean", "", "", ""]);
}

// Scroll region: text scrolls only inside the margins.
{
  const { screen, write } = driver(5, 6);
  write("\u001b[2;4r"); // region = rows 2..4
  write("\u001b[1;1Htop");
  write("\u001b[5;1Hbot");
  write("\u001b[4;1Ha\r\n"); // LF on the region's last row scrolls it
  write("b");
  check("scroll region keeps rows outside intact", screenText(screen), [
    "top",
    "",
    "a",
    "b",
    "bot",
  ]);
}

// Deferred wrap at the right margin.
{
  const { screen, write } = driver(3, 4);
  write("abcd");
  check("no premature wrap on last column", screenText(screen), [
    "abcd",
    "",
    "",
  ]);
  write("e");
  check("wraps once one more char arrives", screenText(screen), [
    "abcd",
    "e",
    "",
  ]);
}

// Line insert/delete, used heavily by TUIs redrawing lists.
{
  const { screen, write } = driver(4, 4);
  write("\u001b[1;1Hone\u001b[2;1Htwo\u001b[3;1Hsix");
  write("\u001b[2;1H\u001b[M"); // DL: delete row 2
  check("DL pulls following rows up", screenText(screen), [
    "one",
    "six",
    "",
    "",
  ]);
  write("\u001b[2;1H\u001b[L"); // IL: reopen row 2
  check("IL pushes rows back down", screenText(screen), [
    "one",
    "",
    "six",
    "",
  ]);
}

// Reverse index at the top of the region scrolls down.
{
  const { screen, write } = driver(3, 3);
  write("\u001b[1;1Haaa\u001b[2;1Hbbb");
  write("\u001b[1;1H\u001bM");
  check("ESC M scrolls down at the top row", screenText(screen), [
    "",
    "aaa",
    "bbb",
  ]);
}

// Erase honors the current background so boxes keep their fill.
{
  const { screen, write, term } = driver(2, 6);
  write("\u001b[44m\u001b[K");
  const filled = screen.lines[0].s.every((s) => s === term.eraseStyleIdx);
  check("EL fills with the active background", filled, true);
  check("erase style is not the default", term.eraseStyleIdx !== 0, true);
}

// Cursor rendering.
{
  const { screen, write } = driver(2, 5);
  write("\u001b[1;2Hab");
  const html = renderLineHtml(screen.lines[0], screen.col);
  check("cursor cell is rendered inverted", html.indexOf('class="cur"') !== -1, true);
  write("\u001b[?25l");
  check("DECTCEM hides the cursor", screen.cursorVisible, false);
}

// Resize follows the host's window and starts from a clean grid.
{
  const { screen, write } = driver(3, 5);
  write("hello");
  screen.resize(2, 8);
  check("resize adopts the new geometry", [screen.rows, screen.cols], [2, 8]);
  check("resize clears the grid for the repaint", screenText(screen), ["", ""]);
}

// End-to-end: the live session must switch buffers on ?1049h and back.
{
  const { session, terminal, render } = sandbox.__exports;
  terminal.write("\u001b[?1049h\u001b[2J\u001b[3;2Hclaude");
  check("?1049h enters the alternate screen", session.altScreen, true);
  check("body switches to the grid view", document.body.className, "alt");
  check(
    "full-screen output lands on the grid",
    screenText(session.screen)[2],
    " claude"
  );
  render();
  check(
    "grid rows are painted",
    byId.screen.childNodes[2].innerHTML.indexOf("claude") !== -1,
    true
  );
  terminal.write("\u001b[?1049l");
  check("?1049l leaves the alternate screen", session.altScreen, false);
  check("body returns to the block view", document.body.className, "");
}

// Claude Code leaves SGR 4 stuck across the whole UI; CSS underlines would
// rule every row, so the viewer must not emit text-decoration:underline.
{
  const { write } = driver(2, 20);
  write("\u001b[4munderlined");
  const hasUnderlineCss = sandbox.__exports.styleCss.some(
    (css) => css.indexOf("underline") !== -1
  );
  check(
    "SGR underline does not emit CSS text-decoration",
    hasUnderlineCss,
    false
  );
  write("\u001b[0m\u001b[9mstruck");
  const hasStrike = sandbox.__exports.styleCss.some(
    (css) => css.indexOf("line-through") !== -1
  );
  check("SGR strikethrough still emits CSS", hasStrike, true);
}

// Agent markdown rendering.
{
  check(
    "markdown renders headings",
    renderMarkdown("### Core Purpose\n\nHello"),
    "<h3>Core Purpose</h3><p>Hello</p>"
  );
  check(
    "markdown renders unordered lists",
    renderMarkdown("* one\n* two"),
    "<ul><li>one</li><li>two</li></ul>"
  );
  check(
    "markdown renders inline code and bold",
    renderMarkdown("Use `./script/bootstrap` and **Warp**"),
    "<p>Use <code>./script/bootstrap</code> and <strong>Warp</strong></p>"
  );
  check(
    "markdown escapes raw HTML",
    renderMarkdown("<script>alert(1)</script>"),
    "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
  );
  check(
    "markdown renders fenced code",
    renderMarkdown("```\necho hi\n```"),
    "<pre><code>echo hi</code></pre>"
  );

  upsertAgentExchange({
    id: "ex-1",
    query: "/agent what is this repo about?",
    output: "### Key Features\n\n* Local agent\n* Terminal",
    running: false,
  });
  const agentEl = sandbox.document.getElementById("blocks").lastChild;
  check("agent block mounts", !!agentEl, true);
  check("agent query is shown", agentEl.childNodes[1]._text, "/agent what is this repo about?");
  check(
    "agent output is rendered as HTML",
    agentEl.childNodes[2]._html,
    "<h3>Key Features</h3><ul><li>Local agent</li><li>Terminal</li></ul>"
  );

  upsertAgentExchange({
    id: "ex-1",
    query: "/agent what is this repo about?",
    output: "### Key Features\n\n* Local agent\n* Terminal\n* Sharing",
    running: true,
  });
  check(
    "streamed agent update replaces the same block",
    agentEl.childNodes[2]._html,
    "<h3>Key Features</h3><ul><li>Local agent</li><li>Terminal</li><li>Sharing</li></ul>"
  );
  check("running agent shows status", agentEl.childNodes[0]._text, "responding…");
}

// Guest execute / write-to-pty payloads.
{
  const sent = [];
  transport.ws = {
    readyState: 1, // OPEN
    send(payload) {
      sent.push(JSON.parse(payload));
    },
  };
  transport.viewerId = "viewer-1";
  transport.ptySeq = 0;
  transport.bufferId = null;

  // A buffer id we were never told must not silently swallow the command: the
  // hub routes on the socket's participant, not the buffer.
  check("ExecuteCommand sends without a known buffer id", sendExecuteCommand("x"), true);
  check("ExecuteCommand falls back to a placeholder buffer", sent[0], {
    ExecuteCommand: { buffer_id: "local-share", command: "x" },
  });

  sent.length = 0;
  transport.bufferId = "buf-1";
  check("ExecuteCommand sends when connected", sendExecuteCommand("echo hi"), true);
  check("ExecuteCommand payload shape", sent[0], {
    ExecuteCommand: { buffer_id: "buf-1", command: "echo hi" },
  });

  check("WriteToPty sends raw bytes", sendWriteToPty([0x0d, 0x41]), true);
  check("WriteToPty payload shape", sent[1], {
    WriteToPty: {
      request_id: { participant_id: "viewer-1", op_no: 0 },
      bytes: [13, 65],
    },
  });

  // The guest's command line is its own element: a render pass must never move
  // it (that blurs a focused contenteditable) and the host mirror must never
  // overwrite it.
  const { session, render } = sandbox.__exports;
  guest.joined = false;
  session.altScreen = false;
  renderGuestBar();
  check("guest bar is hidden before joining", guestBarEl.className, "off");

  guest.joined = true;
  renderGuestBar();
  check("guest bar is enabled once joined", guestBarEl.className, "");

  session.altScreen = true;
  renderGuestBar();
  check("guest bar is hidden in a full-screen app", guestBarEl.className, "off");
  session.altScreen = false;

  session.state = "prompt";
  session.typedInput = "host is typing this";
  ginputEl.textContent = "echo guest";
  render();
  check("host mirror does not overwrite the guest draft", ginputEl.textContent, "echo guest");
  check(
    "guest input is never re-parented by a render",
    ginputEl.parentNode || null,
    null
  );

  sent.length = 0;
  submitGuestCommand();
  check("Enter at the prompt runs the command", sent[0], {
    ExecuteCommand: { buffer_id: "buf-1", command: "echo guest" },
  });
  check("submitting clears the draft", ginputEl.textContent, "");

  sent.length = 0;
  session.state = "running";
  ginputEl.textContent = "yes";
  submitGuestCommand();
  check(
    "Enter during a running command writes to its stdin",
    sent[0].WriteToPty.bytes,
    [121, 101, 115, 13]
  );
  session.state = "idle";
}

// History pagination: mount a short tail, then page older blocks on demand.
(async function testHistoryPagination() {
  const {
    ingestScrollback,
    loadOlderHistory,
    history,
    view,
    INITIAL_VISIBLE_BLOCKS,
    HISTORY_PAGE_SIZE,
    blocksEl,
  } = sandbox.__exports;

  function utf8Bytes(text) {
    return Array.from(Buffer.from(text, "utf8"));
  }

  function fakeScrollback(count) {
    const blocks = [];
    for (let i = 0; i < count; i++) {
      const serialized = JSON.stringify({
        stylized_command: utf8Bytes("cmd-" + i),
        stylized_output: utf8Bytes("out-" + i),
        pwd: "/tmp",
        exit_code: 0,
      });
      blocks.push({ raw: utf8Bytes(serialized) });
    }
    return { blocks };
  }

  // Reset visible state left over from the alt-screen round-trip above.
  view.blocks.splice(0, view.blocks.length);
  history.older.splice(0, history.older.length);
  history.atStart = true;
  history.markerEl = null;
  history.loading = false;
  blocksEl.childNodes.splice(0, blocksEl.childNodes.length);
  blocksEl._text = "";

  const TOTAL = 30;
  await new Promise((resolve) => {
    ingestScrollback(fakeScrollback(TOTAL), function () {}, resolve);
  });

  check(
    "initial visible count is the configured tail",
    view.blocks.length,
    INITIAL_VISIBLE_BLOCKS
  );
  check(
    "remaining history stays unmounted",
    history.older.length,
    TOTAL - INITIAL_VISIBLE_BLOCKS
  );
  check("start marker is not terminal yet", history.atStart, false);

  const before = view.blocks.length;
  loadOlderHistory(HISTORY_PAGE_SIZE);
  check(
    "scroll-up mounts one page",
    view.blocks.length,
    before + HISTORY_PAGE_SIZE
  );
  check(
    "older pile shrinks by one page",
    history.older.length,
    TOTAL - INITIAL_VISIBLE_BLOCKS - HISTORY_PAGE_SIZE
  );

  // A short history never overflows the pane, so no scroll event fires; the
  // wheel and the marker are the paths that must still work.
  const beforeWheel = view.blocks.length;
  blocksEl.scrollTop = 0;
  history.lastLoadAt = 0;
  blocksEl.dispatch("wheel", { deltaY: -120 });
  check(
    "wheel-up at the top loads a page without a scrollbar",
    view.blocks.length,
    beforeWheel + HISTORY_PAGE_SIZE
  );

  const beforeClick = view.blocks.length;
  check("marker is clickable while history remains", !!history.markerEl, true);
  history.markerEl.dispatch("click");
  check(
    "clicking the marker loads a page",
    view.blocks.length,
    beforeClick + HISTORY_PAGE_SIZE
  );

  while (history.older.length) loadOlderHistory(HISTORY_PAGE_SIZE);
  check("all history can be revealed", view.blocks.length, TOTAL);
  check("older pile is empty at the end", history.older.length, 0);
  check("atStart once history is exhausted", history.atStart, true);

  const stuck = view.blocks.length;
  loadOlderHistory(HISTORY_PAGE_SIZE);
  check("further scroll-up is a no-op at the start", view.blocks.length, stuck);

  check(
    "prepended history keeps chronological order",
    view.blocks.map((b) => b.commandBuffer.toText()),
    Array.from({ length: TOTAL }, (_, i) => "cmd-" + i)
  );

  console.log(failures ? "\n" + failures + " FAILURES" : "\nall checks passed");
  process.exit(failures ? 1 : 0);
})().catch((err) => {
  console.error(err);
  process.exit(1);
});