// Embedding parity: Selene's runtime (Transformers.js from Selene's node_modules, CPU, the
// `{ pooling: "mean", normalize: true }` call of lib/ai/local-embeddings.ts) against our ORT/Python
// embeddings, on the first N chunk inputs of a corpus. Offline only (allowRemoteModels = false);
// the model is read from eval/.cache/tjs-models/<org>/<name>/ (hard links to eval/.cache/models).
//
//   node verify_embed.mjs <corpus> <emb-tag> <org/model> [n=256]
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import readline from "node:readline";

const EVAL = path.resolve(import.meta.dirname, "..");
const SELENE = process.env.SELENE_ROOT ?? path.resolve(EVAL, "../../selene"); // Selene checkout next to this repository
const [corpus = "selene-src", tag = "small-fp32", model = "Xenova/bge-small-en-v1.5", nArg = "256"] = process.argv.slice(2);
const N = Number(nArg);

const require = createRequire(path.join(SELENE, "package.json"));
const tjs = await import(pathToFileURL(require.resolve("@huggingface/transformers")).href);
tjs.env.allowRemoteModels = false;
tjs.env.localModelPath = path.join(EVAL, ".cache", "tjs-models") + path.sep;

const inputs = [];
const rl = readline.createInterface({ input: fs.createReadStream(path.join(EVAL, ".cache", "v2", corpus, "chunks.jsonl"), "utf8") });
for await (const line of rl) {
  if (inputs.length >= N) break;
  inputs.push(JSON.parse(line).embed_input);
}
rl.close();

// minimal .npy (float32, C order) reader for the first N rows
function readNpyRows(p, rows) {
  const buf = fs.readFileSync(p);
  const hlen = buf.readUInt16LE(8);
  const header = buf.subarray(10, 10 + hlen).toString("latin1");
  const shape = header.match(/'shape': \((\d+), (\d+)\)/);
  if (!header.includes("'<f4'") || !shape) throw new Error(`unexpected npy header ${header}`);
  const dim = Number(shape[2]);
  const data = new Float32Array(buf.buffer.slice(buf.byteOffset + 10 + hlen, buf.byteOffset + 10 + hlen + rows * dim * 4));
  return { dim, data };
}

const extractor = await tjs.pipeline("feature-extraction", model, { dtype: "fp32", device: "cpu" });
const { dim, data } = readNpyRows(path.join(EVAL, ".cache", "v2", corpus, `emb-${tag}.npy`), inputs.length);
let minCos = 1;
let sumCos = 0;
for (let s = 0; s < inputs.length; s += 16) {
  const batch = inputs.slice(s, s + 16);
  const out = await extractor(batch, { pooling: "mean", normalize: true });
  const v = out.data;
  for (let b = 0; b < batch.length; b++) {
    let dot = 0;
    for (let d = 0; d < dim; d++) dot += v[b * dim + d] * data[(s + b) * dim + d];
    minCos = Math.min(minCos, dot);
    sumCos += dot;
  }
}
console.log(`${model} (${tag}) on ${inputs.length} ${corpus} chunks: Transformers.js vs ORT cosine min ${minCos.toFixed(7)}, mean ${(sumCos / inputs.length).toFixed(7)}`);
process.exit(minCos > 0.999 ? 0 : 1);
