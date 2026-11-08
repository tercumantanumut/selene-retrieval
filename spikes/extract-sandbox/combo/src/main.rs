//! M0-C test 2a: pdfium (dynamic, absolute path) + ORT (static, via fastembed) in ONE process.
//! Usage: m0c-combo <pdfium.dll> <model_dir> <pdf>
use std::time::Instant;

use anyhow::{Context, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use pdfium_render::prelude::*;

fn loaded_modules() -> Vec<String> {
    // List DLLs loaded into this process (EnumProcessModules) to check which onnxruntime/pdfium got used.
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn EnumProcessModules(h: isize, m: *mut isize, cb: u32, needed: *mut u32) -> i32;
        fn GetModuleFileNameExW(h: isize, m: isize, buf: *mut u16, n: u32) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
    }
    let mut mods = vec![0isize; 1024];
    let mut needed = 0u32;
    let mut out = vec![];
    unsafe {
        let h = GetCurrentProcess();
        if EnumProcessModules(h, mods.as_mut_ptr(), (mods.len() * 8) as u32, &mut needed) != 0 {
            for &m in &mods[..(needed as usize / 8)] {
                let mut buf = [0u16; 520];
                let n = GetModuleFileNameExW(h, m, buf.as_mut_ptr(), 520);
                out.push(String::from_utf16_lossy(&buf[..n as usize]));
            }
        }
    }
    out
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (dll, model_dir, pdf) = (&a[1], &a[2], &a[3]);
    // Same hardening the plan prescribes for every binary.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetDefaultDllDirectories(flags: u32) -> i32;
    }
    unsafe { SetDefaultDllDirectories(0x0000_0800) }; // LOAD_LIBRARY_SEARCH_SYSTEM32

    let t = Instant::now();
    let rd = |f: &str| std::fs::read(format!("{model_dir}/{f}")).with_context(|| f.to_string());
    let model = UserDefinedEmbeddingModel::new(
        rd("model_quantized.onnx")?,
        TokenizerFiles {
            tokenizer_file: rd("tokenizer.json")?,
            config_file: rd("config.json")?,
            special_tokens_map_file: rd("special_tokens_map.json")?,
            tokenizer_config_file: rd("tokenizer_config.json")?,
        },
    )
    .with_pooling(Pooling::Cls);
    let mut emb = TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::default())?;
    println!("ort_init_ms={}", t.elapsed().as_millis());

    let t = Instant::now();
    let pdfium = Pdfium::new(Pdfium::bind_to_library(dll).map_err(|e| anyhow::anyhow!("{e:?}"))?);
    println!("pdfium_bind_ms={}", t.elapsed().as_millis());

    // Interleave the two libraries several times to shake out TLS/allocator clashes.
    for round in 0..5 {
        let bytes = std::fs::read(pdf)?;
        let doc = pdfium.load_pdf_from_byte_slice(&bytes, None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut texts = vec![];
        for page in doc.pages().iter() {
            texts.push(page.text().map_err(|e| anyhow::anyhow!("{e:?}"))?.all());
        }
        drop(doc);
        let t = Instant::now();
        let v = emb.embed(&texts, None)?;
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        println!(
            "round={round} pages={} first_page={:?} vecs={} dim={} norm0={norm:.4} embed_ms={}",
            texts.len(),
            texts[0].replace(['\r', '\n'], " "),
            v.len(),
            v[0].len(),
            t.elapsed().as_millis()
        );
    }
    // Cosine of the query against the page texts: sanity check that inference is real.
    let q = emb.embed(["Represent this sentence for searching relevant passages: pdfium ort coexist page 2"], None)?;
    let d = emb.embed(["Keyword: pdfium-ort-coexist-2", "Bananas are yellow fruit."], None)?;
    let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    println!("cos(q,relevant)={:.4} cos(q,irrelevant)={:.4}", cos(&q[0], &d[0]), cos(&q[0], &d[1]));

    for m in loaded_modules() {
        let l = m.to_lowercase();
        if l.contains("onnx") || l.contains("pdfium") || l.contains("vcruntime") || l.contains("msvcp") || l.contains("ucrtbase") {
            println!("module: {m}");
        }
    }
    println!("OK");
    Ok(())
}
