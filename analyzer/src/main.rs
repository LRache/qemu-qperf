use std::{
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
};

use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(value_name = "INPUT")]
    input: PathBuf,
    #[clap(value_name = "OUTPUT")]
    output: PathBuf,
    #[clap(long, short, value_name = "ELF")]
    elf: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    let mut input = BufReader::new(File::open(cli.input).expect("Failed to open input file"));
    let mut decode = || -> Result<Vec<u64>, _> {
        bincode::decode_from_std_read(&mut input, bincode::config::standard())
    };

    let loader = addr2line::Loader::new(cli.elf).expect("Failed to create addr2line loader");

    let mut output =
        BufWriter::new(File::create(cli.output).expect("Failed to create output file"));

    while let Ok(trace) = decode() {
        let mut result = vec![];
        for (i, ip) in trace.into_iter().enumerate() {
            if ip == 0 || ip == u64::MAX {
                continue;
            }
            let mut frames = loader
                .find_frames(if i == 0 { ip } else { ip - 1 })
                .unwrap();
            while let Ok(Some(frame)) = frames.next() {
                let func = frame
                    .function
                    .as_ref()
                    .and_then(|f| f.demangle().ok())
                    .unwrap_or("??".into())
                    .into_owned();
                result.push(func);
            }
        }
        if result.is_empty() {
            result.push("??".into());
        }

        result.reverse();
        writeln!(output, "{} 1", result.join(";")).expect("Failed to write to output file");
    }
}
