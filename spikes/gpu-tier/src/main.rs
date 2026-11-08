//! M0-E spike.
//!   gpu-tier-spike detect
//!   gpu-tier-spike bench <model_dir> <variant.onnx> <cpu|dml> [n_chunks] [batches=16,32,64]
//!   gpu-tier-spike fallback <model_dir> <variant.onnx>
//!   gpu-tier-spike compare <a.bin> <b.bin>
//!   gpu-tier-spike reembed-demo        (event stream of the simulated re-embed flow, JSON lines)
//!   gpu-tier-spike tiers               (the TierProfile table as JSON)

mod hw;
mod reembed;
mod tier;

use anyhow::{Context, Result, anyhow, bail};
use ort::ep;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{Tensor, TensorElementType, ValueType};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokenizers::Tokenizer;

const SEQ: usize = 512;
// Selene's lib/ in the Selene checkout next to this repository. Relative to spikes/gpu-tier, so
// run the spike from that folder.
const CORPUS_ROOT: &str = r"..\..\..\selene\lib";

fn main() -> Result<()> {
    // M8: never resolve DLLs from the current directory / PATH for LoadLibrary calls made after this point.
    unsafe {
        let _ = windows::Win32::System::LibraryLoader::SetDefaultDllDirectories(
            windows::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32
                | windows::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        );
    }
    let args: Vec<String> = std::env::args().collect();
    let needs_ort = matches!(args.get(1).map(String::as_str), Some("bench" | "fallback"));
    if needs_ort {
        // Absolute-path load (M8). DirectML.dll is resolved by onnxruntime.dll: app dir, then System32.
        let dll = std::env::current_exe()?.with_file_name("onnxruntime.dll");
        ort::init_from(&dll).map_err(|e| anyhow!("{e}"))?.commit();
    }
    match args.get(1).map(String::as_str) {
        Some("detect") => detect(),
        Some("bench") => {
            let n = args.get(5).map(|s| s.parse()).transpose()?.unwrap_or(1000usize);
            let batches: Vec<usize> = args
                .get(6)
                .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
                .unwrap_or(vec![16, 32, 64]);
            bench(Path::new(&args[2]), &args[3], &args[4], n, &batches)
        }
        Some("fallback") => fallback(Path::new(&args[2]), &args[3]),
        Some("compare") => compare(Path::new(&args[2]), Path::new(&args[3])),
        Some("reembed-demo") => reembed_demo(),
        Some("tiers") => {
            for t in [tier::Tier::Minimum, tier::Tier::Standard, tier::Tier::High] {
                println!("{}", serde_json::to_string(&tier::profile(t))?);
            }
            println!("{}", serde_json::to_string(&tier::Thresholds::default())?);
            Ok(())
        }
        _ => bail!("usage: detect | bench <dir> <file> <cpu|dml> [n] [batches] | fallback <dir> <file> | compare a b"),
    }
}

fn detect() -> Result<()> {
    let t0 = Instant::now();
    let data_dir = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("C:\\"));
    let h = hw::detect(&data_dir);
    let took = t0.elapsed();
    let th = tier::Thresholds::default();
    let d = tier::select_tier(&h, &th);
    println!("{}", serde_json::to_string_pretty(&h)?);
    println!("detect_ms={:.1}", took.as_secs_f64() * 1000.0);
    println!("decision={}", serde_json::to_string_pretty(&d)?);
    println!("profile={}", serde_json::to_string_pretty(&tier::profile(d.tier))?);
    Ok(())
}

// ---------------------------------------------------------------- corpus

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.path());
    for e in ents {
        let p = e.path();
        if p.is_dir() {
            if !p.ends_with("node_modules") {
                collect_files(&p, out);
            }
        } else if matches!(p.extension().and_then(|s| s.to_str()), Some("ts" | "tsx" | "md")) {
            out.push(p);
        }
    }
}

