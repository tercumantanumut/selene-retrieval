//! Size probe: rusqlite bundled only.
fn main() -> anyhow::Result<()> {
    let c = rusqlite::Connection::open_in_memory()?;
    let v: String = c.query_row("select sqlite_version()", [], |r| r.get(0))?;
    println!("{v}");
    Ok(())
}
