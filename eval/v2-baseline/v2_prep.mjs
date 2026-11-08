// Faithful offline re-implementation of Selene V2's *indexing-side* text pipeline and query
// expansion (Selene commit e09802592ccb61e1764e1f0c2c1ffbb3e8de25d4). Each function below is a
// verbatim port; the Selene source it mirrors is cited. Nothing in Selene is modified: the only
// Selene artefact used is its installed `gpt-tokenizer` package (o200k_base), loaded read-only so
// token boundaries are identical to Selene's chunker.
//
//   node v2_prep.mjs corpus  <manifest.json|docs.jsonl> <root-or-"-"> <out chunks.jsonl> [--raw-text]
//   node v2_prep.mjs queries <golden.jsonl> <out queries.jsonl>
//
// `--raw-text` = Selene's `indexTextToVectorDB` path (manual content: no line checks, no parser).
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import readline from "node:readline";

const SELENE = process.env.SELENE_ROOT ?? path.resolve(import.meta.dirname, "../../../selene"); // Selene checkout next to this repository
const require = createRequire(path.join(SELENE, "package.json"));
const { encode, decode } = require("gpt-tokenizer/cjs/encoding/o200k_base");

// DEFAULT_SETTINGS (lib/settings/settings-manager.ts:389-404) -> env -> loadConfigFromEnv.
export const V2 = {
  tokenChunkSize: 96,
  tokenChunkStride: 48,
  maxFileLines: 3000,
  maxLineLength: 1000,
  // lib/ai/local-embeddings.ts:14-15
  queryPrefix: "Represent this code for search:",
  queryPrefixMaxChars: 512,
};

// ---- lib/vectordb/sync-helpers.ts:86-118 (scanTextFileLimits) ----
function scanTextFileLimits(content, maxFileLines, maxLineLength) {
  let lineCount = 1;
  let cur = 0;
  for (let i = 0; i < content.length; i += 1) {
    const c = content.charCodeAt(i);
    if (c === 10) {
      lineCount += 1;
      if (lineCount > maxFileLines) return { lineCount };
      cur = 0;
      continue;
    }
    if (c === 13) continue;
    cur += 1;
    if (cur > maxLineLength) return { lineCount, tooLongLineLength: cur };
  }
  return { lineCount };
}

