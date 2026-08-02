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
  "globalThis.__exports = { Screen: Screen, Terminal: Terminal, renderLineHtml: renderLineHtml, session: session, terminal: terminal, render: render };"
);

function makeEl(tag) {
  const el = {
    tagName: tag,
    childNodes: [],
    style: { cssText: "", setProperty() {} },
    dataset: {},
    className: "",
    _text: "",
    _html: "",
    clientWidth: 800,
    clientHeight: 600,
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
    addEventListener() {},
    select() {},
    getBoundingClientRect() {
      return { width: 600, height: 400 };
    },
  };
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
sandbox.window = sandbox;
sandbox.globalThis = sandbox;
vm.createContext(sandbox);
vm.runInContext(script, sandbox);

const { Screen, Terminal, renderLineHtml } = sandbox.__exports;

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

console.log(failures ? "\n" + failures + " FAILURES" : "\nall checks passed");
process.exit(failures ? 1 : 0);
