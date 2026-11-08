//! Size probe: fastembed + static ORT only.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() > 3 {
        let rd = |f: &str| std::fs::read(std::path::Path::new(&a[1]).join(f)).unwrap();
        let m = fastembed::UserDefinedEmbeddingModel::new(rd(&a[2]), fastembed::TokenizerFiles {
            tokenizer_file: rd("tokenizer.json"), config_file: rd("config.json"),
            special_tokens_map_file: rd("special_tokens_map.json"), tokenizer_config_file: rd("tokenizer_config.json"),
        }).with_pooling(fastembed::Pooling::Cls);
        let mut e = fastembed::TextEmbedding::try_new_from_user_defined(m, Default::default()).unwrap();
        println!("{}", e.embed(&[a[3].as_str()], None).unwrap()[0].len());
    }
}
