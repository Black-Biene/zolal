// Runs the engine in a worker, so the page stays responsive and can show progress while big files are hidden
// or opened. Inputs are read from the chosen files a piece at a time (FileReaderSync only exists in workers),
// and outputs grow in fixed-size pages, so nothing large is ever copied whole: the shim's own File would copy
// the entire output on every write, which froze the page on a 1 GB video.
import { WASI, File, OpenFile, Directory, PreopenDirectory, ConsoleStdout, wasi as W } from "./vendor/browser_wasi_shim/index.js";

const reader = new FileReaderSync();

// A chosen file (Blob), read on demand. Read-only.
class BlobFile extends File {
  constructor(blob) { super([], { readonly: true }); this.blob = blob; }
  get size() { return BigInt(this.blob.size); }
  path_open(...args) {
    const r = super.path_open(...args);
    if (r.fd_obj) r.fd_obj = new OpenBlob(this);
    return r;
  }
}
class OpenBlob extends OpenFile {
  fd_pread(size, offset) {
    const from = Number(offset);
    return { ret: 0, data: new Uint8Array(reader.readAsArrayBuffer(this.file.blob.slice(from, from + size))) };
  }
  fd_read(size) {
    const r = this.fd_pread(size, this.file_pos);
    this.file_pos += BigInt(r.data.length);
    return r;
  }
  fd_seek(offset, whence) {
    if (whence === W.WHENCE_END) return super.fd_seek(this.file.size + offset, W.WHENCE_SET);
    return super.fd_seek(offset, whence);
  }
}

// A file the engine writes, kept as a list of pages that are never moved.
const PAGE = 8 << 20;
class PagedFile extends File {
  constructor() { super([]); this.pages = []; this.len = 0; }
  get size() { return BigInt(this.len); }
  path_open(oflags, rights, fdflags) {
    if (oflags & W.OFLAGS_TRUNC) this.resize(0);
    const fd = new OpenPaged(this);
    if (fdflags & W.FDFLAGS_APPEND) fd.file_pos = this.size;
    return { ret: W.ERRNO_SUCCESS, fd_obj: fd };
  }
  resize(len) {
    while (this.pages.length * PAGE < len) this.pages.push(new Uint8Array(PAGE));
    this.pages.length = Math.ceil(len / PAGE);
    // Bytes past a shrink must read back as zero if the file grows again.
    if (len < this.len && len % PAGE) this.pages[this.pages.length - 1].fill(0, len % PAGE);
    this.len = len;
  }
  write(data, at) {
    if (at + data.length > this.len) this.resize(at + data.length);
    for (let done = 0; done < data.length;) {
      const page = this.pages[Math.floor((at + done) / PAGE)], off = (at + done) % PAGE;
      const n = Math.min(PAGE - off, data.length - done);
      page.set(data.subarray(done, done + n), off);
      done += n;
    }
  }
  read(at, size) {
    const out = new Uint8Array(Math.max(0, Math.min(size, this.len - at)));
    for (let done = 0; done < out.length;) {
      const page = this.pages[Math.floor((at + done) / PAGE)], off = (at + done) % PAGE;
      const n = Math.min(PAGE - off, out.length - done);
      out.set(page.subarray(off, off + n), done);
      done += n;
    }
    return out;
  }
  blob() {
    return new Blob(this.pages.map((p, i) => p.subarray(0, Math.min(PAGE, this.len - i * PAGE))));
  }
}
class OpenPaged extends OpenFile {
  fd_write(data) {
    this.file.write(data, Number(this.file_pos));
    this.file_pos += BigInt(data.length);
    return { ret: 0, nwritten: data.length };
  }
  fd_pwrite(data, offset) {
    this.file.write(data, Number(offset));
    return { ret: 0, nwritten: data.length };
  }
  fd_read(size) {
    const data = this.file.read(Number(this.file_pos), size);
    this.file_pos += BigInt(data.length);
    return { ret: 0, data };
  }
  fd_pread(size, offset) { return { ret: 0, data: this.file.read(Number(offset), size) }; }
  fd_seek(offset, whence) {
    if (whence === W.WHENCE_END) return super.fd_seek(this.file.size + offset, W.WHENCE_SET);
    return super.fd_seek(offset, whence);
  }
  fd_filestat_set_size(size) { this.file.resize(Number(size)); return W.ERRNO_SUCCESS; }
  fd_allocate(offset, len) {
    if (Number(offset + len) > this.file.len) this.file.resize(Number(offset + len));
    return W.ERRNO_SUCCESS;
  }
}

// Every file the engine creates becomes a PagedFile.
const create = Directory.prototype.create_entry_for_path;
const swap = (dir, old, neu) => [...dir.contents].some(([name, e]) =>
  e === old ? (dir.contents.set(name, neu), true) : e instanceof Directory && swap(e, old, neu));
Directory.prototype.create_entry_for_path = function (path, isDir) {
  const r = create.call(this, path, isDir);
  if (!r.entry || isDir) return r;
  const paged = new PagedFile();
  swap(this, r.entry, paged);
  return { ...r, entry: paged };
};

onmessage = async ({ data: { module, args, files, pass } }) => {
  const stdout = [], stderr = [];
  const dir = new PreopenDirectory(".", new Map(Object.entries(files).map(([n, b]) => [n, new BlobFile(b)])));
  const wasi = new WASI(["zolal-cli", ...args], [`ZOLAL_PASS=${pass}`, "ZOLAL_PROGRESS=1"], [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered(l => stdout.push(l)),
    ConsoleStdout.lineBuffered(l => {
      const p = l.match(/^progress (\d+) (\d+)$/);
      if (p) postMessage({ progress: [+p[1], +p[2]] }); else stderr.push(l);
    }),
    dir]);
  let code;
  try {
    code = wasi.start(await WebAssembly.instantiate(module, { wasi_snapshot_preview1: wasi.wasiImport }));
  } catch (e) { code = 1; stderr.push("error: " + e); }
  const out = [...dir.dir.contents].filter(([, f]) => f instanceof PagedFile).map(([n, f]) => [n, f.blob()]);
  postMessage({ done: { code, stdout, stderr, files: new Map(out) } });
};
