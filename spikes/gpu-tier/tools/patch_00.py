"""One-off doc patch for docs/00-review-and-decisions.md (M0-E amendments). Idempotent guard via assert."""
import pathlib, sys

p = pathlib.Path(sys.argv[1])
s = p.read_text(encoding="utf-8")
assert "## Amendments (approved" not in s, "already patched"


def rep(a, b):
    global s
    assert a in s, a[:70]
    s = s.replace(a, b, 1)


rep("11. **DirectML is dropped from v1.** It is in maintenance mode and needs DLL loading (M8). That closes R-UX7.",
    "11. **DirectML is dropped from v1.** It is in maintenance mode and needs DLL loading (M8). That closes R-UX7. *(Partially reversed by **Amendment 2**: the GPU is used by default.)*")
rep("12. **Default model.** bge-small-en-v1.5 (int8, 384 dims) ships inside the installer.",
    "12. **Default model.** bge-small-en-v1.5 (int8, 384 dims) ships inside the installer. *(Amended by **Amendment 1**: it is now the Minimum tier; Standard and High download on demand.)*")
rep("full ErrorCode → UserMessage table (36 codes)", "full ErrorCode → UserMessage table (37 codes; #37 `ModelIncompatible` added by Amendment 1)")
rep("and the 36-row ErrorCode table.", "and the 37-row ErrorCode table.")
rep("| U10 | DirectML opt-in | Unfindable; DLL risk | Removed from v1. Windows ML evaluated after v1. | — |",
    "| U10 | DirectML opt-in | Unfindable; DLL risk | **Amendment 2:** no opt-in at all. The GPU is used by default when it helps, with a silent CPU fallback. Advanced has only an off switch. DLL risk handled by absolute-path loads plus a signature check. | M2 |")
rep("| fastembed / ort | **Corrected.** ORT linked statically, so no DLL. CPU only, no DirectML.",
    "| fastembed / ort | **Corrected, then amended (Amendment 2).** ORT is linked statically where possible; `DirectML.dll` (and `onnxruntime.dll` if static linking fails) ship app-local, so the user still installs nothing. The GPU is on by default.")
rep("3. **Default model:** bge-small-en-v1.5 int8 (384 dims), bundled, with a one-click multilingual-e5-small switch.",
    "3. **Default model:** bge-small-en-v1.5 int8 (384 dims), bundled, with a one-click multilingual-e5-small switch. → **Amended (Amendment 1, approved):** three hardware tiers.")
rep("5. **Dropping DirectML** from v1 and **dropping figment** for one app-written settings file.",
    "5. **Dropping DirectML** from v1 and **dropping figment** for one app-written settings file. → **DirectML part reversed (Amendment 2, approved):** GPU on by default. Dropping figment stands.")

AMEND = (pathlib.Path(__file__).with_name("amendments_00.md")).read_text(encoding="utf-8")
rep("### Flagged as unverified (resolved in M0 unless noted)", AMEND.rstrip("\n") + "\n\n### Flagged as unverified (resolved in M0 unless noted)")
p.write_text(s, encoding="utf-8")
print("patched")
