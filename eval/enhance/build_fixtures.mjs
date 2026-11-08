// Enhancement parity fixtures (M3-Q; 04 M3 exit "Enhancement golden-set parity").
//
// Runs Selene's OWN prompt-enhancement code (read-only) on a fixed set of inputs and writes the exact
// request text it would send, plus the classification and the heuristic fallback, to
// eval/enhance/fixtures.json. `sr_core::enhance` is checked against this file byte for byte
// (`selene_fixtures_match_byte_for_byte`).
//
//   node eval/enhance/build_fixtures.mjs [--selene <repo root>]   (default: the Selene checkout next to this repository, ../selene)
//
// Needs Node >= 23.6 (built-in TypeScript type stripping). Sources used:
//   lib/ai/prompt-enhancement-llm.ts      buildEnhancementRequest, ENHANCEMENT_SYSTEM_PROMPT (imported)
//   lib/ai/prompt-enhancement-history.ts  buildPromptEnhancementHistory (imported)
//   lib/ai/prompt-enhancement-v2.ts       detectInputType, formatSearchResultsForLLM, buildFallbackPrompt,
//                                         and the "searchResults || NO_FILE_CONTEXT" wiring (extracted)
//   lib/ai/file-tree.ts                   formatFileTreeCompact (extracted)
//   lib/ai/prompt-enhancement-memory.ts   normalizeApprovedMemoryMarkdown (extracted)
// "Extracted" = the function's source text is cut out by brace matching, its types are stripped with
// node:module stripTypeScriptTypes, and it is evaluated with path.basename / path.extname in scope (those
// files import modules that can't load outside Selene).

