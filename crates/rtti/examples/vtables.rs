//! `cargo run --release -p rtti --example vtables -- <exe> <Class>...`
//! Prints each class's vtables (subobject offset, RVA, slot count).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let path = arguments.next().ok_or("usage: vtables <exe> <Class>...")?;
    let bytes = std::fs::read(&path)?;
    let image = rtti::Image::from_file(&bytes)?;
    let start = std::time::Instant::now();
    let classes = rtti::index(&image);
    println!(
        "{path}: {} classes with RTTI, indexed in {:?}",
        classes.len(),
        start.elapsed()
    );
    for class in arguments {
        let found = classes
            .get(&rtti::decorated(&class, false))
            .or_else(|| classes.get(&rtti::decorated(&class, true)));
        match found {
            None => println!("  {class}: not found"),
            Some(tables) => {
                for table in tables {
                    println!(
                        "  {class}: offset {:#x} vtable {:#x} slots {}",
                        table.offset,
                        table.rva,
                        table.slots.len()
                    );
                }
            }
        }
    }
    Ok(())
}
