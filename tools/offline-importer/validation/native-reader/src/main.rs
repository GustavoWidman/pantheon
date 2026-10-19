// This uses the byte-identical frozen V5 engine, not the importer adapter.
#[allow(dead_code)]
#[path = "../../../vendor/native-v5.rs"]
mod native;
fn main()->anyhow::Result<()> {
    let path=std::env::args_os().nth(1).ok_or_else(||anyhow::anyhow!("native memory directory required"))?;
    let memory=native::Memory::open(path,native::VIEW_BYTES)?;
    println!("{}",serde_json::to_string_pretty(&memory.stats())?);
    let n=memory.stats()["messages"].as_u64().unwrap();
    for id in 0..n {println!("{} {}",memory.date(id)?,memory.zoom(id,1)?);}
    Ok(())
}
