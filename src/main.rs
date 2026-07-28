mod analyze;
mod audio;
mod cli;
mod dtvi;
mod external;
mod jls;
mod mp4io;
mod order;
mod plan;
mod report;
mod tools;
mod trim;
mod verify;
mod workdir;

fn main() -> anyhow::Result<()> {
    println!("Hello, world!");
    Ok(())
}

#[cfg(test)]
mod tests {
    use mp4_atom::{Encode, Ftyp};

    #[test]
    fn ftyp_encodes() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let ftyp = Ftyp {
            major_brand: b"isom".into(),
            minor_version: 512,
            compatible_brands: vec![b"isom".into(), b"iso2".into(), b"avc1".into(), b"mp41".into()],
        };

        let mut buf = Vec::new();
        ftyp.encode(&mut buf)?;

        assert!(!buf.is_empty());
        Ok(())
    }
}
