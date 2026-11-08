# Spikes

These are early experiments. They tested the native stack, the document extractor sandbox, the IPC bridge and the GPU tiers before the real crates existed. The workspace and CI do not build them, and they are not maintained. They are kept, with their results, as evidence for the decisions recorded in [docs/00-review-and-decisions.md](../docs/00-review-and-decisions.md).

- [native-stack/](native-stack/): LanceDB, Tantivy, ONNX Runtime embeddings and SQLite linked into one Windows exe, with the reference embeddings in `test11/`.
- [extract-sandbox/](extract-sandbox/): parsing documents in a sandboxed worker process, plus `msvc-env.sh`, an optional helper for a locally downloaded MSVC toolset.
- [ipc-bridge/](ipc-bridge/): MCP between a small bridge exe and a background service over a named pipe that only the current user can open.
- [gpu-tier/](gpu-tier/): hardware detection, DirectML versus CPU speed and correctness, and a simulated re-embed.