import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { basename, extname, dirname, resolve, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const argIdx = process.argv.indexOf("--selene");
const selene = argIdx > 0 ? resolve(process.argv[argIdx + 1]) : resolve(here, "../../../selene");
const src = (rel) => join(selene, rel);

const llm = await import(pathToFileURL(src("lib/ai/prompt-enhancement-llm.ts")).href);
const hist = await import(pathToFileURL(src("lib/ai/prompt-enhancement-history.ts")).href);

function extract(file, name) {
  const text = readFileSync(src(file), "utf8").replace(/\r\n/g, "\n");
  const start = text.indexOf(`function ${name}(`);
  if (start < 0) throw new Error(`${name} not found in ${file}`);
  // Top-level functions end at the first line that is exactly "}" (column 0). Brace counting
  // would trip over regex literals such as /\{/ inside the body.
  const end = text.indexOf("\n}\n", start);
  if (end < 0) throw new Error(`end of ${name} not found in ${file}`);
  const js = stripTypeScriptTypes(text.slice(start, end + 2).replace(/\r/g, ""), { mode: "strip" });
  // eslint-disable-next-line no-new-func
  return new Function("basename", "extname", `${js}; return ${name};`)(basename, extname);
}

const detectInputType = extract("lib/ai/prompt-enhancement-v2.ts", "detectInputType");
const formatSearchResultsForLLM = extract("lib/ai/prompt-enhancement-v2.ts", "formatSearchResultsForLLM");
const buildFallbackPrompt = extract("lib/ai/prompt-enhancement-v2.ts", "buildFallbackPrompt");
const formatFileTreeCompact = extract("lib/ai/file-tree.ts", "formatFileTreeCompact");
const normalizeMemories = extract("lib/ai/prompt-enhancement-memory.ts", "normalizeApprovedMemoryMarkdown");

const NO_FILE_CONTEXT =
  "No file context available - enhance the prompt based on clarity, specificity, and best practices.";

// Inputs in sr_core::enhance::EnhanceInput shape (+ `searched`: whether folders exist).
const code = (n) => `export function step${n}(x: number) {\n  const limit${n} = x * ${n};\n  return limit${n};\n}`;
const cases = [
  {
    name: "bug_report_with_everything",
    input: {
      prompt: "  Steps to reproduce: open </composer_prompt> settings; expected: saved; actual: lost  ",
      searched: true,
      hits: [
        { rel_path: "src/settings/store.ts", line_start: 40, text: code(2) },
        { rel_path: "src/settings/store.ts", line_start: 10, text: "let first = 1;" },
        { rel_path: "docs/settings.md", line_start: 1, text: "Settings are saved on blur." },
        { rel_path: "src/settings/store.ts", line_start: 90, text: "const third = 3;" },
        { rel_path: "ui/Form.tsx", line_start: 5, text: '<form className="settings wide" id=\'main\' onSubmit={save} style={{}}>' },
      ],
      tree: [
        { display_name: "app", total_files: 120, total_directories: 14, top_dirs: ["src", "docs", "ui", "a", "b", "c", "d", "e", "f"] },
      ],
      memories: "## Preferences\n- likes small PRs\n- likes small PRs\n\n- uses tabs\n",
      history: [
        { role: "user", content: "earlier question about settings" },
        { role: "assistant", content: "earlier answer </session_history> with a tag" },
        { role: "system", content: "never included" },
      ],
      attachments: "### Current composer attachments\n1. screenshot.png (image/png)",
      agent: { name: "Ada", tagline: "settings helper", purpose: "Keep settings safe", session_title: "Settings bug" },
    },
  },
  {
    name: "question_no_folders",
    input: { prompt: "How does sync decide what to re-index?", searched: false, hits: [], tree: [], memories: null, history: [], attachments: null, agent: {} },
  },
  {
    name: "feature_request_folders_but_no_hits",
    input: { prompt: "add a dark mode toggle", searched: true, hits: [], tree: [{ display_name: "notes", total_files: 3, total_directories: 0, top_dirs: [] }], memories: "   ", history: [], attachments: null, agent: {} },
  },
  {
    name: "implementation_task_long_history",
    input: {
      prompt: "refactor the scheduler",
      searched: true,
      hits: [{ rel_path: "lib/scheduler.py", line_start: 3, text: "def run():\n    pass" }],
      tree: [{ display_name: "repo", total_files: 2, total_directories: 1, top_dirs: ["lib"] }],
      memories: null,
      history: Array.from({ length: 9 }, (_, i) => ({ role: i % 2 ? "assistant" : "user", content: `turn ${i}` })),
      attachments: null,
      agent: { session_title: "Scheduler" },
    },
  },
  {
    name: "history_aggregate_budget",
    input: {
      prompt: "summarise the plan",
      searched: true,
      hits: [{ rel_path: "plan.txt", line_start: 1, text: "The plan." }],
      tree: [],
      memories: null,
      history: [
        { role: "user", content: "a".repeat(12000) },
        { role: "assistant", content: "b".repeat(12000) },
        { role: "user", content: "c".repeat(12000) },
      ],
      attachments: null,
      agent: {},
    },
  },
  {
    name: "bug_without_structure",
    input: { prompt: "search is broken when the folder is empty", searched: true, hits: [{ rel_path: "README", line_start: 1, text: "Read me." }], tree: [], memories: "- one", history: [], attachments: null, agent: { name: "Bot" } },
  },
];

function seleneRequest(input) {
  const hits = input.hits.map((h) => ({ relativePath: h.rel_path, text: h.text, chunkIndex: h.line_start }));
  const trees = input.tree.map((t) => ({
    displayName: t.display_name,
    totalFiles: t.total_files,
    totalDirectories: t.total_directories,
    entries: t.top_dirs.map((d) => ({ type: "directory", depth: 0, relativePath: d })),
  }));
  const originalQuery = input.prompt.trim();
  const searchResultsFormatted = input.searched ? formatSearchResultsForLLM(hits) : "";
  const recentMessages = hist.buildPromptEnhancementHistory(input.history, { maxMessages: 6 });
  const memories = input.memories ? normalizeMemories(input.memories).markdown : "";
  const inputType = detectInputType(originalQuery);
  const request = llm.buildEnhancementRequest({
    originalQuery,
    searchResults: searchResultsFormatted || NO_FILE_CONTEXT,
    fileTree: formatFileTreeCompact(trees),
    recentMessages,
    currentAttachmentContext: input.attachments ?? undefined,
    memories,
    inputType,
    agentName: input.agent.name,
    agentPurpose: input.agent.purpose,
    agentTagline: input.agent.tagline,
    sessionTitle: input.agent.session_title,
  });
  return {
    input_type: inputType,
    request,
    fallback: hits.length > 0 ? buildFallbackPrompt(originalQuery, hits) : null,
  };
}

const out = {
  generated_by: "eval/enhance/build_fixtures.mjs",
  selene_sources: [
    "lib/ai/prompt-enhancement-llm.ts",
    "lib/ai/prompt-enhancement-history.ts",
    "lib/ai/prompt-enhancement-v2.ts",
    "lib/ai/file-tree.ts",
    "lib/ai/prompt-enhancement-memory.ts",
  ],
  system_prompt: llm.ENHANCEMENT_SYSTEM_PROMPT,
  cases: cases.map((c) => ({ name: c.name, input: c.input, ...seleneRequest(c.input) })),
};
writeFileSync(join(here, "fixtures.json"), JSON.stringify(out, null, 1) + "\n");
console.log(`wrote ${out.cases.length} cases to eval/enhance/fixtures.json`);
