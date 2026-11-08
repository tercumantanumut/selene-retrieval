//! Size probe: tantivy only.
fn main() -> anyhow::Result<()> {
    use tantivy::schema::{Schema, TEXT};
    let mut sb = Schema::builder();
    let f = sb.add_text_field("b", TEXT);
    let idx = tantivy::Index::create_in_ram(sb.build());
    let mut w = idx.writer(15_000_000)?;
    w.add_document(tantivy::doc!(f => "hello world"))?;
    w.commit()?;
    let q = tantivy::query::QueryParser::for_index(&idx, vec![f]).parse_query("hello")?;
    let n = idx.reader()?.searcher().search(&q, &tantivy::collector::Count)?;
    println!("{n}");
    Ok(())
}
