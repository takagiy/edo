use std::io::{IsTerminal, Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let langs = edo::install::languages(&edo::install::home_dir());
    let mut stdin = || {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        s
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let (out, code) = edo::cli::run(&args, &mut stdin, langs, interactive);
    let _ = std::io::stdout().write_all(out.as_bytes());
    std::process::exit(code);
}
