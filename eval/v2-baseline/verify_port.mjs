// Fidelity check: run Selene's *own* TypeScript functions (imported read-only from the Selene repo
// with Node's built-in type stripping; nothing is modified, no app/server/database is started)
// on the same inputs as the offline port, and diff the outputs.
//
//   node --disable-warning=ExperimentalWarning verify_port.mjs <corpus> [candidates.jsonl]
//
// Checks: chunkByTokens (lib/documents/v2/token-chunking.ts) chunk texts + line numbers,
// generateLexicalVector (lib/vectordb/v2/lexical-vectors.ts) on every chunk and query,
// expandQuery (lib/vectordb/v2/query-expansion.ts) on every golden query, and, given a candidate
// dump from v2_search.py, rrfFusion + sortByFusedScore (lib/ai/vector-search/v2/fusion.ts) plus
// hybridSearchV2's empty-arm fallbacks, which must reproduce the Python top-50 exactly.
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { registerHooks } from "node:module";
import readline from "node:readline";

const EVAL = path.resolve(import.meta.dirname, "..");
const SELENE = process.env.SELENE_ROOT ?? path.resolve(EVAL, "../../selene"); // Selene checkout next to this repository
const corpus = process.argv[2] ?? "selene-src";
const candPath = process.argv[3];

// Selene's TS uses extensionless relative imports ("./tokenizer-runtime"): resolve them to .ts.
registerHooks({
  resolve(specifier, context, next) {
    if ((specifier.startsWith("./") || specifier.startsWith("../")) && !path.extname(specifier)) {
      try {
        return next(`${specifier}.ts`, context);
      } catch {
        /* fall through */
      }
    }
    return next(specifier, context);
  },
});
process.chdir(SELENE); // tokenizer-runtime.ts resolves gpt-tokenizer relative to process.cwd()
const imp = (rel) => import(pathToFileURL(path.join(SELENE, rel)).href);
const { chunkByTokens } = await imp("lib/documents/v2/token-chunking.ts");
const { generateLexicalVector } = await imp("lib/vectordb/v2/lexical-vectors.ts");
const { expandQuery } = await imp("lib/vectordb/v2/query-expansion.ts");
const { rrfFusion, sortByFusedScore } = await imp("lib/ai/vector-search/v2/fusion.ts");

async function* jsonl(p) {
  const rl = readline.createInterface({ input: fs.createReadStream(p, "utf8"), crlfDelay: Infinity });
  for await (const line of rl) if (line.trim()) yield JSON.parse(line);
}
const work = path.join(EVAL, ".cache", "v2", corpus);
let failures = 0;
const fail = (msg) => {
  if (failures++ < 20) console.error("MISMATCH", msg);
};

