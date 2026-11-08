# cqadupstack-webmasters

Public mixed-document corpus for the retrieval eval: web-development and SEO questions from the Webmasters StackExchange, with duplicate-question relevance labels.

- **Source:** [`mteb/cqadupstack-webmasters`](https://huggingface.co/datasets/mteb/cqadupstack-webmasters) at revision `c691d4af16c19a79df7ee93bfbd12cc186a5bc92`. This is the BEIR packaging of CQADupStack (Hoogeveen, Verspoor and Baldwin, ADCS 2015) as used by Thakur et al., *BEIR*, NeurIPS 2021 Datasets and Benchmarks. The sha256 of each file is in `manifest.json`.
- **Licence:** BEIR and MTEB distribute it under **Apache-2.0**. The underlying text is StackExchange user content under **CC BY-SA 3.0** (posts written before April 2011: CC BY-SA 2.5; [license](https://creativecommons.org/licenses/by-sa/3.0/); attribution: the original authors on webmasters.stackexchange.com). Redistribution is allowed with attribution and share-alike for the text.
- **What is committed here:** only the manifest, plus the derived golden file `eval/golden/cqadupstack-webmasters.jsonl` (query text + qrels, with attribution above). The corpus itself (13 MB) is downloaded into `eval/.cache/` and never committed.
- **Why this corpus:** a small, permissively licensed, real-user mixed prose and code corpus. Alternatives were rejected:
  - NFCorpus: the upstream site restricts non-academic use of the NutritionFacts.org text.
  - SciFact: CC BY-NC.
  - ArguAna: queries are whole arguments longer than 512 characters, not search queries.
- **Ingestion:** each document is `title + "\n\n" + text` and goes through Selene's raw-text path (`indexTextToVectorDB`). Many bodies are single lines longer than 1000 characters, which Selene's file-sync line check would otherwise reject.
- **Antivirus (Windows Defender):** a few posts quote malicious code from hacked sites (for example `eval(base64_decode(...))`, Blackhole redirect scripts, `.htaccess` hijacks, PHP web shells). The text is inert, but `sr-bench-engine --corpus-jsonl` writes each document as `<index>\corpus-files\<id>.txt`, so Defender may report "Severe threat" and quarantine some of them (seen: 17017, 28868, 45289, 49478). The bench checks every document file before and after indexing and fails with the missing ids instead of reporting understated recall. Add a Defender exclusion for the bench index directory (the `--index` folder, e.g. `Add-MpPreference -ExclusionPath <index>` in an elevated PowerShell), then rerun with `--rebuild`.
