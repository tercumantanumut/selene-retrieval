# Test data

## `bge-small-tokenizer.json`

The tokenizer of the bundled search model, used by the chunking tests (`crates/core/src/ingest/chunk.rs`), the engine tests and the MCP HTTP tests.

- **Source:** `tokenizer.json` from [`Qdrant/bge-small-en-v1.5-onnx-Q`](https://huggingface.co/Qdrant/bge-small-en-v1.5-onnx-Q) at revision `aa8f8b060edb00e03bfdd08813a2949946c8ba55`, the revision pinned in `crates/providers/src/models.rs`. The file is unchanged.
- **sha256:** `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66`
- **Upstream model:** [`BAAI/bge-small-en-v1.5`](https://huggingface.co/BAAI/bge-small-en-v1.5) ([FlagEmbedding](https://github.com/FlagOpen/FlagEmbedding)).
- **License:** MIT License, Copyright (c) 2022 staoxiao. The full text follows.

```text
MIT License

Copyright (c) 2022 staoxiao

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
