// (network, FS/module, env, XHR) at runtime and records, for each, whether the
// attempt succeeded or was blocked. Run on the production `run_wasi_json` path,
// every attempt must be blocked — proving the WASI/QuickJS sandbox holds when a
// plugin tries to escape, not merely that the APIs are absent.
function write(s) { Javy.IO.writeSync(1, new TextEncoder().encode(s)); }

const r = {};

// 1. Network effect.
try {
  fetch("http://169.254.169.254/");
  r.fetch = "SUCCEEDED";
} catch (e) {
  r.fetch = "blocked:" + (e && e.name ? e.name : "error");
}

// 2. Module/FS effect.
try {
  require("fs");
  r.require = "SUCCEEDED";
} catch (e) {
  r.require = "blocked:" + (e && e.name ? e.name : "error");
}

// 3. Environment read.
try {
  r.env = process.env ? "SUCCEEDED" : "blocked:empty";
} catch (e) {
  r.env = "blocked:" + (e && e.name ? e.name : "error");
}

// 4. XMLHttpRequest (alternate network surface).
try {
  new XMLHttpRequest();
  r.xhr = "SUCCEEDED";
} catch (e) {
  r.xhr = "blocked:" + (e && e.name ? e.name : "error");
}

write(JSON.stringify(r));
