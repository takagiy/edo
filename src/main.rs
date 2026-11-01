use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let langs = edo::install::languages(&edo::install::store_dir());
    let mut stdin = || {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        s
    };
    let (out, code) = edo::cli::run(&args, &mut stdin, langs);
    let _ = std::io::stdout().write_all(out.as_bytes());
    std::process::exit(code);
}
