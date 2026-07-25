use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use clap::Parser;

const FILE_HEADER: &[u8; 7] = b"QPERF\0\x01";

#[derive(Parser)]
struct Cli {
    #[clap(value_name = "INPUT")]
    input: PathBuf,
    #[clap(value_name = "OUTPUT")]
    output: PathBuf,
    #[clap(long, short, value_name = "ELF")]
    elf: PathBuf,
    /// Merge samples from all vCPUs instead of adding a per-vCPU root frame.
    #[clap(long)]
    aggregate: bool,
}

fn main() {
    let cli = Cli::parse();

    let mut input = BufReader::new(File::open(cli.input).expect("Failed to open input file"));
    let mut header = [0; FILE_HEADER.len()];
    input
        .read_exact(&mut header)
        .expect("Failed to read profiling file header");
    assert_eq!(
        &header, FILE_HEADER,
        "Unsupported profiling file format; regenerate it with the matching qperf plugin"
    );

    let mut decode =
        || -> Result<(u32, Vec<u64>), _> { bincode::decode_from_std_read(&mut input, bincode::config::standard()) };

    let loader = addr2line::Loader::new(cli.elf).expect("Failed to create addr2line loader");

    let mut output = BufWriter::new(File::create(cli.output).expect("Failed to create output file"));

    while let Ok((vcpu_id, trace)) = decode() {
        let mut result = vec![];
        for (i, ip) in trace.into_iter().enumerate() {
            if ip == 0 || ip == u64::MAX {
                continue;
            }
            let mut frames = loader.find_frames(if i == 0 { ip } else { ip - 1 }).unwrap();
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
        if !cli.aggregate {
            result.insert(0, format!("[CPU {vcpu_id}]"));
        }
        writeln!(output, "{} 1", result.join(";")).expect("Failed to write to output file");
    }
}
