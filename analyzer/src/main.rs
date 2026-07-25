use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use clap::Parser;
use object::{Object, ObjectSymbol, SymbolKind};

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

#[derive(Default)]
struct SymbolTable {
    symbols: Vec<Symbol>,
}

struct Symbol {
    address: u64,
    size: u64,
    name: String,
}

impl SymbolTable {
    fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = std::fs::read(path)?;
        let file = object::File::parse(&*data)?;
        let mut symbols = file
            .symbols()
            .chain(file.dynamic_symbols())
            .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.address() != 0)
            .filter_map(|symbol| {
                let name = symbol.name().ok()?;
                Some(Symbol {
                    address: symbol.address(),
                    size: symbol.size(),
                    name: rustc_demangle::demangle(name).to_string(),
                })
            })
            .collect::<Vec<_>>();

        symbols.sort_by_key(|symbol| symbol.address);
        symbols.dedup_by_key(|symbol| symbol.address);

        Ok(SymbolTable { symbols })
    }

    fn find(&self, ip: u64) -> Option<String> {
        let index = self.symbols.partition_point(|symbol| symbol.address <= ip);
        let symbol = self.symbols.get(index.checked_sub(1)?)?;
        let offset = ip - symbol.address;
        if symbol.size != 0 && offset >= symbol.size {
            return None;
        }

        if offset == 0 {
            Some(symbol.name.clone())
        } else {
            Some(format!("{}+0x{offset:x}", symbol.name))
        }
    }
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

    let symbols = SymbolTable::load(&cli.elf).unwrap_or_default();
    let loader = addr2line::Loader::new(cli.elf).expect("Failed to create addr2line loader");

    let mut output = BufWriter::new(File::create(cli.output).expect("Failed to create output file"));

    while let Ok((vcpu_id, trace)) = decode() {
        let mut result = vec![];
        for (i, ip) in trace.into_iter().enumerate() {
            if ip == 0 || ip == u64::MAX {
                continue;
            }
            let lookup_ip = if i == 0 { ip } else { ip - 1 };
            let mut resolved = false;

            if let Ok(mut frames) = loader.find_frames(lookup_ip) {
                while let Ok(Some(frame)) = frames.next() {
                    if let Some(func) = frame.function.as_ref().and_then(|f| f.demangle().ok()) {
                        result.push(func.into_owned());
                        resolved = true;
                    }
                }
            }

            if !resolved {
                result.push(symbols.find(lookup_ip).unwrap_or_else(|| format!("0x{lookup_ip:x}")));
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