// ---- lib/documents/parser.ts:264-275 (ensureNonEmptyExtraction) + 533-541 (normalizeMarkdown) ----
const ensureNonEmpty = (t) => t.replace(/\r\n/g, "\n").trim();
function normalizeMarkdown(md) {
  let t = md.replace(/^---[\s\S]*?---\s*/u, "");
  t = t.replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1");
  t = t.replace(/\[([^\]]+)\]\([^)]*\)/g, "$1");
  t = t.replace(/```[\s\S]*?```/g, "");
  t = t.replace(/`([^`]+)`/g, "$1");
  t = t.replace(/\n{3,}/g, "\n\n");
  return t.trim();
}
function extractText(raw, file) {
  const ext = path.extname(file).toLowerCase();
  if (ext === ".md" || ext === ".markdown") return ensureNonEmpty(normalizeMarkdown(raw));
  return ensureNonEmpty(raw); // text/typescript, text/javascript -> plain-text extractor
}

// ---- lib/documents/v2/token-chunking.ts (chunkByTokens, buildLineStartIndex, findLineNumber) ----
function buildLineStartIndex(text) {
  const s = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === "\n") s.push(i + 1);
  return s;
}
function findLineNumber(ls, off) {
  let lo = 0;
  let hi = ls.length - 1;
  while (lo < hi) {
    const mid = Math.floor((lo + hi + 1) / 2);
    if (ls[mid] <= off) lo = mid;
    else hi = mid - 1;
  }
  return lo + 1;
}
function chunkByTokens(text, windowTokens, strideTokens) {
  if (!text.trim()) return [];
  const tokens = encode(text);
  const chunks = [];
  const ls = buildLineStartIndex(text);
  let idx = 0;
  for (let ts = 0; ts < tokens.length; ts += strideTokens) {
    const te = Math.min(ts + windowTokens, tokens.length);
    const chunkText = decode(tokens.slice(ts, te));
    const startChar = decode(tokens.slice(0, ts)).length;
    const endChar = decode(tokens.slice(0, te)).length;
    chunks.push({
      index: idx++,
      text: chunkText,
      startLine: findLineNumber(ls, startChar),
      endLine: findLineNumber(ls, endChar),
    });
    if (te >= tokens.length) break;
  }
  return chunks;
}

// ---- lib/vectordb/v2/lexical-vectors.ts ----
const LEX_DIM = 4096;
const STOP_WORDS = new Set([
  "the", "a", "an", "is", "are", "was", "were", "be", "been",
  "being", "have", "has", "had", "do", "does", "did", "will",
  "would", "could", "should", "may", "might", "must", "shall",
  "can", "need", "dare", "ought", "used", "to", "of", "in",
  "for", "on", "with", "at", "by", "from", "as", "into",
  "through", "during", "before", "after", "above", "below",
  "between", "under", "again", "further", "then", "once",
  "if", "or", "and", "but", "not", "so", "than", "too",
  "very", "just", "only", "own", "same", "that", "this",
  "function", "return", "const", "let", "var", "import", "export",
  "from", "default", "class", "interface", "type", "extends",
]);
export function tokenizeForLex(text) {
  return text
    .replace(/([a-z])([A-Z])/g, "$1 $2")
    .split(/[_\-\s\.\/\\:;,(){}[\]<>"'`=+*&|!?@#$%^~]+/)
    .map((token) => token.toLowerCase().trim())
    .filter((token) => token.length > 1 && !STOP_WORDS.has(token));
}
function hashString(str) {
  let hash = 5381;
  for (let i = 0; i < str.length; i++) {
    hash = ((hash << 5) + hash) ^ str.charCodeAt(i);
  }
  return Math.abs(hash);
}
/** Sparse form of generateLexicalVector: {bucket: value}, L2-normalised exactly as Selene does. */
export function lexicalVectorSparse(text) {
  const vec = new Map();
  for (const t of tokenizeForLex(text)) {
    const b = hashString(t) % LEX_DIM;
    vec.set(b, (vec.get(b) ?? 0) + 1.0);
  }
  let ss = 0;
  for (const v of vec.values()) ss += v * v;
  const norm = Math.sqrt(ss);
  const out = {};
  for (const [b, v] of vec) out[b] = norm > 0 ? v / norm : v;
  return out;
}

// ---- lib/ai/local-embeddings.ts:207-211 (applyQueryPrefix; applied to EVERY doEmbed input) ----
const applyQueryPrefix = (t) =>
  t.length > V2.queryPrefixMaxChars ? t : `${V2.queryPrefix} ${t}`.trim();

// ---- lib/vectordb/v2/query-expansion.ts (expandQuery / getCodeExpansions) ----
function getCodeExpansions(query) {
  const expansions = [];
  const lowerQuery = query.toLowerCase();
  const synonyms = {
    function: ["method", "fn", "func"],
    class: ["type", "interface", "struct"],
    get: ["fetch", "retrieve", "read", "load"],
    set: ["update", "write", "save", "store"],
    delete: ["remove", "destroy", "drop"],
    create: ["new", "add", "insert", "make"],
    user: ["account", "member", "profile"],
    auth: ["authentication", "login", "signin"],
    error: ["exception", "failure", "issue"],
    config: ["configuration", "settings", "options"],
  };
  for (const [key, values] of Object.entries(synonyms)) {
    if (lowerQuery.includes(key)) {
      expansions.push(...values.map((value) => query.replace(new RegExp(key, "gi"), value)));
    }
  }
  return expansions.slice(0, 3);
}
const expandQuery = (q) => Array.from(new Set([q, ...getCodeExpansions(q)]));

