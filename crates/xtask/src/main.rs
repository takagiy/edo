//! Release packaging: `xtask package <target>...` writes `target/edo-<target>.tar.gz` for each
//! target already built with `--release`. Tar entries get mode 0755 explicitly, since a Windows
//! file system has no executable bit to copy.

use std::fs::File;
use std::io;
use std::path::Path;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(("package", targets)) = args.split_first().map(|(cmd, rest)| (cmd.as_str(), rest)) else {
        eprintln!("usage: xtask package <target>...");
        std::process::exit(2);
    };
    for target in targets {
        package(target)?;
    }
    Ok(())
}

fn package(target: &str) -> io::Result<()> {
    let name = if target.contains("windows") { "edo.exe" } else { "edo" };
    let binary = std::fs::read(Path::new("target").join(target).join("release").join(name))?;
    let out = File::create(Path::new("target").join(format!("edo-{target}.tar.gz")))?;
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(out, flate2::Compression::best()));
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    archive.append_data(&mut header, name, binary.as_slice())?;
    archive.into_inner()?.finish()?;
    println!("target/edo-{target}.tar.gz");
    Ok(())
}