// --- chunks + lexical vectors -------------------------------------------------------------
const byDoc = new Map();
for await (const c of jsonl(path.join(work, "chunks.jsonl"))) {
  if (!byDoc.has(c.doc)) byDoc.set(c.doc, []);
  byDoc.get(c.doc).push(c);
}
const lexEq = (dense, sparse) => {
  const nz = Object.keys(sparse).length;
  let count = 0;
  for (let i = 0; i < dense.length; i++) {
    const want = sparse[i] ?? 0;
    if (Math.abs(dense[i] - want) > 1e-12) return false;
    if (dense[i] !== 0) count++;
  }
  return count === nz;
};
let nChunks = 0;
let nDocsChunked = 0;
const rawText = corpus !== "selene-src";
const docText = new Map();
if (rawText) {
  for await (const d of jsonl(path.join(EVAL, ".cache", corpus, "corpus.jsonl"))) {
    if (byDoc.has(d._id)) docText.set(d._id, d.title ? `${d.title}\n\n${d.text}` : d.text);
  }
}
for (const [doc, chunks] of byDoc) {
  let parsed;
  if (rawText) parsed = docText.get(doc).replace(/\r\n/g, "\n").trim();
  else {
    const raw = fs.readFileSync(path.join(EVAL, ".cache", "corpora", "selene-src", doc), "utf8");
    parsed = raw.replace(/\r\n/g, "\n").trim();
    if (/\.(md|markdown)$/i.test(doc)) parsed = null; // normalizeMarkdown is private in parser.ts
  }
  for (const c of chunks) {
    nChunks++;
    if (!lexEq(generateLexicalVector(c.text), c.lex)) fail(`lexical vector ${c.id}`);
  }
  if (parsed === null) continue;
  nDocsChunked++;
  const ref = chunkByTokens(parsed, { windowTokens: 96, strideTokens: 48 });
  if (ref.length !== chunks.length) {
    fail(`${doc}: ${ref.length} chunks (Selene) vs ${chunks.length} (port)`);
    continue;
  }
  const raw = rawText ? parsed : fs.readFileSync(path.join(EVAL, ".cache", "corpora", "selene-src", doc), "utf8").replace(/\r\n/g, "\n");
  const off = (raw.slice(0, raw.length - raw.trimStart().length).match(/\n/g) ?? []).length;
  ref.forEach((r, i) => {
    const p = chunks[i];
    if (r.text !== p.text) fail(`${p.id}: chunk text differs`);
    if (r.startLine + off !== p.start_line || r.endLine + off !== p.end_line) fail(`${p.id}: lines ${r.startLine}-${r.endLine}(+${off}) vs ${p.start_line}-${p.end_line}`);
  });
}
console.log(`chunks: ${nDocsChunked} docs re-chunked with Selene's chunkByTokens; ${nChunks} lexical vectors checked`);

// --- query expansion + query lexical vectors ----------------------------------------------
let nq = 0;
for await (const q of jsonl(path.join(work, "queries.prepped.jsonl"))) {
  nq++;
  const ref = await expandQuery(q.query);
  const mine = q.expanded.map((e) => e.text);
  if (JSON.stringify(ref) !== JSON.stringify(mine)) fail(`${q.qid}: expansion ${JSON.stringify(ref)} vs ${JSON.stringify(mine)}`);
  for (const e of q.expanded) if (!lexEq(generateLexicalVector(e.text), e.lex)) fail(`${q.qid}: query lexical vector`);
}
console.log(`queries: ${nq} expansions + query lexical vectors checked`);

// --- fusion --------------------------------------------------------------------------------
if (candPath) {
  let nf = 0;
  for await (const c of jsonl(candPath)) {
    nf++;
    const dense = c.dense.map(([id, score]) => ({ id, score }));
    const lex = c.lex.map(([id, score]) => ({ id, score }));
    let ids;
    // lib/vectordb/v2/hybrid-search.ts:58-81
    if (dense.length === 0) ids = lex.slice(0, 50).map((h) => h.id);
    else if (lex.length === 0) ids = dense.slice(0, 50).map((h) => h.id);
    else {
      const fused = rrfFusion(
        dense.map((h, i) => ({ id: h.id, rank: i, score: h.score, source: "dense" })),
        lex.map((h, i) => ({ id: h.id, rank: i, score: h.score, source: "lexical" })),
        { k: 50, denseWeight: 1.0, lexicalWeight: 2.0 },
      );
      const hitMap = new Map();
      [...dense, ...lex].forEach((h) => { if (!hitMap.has(h.id)) hitMap.set(h.id, h); });
      ids = sortByFusedScore(fused, hitMap, 50).map((h) => h.id);
    }
    if (JSON.stringify(ids) !== JSON.stringify(c.fused)) fail(`${c.qid}: fused top-50 differs`);
  }
  console.log(`fusion: ${nf} queries re-fused with Selene's rrfFusion/sortByFusedScore`);
}

console.log(failures === 0 ? "PORT VERIFIED: 0 mismatches" : `${failures} MISMATCHES`);
process.exit(failures === 0 ? 0 : 1);
