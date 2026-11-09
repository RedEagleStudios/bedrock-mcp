//! `compare <exe> <decorated>...`: print each class's vtables with their first slots.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let bytes = std::fs::read(
        arguments
            .next()
            .ok_or("usage: compare <exe> <decorated>...")?,
    )?;
    let image = rtti::Image::from_file(&bytes)?;
    let classes = rtti::index(&image);
    for name in arguments {
        for table in classes.get(&name).into_iter().flatten() {
            let slots: Vec<String> = table
                .slots
                .iter()
                .take(6)
                .map(|slot| format!("{slot:#x}"))
                .collect();
            println!(
                "{name} +{:#x} @{:#x} [{}] {}",
                table.offset,
                table.rva,
                table.slots.len(),
                slots.join(" ")
            );
        }
    }
    Ok(())
}