/// Some tokenizer.json files (e.g. Snowflake arctic) ship with truncation=512 / padding enabled.
/// The corpus builder needs whole-file token streams, so both are switched off.
fn load_tokenizer(dir: &Path) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow!("{e}"))?;
    tok.with_truncation(None).map_err(|e| anyhow!("{e}"))?;
    tok.with_padding(None);
    Ok(tok)
}

/// N chunks of exactly SEQ tokens ([CLS] + 510 + [SEP]) from Selene's own source (code + docs mix).
fn build_chunks(tok: &Tokenizer, n: usize) -> Result<Vec<Vec<i64>>> {
    let cls = tok.token_to_id("[CLS]").context("no [CLS]")? as i64;
    let sep = tok.token_to_id("[SEP]").context("no [SEP]")? as i64;
    let mut files = Vec::new();
    collect_files(Path::new(CORPUS_ROOT), &mut files);
    let mut chunks = Vec::with_capacity(n);
    let mut buf: Vec<i64> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let enc = tok.encode(text.as_str(), false).map_err(|e| anyhow!("{e}"))?;
        buf.extend(enc.get_ids().iter().map(|&x| x as i64));
        while buf.len() >= SEQ - 2 {
            let mut c = Vec::with_capacity(SEQ);
            c.push(cls);
            c.extend(buf.drain(..SEQ - 2));
            c.push(sep);
            chunks.push(c);
            if chunks.len() == n {
                return Ok(chunks);
            }
        }
    }
    bail!("corpus too small: {} chunks", chunks.len())
}

// ---------------------------------------------------------------- sessions

#[derive(Debug, Clone, Copy, PartialEq)]
enum EpChoice {
    Cpu,
    Dml(i32),
}

