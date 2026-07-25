use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::spawn;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use crossbeam_channel::{Sender, bounded, unbounded};
use qemu_plugin::install::{Args, Info, Value};
use qemu_plugin::plugin::{HasCallbacks, Register};
use qemu_plugin::{
    CallbackFlags, PluginId, TranslationBlock, VCPUIndex, qemu_plugin_get_registers, qemu_plugin_read_memory_vaddr,
    qemu_plugin_register_atexit_cb,
};
use zerocopy::IntoBytes;

use crate::reg::{AllRegs, Frame, Reg, Target};

const FILE_HEADER: &[u8; 7] = b"QPERF\0\x01";

#[derive(Debug)]
struct PluginArgs {
    freq: u32,
    out: PathBuf,
    control: Option<PathBuf>,
}

impl TryFrom<&Args> for PluginArgs {
    type Error = anyhow::Error;

    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        let freq = args
            .parsed
            .get("freq")
            .map(|v| {
                if let Value::Integer(v) = v
                    && let Ok(v) = (*v).try_into()
                {
                    Ok(v)
                } else {
                    bail!("invalid frequency")
                }
            })
            .transpose()?
            .unwrap_or(99);
        let out = args
            .parsed
            .get("out")
            .map(|s| {
                if let Value::String(s) = s {
                    Ok(s.into())
                } else {
                    bail!("invalid output path")
                }
            })
            .transpose()?
            .unwrap_or("qperf.bin".into());
        let control = args
            .parsed
            .get("control")
            .map(|s| {
                if let Value::String(s) = s {
                    Ok(s.into())
                } else {
                    bail!("invalid control socket path")
                }
            })
            .transpose()?;
        Ok(PluginArgs { freq, out, control })
    }
}

#[derive(Clone)]
pub struct Profiler {
    target: Target,
    tx: Sender<(VCPUIndex, Vec<u64>)>,
    intvl: Duration,
    last: Arc<Vec<Mutex<Instant>>>,
    regs: Arc<AllRegs>,
    enabled: Arc<AtomicBool>,
}

impl Default for Profiler {
    fn default() -> Self {
        Self {
            target: Target::Riscv64,
            tx: bounded(0).0,
            intvl: Duration::MAX,
            last: Arc::default(),
            regs: Arc::default(),
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Profiler {
    fn sample(&self, vcpu_id: VCPUIndex, ip: u64) -> qemu_plugin::Result<()> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = Instant::now();
        let last = self.last.get(vcpu_id as usize).context("Unexpected vCPU index")?;
        let Ok(mut last) = last.try_lock() else {
            return Ok(());
        };
        if now.duration_since(*last) < self.intvl {
            return Ok(());
        }
        *last = now;

        let mut ips = vec![ip];
        let mut fp = self.regs.read(self.target.reg(Reg::Fp))?;

        while fp > 0 && fp % 8 == 0 {
            let mut frame = Frame::default();
            if qemu_plugin_read_memory_vaddr(fp - self.target.fp_offset(), frame.as_mut_bytes()).is_err() {
                break;
            };
            if qemu_plugin_read_memory_vaddr(frame.ip, &mut [0; 8]).is_err() {
                break;
            }

            ips.push(frame.ip);
            fp = frame.fp;
        }

        self.tx.send((vcpu_id, ips)).context("Failed to send profiling data")?;

        Ok(())
    }
}

impl HasCallbacks for Profiler {
    fn on_vcpu_init(&mut self, _id: PluginId, _vcpu_id: VCPUIndex) -> qemu_plugin::Result<()> {
        self.regs = Arc::new(qemu_plugin_get_registers()?.into());
        Ok(())
    }

    fn on_translation_block_translate(&mut self, _id: PluginId, tb: TranslationBlock) -> qemu_plugin::Result<()> {
        const KERNEL_MASK: u64 = 1 << 63;

        let ip = tb.vaddr();
        if ip & KERNEL_MASK != 0 {
            tb.instructions().for_each(|insn| {
                let ip = insn.vaddr();
                let this = self.clone();
                insn.register_execute_callback_flags(
                    move |vcpu_id| this.sample(vcpu_id, ip).expect("Failed to sample instruction"),
                    CallbackFlags::QEMU_PLUGIN_CB_R_REGS,
                );
            });
        }

        Ok(())
    }
}

impl Register for Profiler {
    fn register(&mut self, id: PluginId, args: &Args, info: &Info) -> qemu_plugin::Result<()> {
        eprintln!("QPerf loaded: id={id:?} info={info:?}");
        let args = PluginArgs::try_from(args)?;
        eprintln!("QPerf arguments: {args:?}");
        let mut file = File::create(args.out).context("Failed to create output file")?;
        file.write_all(FILE_HEADER)
            .context("Failed to write profiling file header")?;

        let (tx, rx) = unbounded();
        spawn(move || {
            while let Ok(event) = rx.recv() {
                bincode::encode_into_std_write(event, &mut file, bincode::config::standard())
                    .expect("Failed to write to output file");
            }
        });

        self.target = info.target_name.parse()?;
        self.tx = tx;
        self.intvl = Duration::from_secs_f64(1.0 / args.freq as f64);
        let max_vcpus: usize = info
            .system
            .as_ref()
            .context("QPerf requires system emulation")?
            .max_vcpus
            .try_into()
            .context("Invalid vCPU count")?;
        self.last = Arc::new((0..max_vcpus).map(|_| Mutex::new(Instant::now())).collect());
        if let Some(path) = args.control {
            let listener =
                UnixListener::bind(&path).with_context(|| format!("Failed to bind control socket at {path:?}"))?;
            let enabled = Arc::new(AtomicBool::new(false));
            self.enabled = enabled.clone();
            let cleanup_path = path.clone();
            qemu_plugin_register_atexit_cb(id, move |_| {
                if std::fs::symlink_metadata(&cleanup_path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
                    let _ = std::fs::remove_file(&cleanup_path);
                }
            })?;
            spawn(move || run_control_socket(listener, path, enabled));
        }

        Ok(())
    }
}

fn run_control_socket(listener: UnixListener, path: PathBuf, enabled: Arc<AtomicBool>) {
    eprintln!("QPerf: control socket listening at {path:?}");

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Ok(reader) = stream.try_clone() else { continue };
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            let resp: String = match line.trim() {
                "start" => {
                    enabled.store(true, Ordering::Relaxed);
                    "ok".into()
                }
                "stop" => {
                    enabled.store(false, Ordering::Relaxed);
                    "ok".into()
                }
                "status" => if enabled.load(Ordering::Relaxed) {
                    "enabled"
                } else {
                    "disabled"
                }
                .into(),
                "exit" | "quit" => {
                    let _ = writeln!(stream, "ok");
                    let _ = stream.flush();
                    break;
                }
                "" => continue,
                other => format!("err: unknown command: {other}"),
            };
            let _ = writeln!(stream, "{resp}");
            let _ = stream.flush();
        }
    }
}

qemu_plugin::register!(Profiler::default());
