// reads the {input, call} envelope, keeps table rows whose `size` ($filesize)
// is >= the `min_size` arg ($filesize). Demonstrates real QuickJS execution,
// the Value↔JS rich-type bridge, and argument passing.
function readAll() {
  const chunks = [];
  const buf = new Uint8Array(4096);
  let n;
  while ((n = Javy.IO.readSync(0, buf)) > 0) { chunks.push(buf.slice(0, n)); }
  let len = 0; for (const c of chunks) len += c.length;
  const all = new Uint8Array(len); let o = 0;
  for (const c of chunks) { all.set(c, o); o += c.length; }
  return new TextDecoder().decode(all);
}
function write(s) { Javy.IO.writeSync(1, new TextEncoder().encode(s)); }

const env = JSON.parse(readAll());
const rows = Array.isArray(env.input) ? env.input : [];
const min = (env.call && env.call.named && env.call.named.min_size && env.call.named.min_size.$filesize) || 0;
const kept = rows.filter(r => r && r.size && typeof r.size.$filesize === "number" && r.size.$filesize >= min);
write(JSON.stringify(kept));