fn build_session(model: &Path, ep_choice: EpChoice) -> Result<Session> {
    let threads = hw::cores().0 as usize;
    let mut b = Session::builder()
        .map_err(|e| anyhow!("{e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow!("{e}"))?;
    match ep_choice {
        EpChoice::Cpu => {
            b = b
                .with_intra_threads(threads)
                .map_err(|e| anyhow!("{e}"))?
                .with_execution_providers([ep::CPU::default().build()])
                .map_err(|e| anyhow!("{e}"))?;
        }
        EpChoice::Dml(id) => {
            // DML EP requirements: no memory pattern, sequential execution.
            b = b
                .with_memory_pattern(false)
                .map_err(|e| anyhow!("{e}"))?
                .with_parallel_execution(false)
                .map_err(|e| anyhow!("{e}"))?
                .with_execution_providers([ep::DirectML::default().with_device_id(id).build().error_on_failure()])
                .map_err(|e| anyhow!("DirectML registration failed: {e}"))?;
        }
    }
    b.commit_from_file(model).map_err(|e| anyhow!("commit failed: {e}"))
}

/// The ladder sr-providers should implement: GPU EP (error_on_failure) -> commit -> canary run -> else CPU.
/// Never surfaces an error to the user when CPU works.
fn session_with_fallback(model: &Path, want: EpChoice, canary: &[Vec<i64>]) -> Result<(Session, EpChoice, Option<String>)> {
    if let EpChoice::Dml(_) = want {
        match build_session(model, want).and_then(|mut s| {
            let v = embed(&mut s, canary)?;
            if v.iter().flatten().any(|x| !x.is_finite()) {
                bail!("canary produced non-finite output");
            }
            // Test hook (M2 must keep an equivalent): fail *after* a real DML session + canary run,
            // so the CPU rebuild path is exercised exactly as on a driver fault.
            if std::env::var_os("SR_FORCE_GPU_FAIL").is_some() {
                bail!("forced GPU failure (SR_FORCE_GPU_FAIL)");
            }
            Ok(s)
        }) {
            Ok(s) => return Ok((s, want, None)),
            Err(e) => {
                let s = build_session(model, EpChoice::Cpu)?;
                return Ok((s, EpChoice::Cpu, Some(format!("{e:#}"))));
            }
        }
    }
    Ok((build_session(model, EpChoice::Cpu)?, EpChoice::Cpu, None))
}

/// Embed a batch of SEQ-token chunks; CLS pooling + L2 normalise (bge, arctic, gte-v1.5 all use CLS).
fn embed(s: &mut Session, batch: &[Vec<i64>]) -> Result<Vec<Vec<f32>>> {
    let bs = batch.len();
    let ids: Vec<i64> = batch.iter().flatten().copied().collect();
    let mask = vec![1i64; bs * SEQ];
    let needs_tt = s.inputs().iter().any(|i| i.name() == "token_type_ids");
    let out_ty = match s.outputs()[0].dtype() {
        ValueType::Tensor { ty, .. } => *ty,
        other => bail!("unexpected output type {other:?}"),
    };
    let ids_t = Tensor::from_array(([bs, SEQ], ids))?;
    let mask_t = Tensor::from_array(([bs, SEQ], mask))?;
    let outputs = if needs_tt {
        let tt = Tensor::from_array(([bs, SEQ], vec![0i64; bs * SEQ]))?;
        s.run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t, "token_type_ids" => tt])?
    } else {
        s.run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t])?
    };
    let o = &outputs[0];
    // [bs, seq, dim] -> CLS row; or [bs, dim] already pooled. Only the CLS rows are read from the
    // borrowed output (no copy of the full last_hidden_state; that copy alone was ~20% of GPU time).
    fn cls_rows<T: Copy>(shape: &[i64], data: &[T], bs: usize, f: impl Fn(T) -> f32) -> Vec<Vec<f32>> {
        let dim = *shape.last().unwrap() as usize;
        let stride = if shape.len() == 3 { shape[1] as usize * dim } else { dim };
        (0..bs)
            .map(|b| {
                let v: Vec<f32> = data[b * stride..b * stride + dim].iter().map(|&x| f(x)).collect();
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                v.into_iter().map(|x| x / n).collect()
            })
            .collect()
    }
    Ok(match out_ty {
        TensorElementType::Float32 => {
            let (sh, d) = o.try_extract_tensor::<f32>()?;
            cls_rows(sh, d, bs, |x| x)
        }
        TensorElementType::Float16 => {
            let (sh, d) = o.try_extract_tensor::<half::f16>()?;
            cls_rows(sh, d, bs, |x| x.to_f32())
        }
        t => bail!("unsupported output dtype {t:?}"),
    })
}

// ---------------------------------------------------------------- bench