// Map parsed-text line numbers back to raw-file lines (needed for engine-agnostic chunk labels).
function lineMapper(raw, file) {
  const lf = raw.replace(/\r\n/g, "\n");
  const ext = path.extname(file).toLowerCase();
  if (ext !== ".md" && ext !== ".markdown") {
    const lead = lf.length - lf.trimStart().length;
    const off = (lf.slice(0, lead).match(/\n/g) ?? []).length;
    return (chunk) => [chunk.startLine + off, chunk.endLine + off];
  }
  // Markdown text is rewritten by normalizeMarkdown: locate the chunk's longest line in the raw file.
  const rawLines = lf.split("\n");
  let cursor = 0;
  return (chunk) => {
    const lines = chunk.text.split("\n");
    let best = 0;
    lines.forEach((l, i) => {
      if (l.trim().length > lines[best].trim().length) best = i;
    });
    const needle = lines[best].trim().slice(0, 60);
    let found = -1;
    if (needle.length >= 8) {
      for (let i = Math.max(0, cursor - 40); i < rawLines.length; i++) {
        if (rawLines[i].includes(needle)) { found = i; break; }
      }
      if (found < 0) {
        for (let i = 0; i < rawLines.length; i++) {
          if (rawLines[i].includes(needle)) { found = i; break; }
        }
      }
    }
    if (found < 0) return [chunk.startLine, chunk.endLine]; // fall back to parsed-text lines
    cursor = found;
    const start = Math.max(1, found + 1 - best);
    return [start, start + (chunk.endLine - chunk.startLine)];
  };
}

async function readJsonl(p) {
  const out = [];
  const rl = readline.createInterface({ input: fs.createReadStream(p, "utf8"), crlfDelay: Infinity });
  for await (const line of rl) if (line.trim()) out.push(JSON.parse(line));
  return out;
}

async function corpus(input, root, outPath, rawText) {
  let docs;
  if (input.endsWith(".json")) {
    const man = JSON.parse(fs.readFileSync(input, "utf8"));
    docs = man.files.map((f) => ({ id: f.path, text: fs.readFileSync(path.join(root, f.path), "utf8") }));
  } else {
    docs = (await readJsonl(input)).map((d) => ({
      id: d._id,
      text: d.title ? `${d.title}\n\n${d.text}` : d.text,
    }));
  }
  const out = fs.createWriteStream(outPath, "utf8");
  const stats = {
    docs: docs.length, indexed: 0, skipped_max_file_lines: 0, skipped_max_line_length: 0,
    skipped_files: [], empty: 0, chunks: 0, config: V2, raw_text_path: rawText,
  };
  for (const d of docs) {
    if (!rawText) {
      const s = scanTextFileLimits(d.text, V2.maxFileLines, V2.maxLineLength);
      if (s.lineCount > V2.maxFileLines) { stats.skipped_max_file_lines++; stats.skipped_files.push(d.id); continue; }
      if (typeof s.tooLongLineLength === "number") { stats.skipped_max_line_length++; stats.skipped_files.push(d.id); continue; }
    }
    const parsed = rawText ? ensureNonEmpty(d.text) : extractText(d.text, d.id);
    if (!parsed) { stats.empty++; continue; }
    const chunks = chunkByTokens(parsed, V2.tokenChunkSize, V2.tokenChunkStride);
    const map = rawText ? (c) => [c.startLine, c.endLine] : lineMapper(d.text, d.id);
    stats.indexed++;
    for (const c of chunks) {
      const [s, e] = map(c);
      out.write(JSON.stringify({
        id: `${d.id}#${c.index}`, doc: d.id, chunk_index: c.index, start_line: s, end_line: e,
        text: c.text, embed_input: applyQueryPrefix(c.text), lex: lexicalVectorSparse(c.text),
      }) + "\n");
      stats.chunks++;
    }
  }
  await new Promise((r) => out.end(r));
  fs.writeFileSync(outPath.replace(/\.jsonl$/, ".stats.json"), JSON.stringify(stats, null, 1) + "\n");
  console.log(JSON.stringify({ ...stats, skipped_files: stats.skipped_files.length, config: undefined }));
}

async function queries(input, outPath) {
  const qs = await readJsonl(input);
  const out = qs.map((q) => ({
    qid: q.qid,
    query: q.query,
    expanded: expandQuery(q.query).map((t) => ({ text: t, embed_input: applyQueryPrefix(t), lex: lexicalVectorSparse(t) })),
  }));
  fs.writeFileSync(outPath, out.map((o) => JSON.stringify(o)).join("\n") + "\n");
  console.log(`${out.length} queries, ${out.filter((o) => o.expanded.length > 1).length} expanded`);
}

const [mode, ...rest] = process.argv.slice(2);
if (mode === "corpus") await corpus(rest[0], rest[1], rest[2], rest.includes("--raw-text"));
else if (mode === "queries") await queries(rest[0], rest[1]);
else {
  console.error("usage: v2_prep.mjs corpus|queries ...");
  process.exit(2);
}