fn bench(dir: &Path, file: &str, ep_s: &str, n: usize, batches: &[usize]) -> Result<()> {
    let tok = load_tokenizer(dir)?;
    let chunks = build_chunks(&tok, n)?;
    let (rss0, _) = hw::rss();
    let want = match ep_s {
        "cpu" => EpChoice::Cpu,
        "dml" => EpChoice::Dml(best_dxgi_index()),
        _ => bail!("ep must be cpu|dml"),
    };
    let vram_idx = if let EpChoice::Dml(i) = want { Some(i as u32) } else { None };
    let vram0 = vram_idx.and_then(hw::vram_usage).unwrap_or(0);
    let t0 = Instant::now();
    let (mut s, got, why) = session_with_fallback(&dir.join(file), want, &chunks[..2])?;
    let load_s = t0.elapsed().as_secs_f64();
    if let Some(w) = &why {
        eprintln!("fallback to CPU: {w}");
    }
    // warm-up (DML compiles shaders per shape)
    for &bs in batches {
        embed(&mut s, &chunks[..bs.min(chunks.len())])?;
    }
    // save the first 64 embeddings for cross-variant comparison
    let first = embed(&mut s, &chunks[..64.min(chunks.len())])?;
    let tag = format!("{}_{}_{}", dir.file_name().unwrap().to_string_lossy(), file.replace(['/', '.'], "_"), ep_s);
    let res_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    std::fs::create_dir_all(&res_dir)?;
    let bytes: Vec<u8> = first.iter().flatten().flat_map(|x| x.to_le_bytes()).collect();
    std::fs::write(res_dir.join(format!("{tag}.bin")), bytes)?;

    let mut peak_vram = 0u64;
    let mut rows = Vec::new();
    for &bs in batches {
        let t = Instant::now();
        for b in chunks.chunks(bs) {
            embed(&mut s, b)?;
            if let Some(i) = vram_idx {
                peak_vram = peak_vram.max(hw::vram_usage(i).unwrap_or(0));
            }
        }
        let secs = t.elapsed().as_secs_f64();
        // Batches run in ascending order and the DML arena only grows, so the running peak after this
        // batch size is the VRAM this batch size needs.
        rows.push(serde_json::json!({"batch": bs, "chunks_per_s": (chunks.len() as f64 / secs * 10.0).round() / 10.0, "secs": (secs*100.0).round()/100.0, "vram_peak_mib": peak_vram.saturating_sub(vram0) / 1048576}));
    }
    let (rss, peak_rss) = hw::rss();
    let line = serde_json::json!({
        "model": dir.file_name().unwrap().to_string_lossy(), "file": file, "ep_requested": ep_s,
        "ep_active": format!("{got:?}"), "fallback_reason": why, "n_chunks": chunks.len(), "seq": SEQ,
        "load_s": (load_s*100.0).round()/100.0, "results": rows,
        "rss_mib": rss / 1048576, "peak_rss_mib": peak_rss / 1048576, "rss_before_load_mib": rss0 / 1048576,
        "vram_delta_peak_mib": peak_vram.saturating_sub(vram0) / 1048576,
        "model_file_mib": std::fs::metadata(dir.join(file))?.len() / 1048576,
        "cpu_load_at_start": std::env::var("SR_CPU_LOAD").ok(),
        "intra_threads_cpu": hw::cores().0,
    });
    println!("{line}");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(res_dir.join("bench.jsonl"))?;
    use std::io::Write;
    writeln!(f, "{line}")?;
    Ok(())
}

fn best_dxgi_index() -> i32 {
    let h = hw::detect(Path::new("C:\\"));
    tier::best_gpu(&h, &tier::Thresholds::default()).map(|(g, _)| g.dxgi_index as i32).unwrap_or(0)
}

// ---------------------------------------------------------------- fallback demo

fn fallback(dir: &Path, file: &str) -> Result<()> {
    let tok = load_tokenizer(dir)?;
    let chunks = build_chunks(&tok, 8)?;
    let model = dir.join(file);

    println!("case 1: DirectML on a DXGI adapter index that does not exist (99), error_on_failure");
    let t = Instant::now();
    let (mut s, got, why) = session_with_fallback(&model, EpChoice::Dml(99), &chunks[..2])?;
    let v = embed(&mut s, &chunks)?;
    println!("  active={got:?} reason={why:?} dims={} ok_in={:.0} ms", v[0].len(), t.elapsed().as_secs_f64() * 1000.0);

    println!("case 2: same, but WITHOUT error_on_failure (ort's own behaviour)");
    let r = Session::builder()
        .map_err(|e| anyhow!("{e}"))?
        .with_memory_pattern(false)
        .map_err(|e| anyhow!("{e}"))?
        .with_execution_providers([ep::DirectML::default().with_device_id(99).build()])
        .map_err(|e| anyhow!("{e}"))?
        .commit_from_file(&model);
    match r {
        Ok(mut s) => {
            let v = embed(&mut s, &chunks)?;
            println!("  session committed silently (ort logged and fell back to CPU); dims={}", v[0].len());
        }
        Err(e) => println!("  commit error: {e}"),
    }

    println!("case 3: real adapter; SR_FORCE_GPU_FAIL set => canary fails after DML init (driver-fault simulation)");
    let t = Instant::now();
    let (mut s, got, why) = session_with_fallback(&model, EpChoice::Dml(best_dxgi_index()), &chunks[..2])?;
    let v = embed(&mut s, &chunks)?;
    println!("  active={got:?} reason={why:?} dims={} ok_in={:.0} ms", v[0].len(), t.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

fn compare(a: &Path, b: &Path) -> Result<()> {
    let ra: Vec<f32> = std::fs::read(a)?.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    let rb: Vec<f32> = std::fs::read(b)?.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    if ra.len() != rb.len() {
        bail!("dimension/length mismatch: {} vs {}", ra.len(), rb.len());
    }
    let dim = ra.len() / 64;
    let mut cos: Vec<f32> = (0..64)
        .map(|i| (0..dim).map(|j| ra[i * dim + j] * rb[i * dim + j]).sum())
        .collect();
    cos.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let mean = cos.iter().sum::<f32>() / 64.0;
    // Retrieval impact: use each of the 64 chunks as a query against the other 63 and compare the
    // top-10 neighbour sets under A and B (mean overlap; 1.0 = identical rankings).
    let top10 = |r: &[f32], q: usize| -> Vec<usize> {
        let mut s: Vec<(usize, f32)> = (0..64)
            .filter(|&j| j != q)
            .map(|j| (j, (0..dim).map(|k| r[q * dim + k] * r[j * dim + k]).sum()))
            .collect();
        s.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());
        s.into_iter().take(10).map(|x| x.0).collect()
    };
    let overlap = (0..64)
        .map(|q| {
            let (ta, tb) = (top10(&ra, q), top10(&rb, q));
            ta.iter().filter(|x| tb.contains(x)).count() as f32 / 10.0
        })
        .sum::<f32>()
        / 64.0;
    println!(
        "{{\"a\":{:?},\"b\":{:?},\"dim\":{dim},\"cos_mean\":{mean:.5},\"cos_min\":{:.5},\"top10_overlap\":{overlap:.3}}}",
        a.file_name().unwrap(),
        b.file_name().unwrap(),
        cos[0]
    );
    Ok(())
}

/// Runs the simulated flow end to end and prints every event: download drop → resume → interruption →
/// resume → swap. Same code paths as the unit tests in reembed.rs.
fn reembed_demo() -> Result<()> {
    use reembed::*;
    let root = std::env::temp_dir().join(format!("sr-reembed-demo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = Store::new(root.join("index"));
    let cat = Catalog::with_chunks(64);
    let small = ModelSpec { model_id: "BAAI/bge-small-en-v1.5".into(), revision: "ea104dac".into(), file_sha256: "bf64d054".into(), dims: 384 };
    let std768 = ModelSpec { model_id: "Snowflake/snowflake-arctic-embed-m-v1.5".into(), revision: "e58a8f75".into(), file_sha256: "8384cc8a".into(), dims: 768 };
    store.bootstrap("01-minimum", &cat, &mut FakeEmbedder::new(small));
    let mut remote = FakeRemote::new(1_000_000);
    remote.drop_at = Some(600_000);
    let job = Job { store: &store, target: std768.clone(), batch: 16, new_index_id: "02-standard".into(), crash_before_swap_rename: false };
    let model = root.join("model_fp16.onnx");
    let mut ev = Vec::new();
    let mut e = FakeEmbedder::new(std768.clone());
    e.fail_on_call = Some(2);
    for attempt in 1..=3 {
        let out = job.run(&cat, &model, &mut remote, &mut e, &mut ev);
        for x in ev.drain(..) {
            println!("{}", serde_json::to_string(&x)?);
        }
        println!("# attempt {attempt}: {out:?}; active={:?}", store.active_id());
        if matches!(out, Outcome::Swapped { .. }) {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}
