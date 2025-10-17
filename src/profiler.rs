use std::{
    fs::File,
    io::BufWriter,
    sync::{Arc, Mutex},
    thread::spawn,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use crossbeam_channel::{Sender, bounded, unbounded};
use qemu_plugin::{
    CallbackFlags, PluginId, TranslationBlock, VCPUIndex,
    install::{Args, Info, Value},
    plugin::{HasCallbacks, Register},
    qemu_plugin_get_registers, qemu_plugin_read_memory_vaddr,
};
use zerocopy::IntoBytes;

use crate::reg::{AllRegs, Frame, Reg, Target};

#[derive(Debug)]
struct PluginArgs {
    freq: u64,
    out_path: String,
}

impl TryFrom<&Args> for PluginArgs {
    type Error = anyhow::Error;

    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        let freq = args.parsed.get("freq").map_or_else(
            || Ok(100),
            |v| {
                if let Value::Integer(v) = v
                    && *v > 0
                {
                    Ok(*v as _)
                } else {
                    bail!("invalid frequency")
                }
            },
        )?;
        let out_path = args.parsed.get("out").map_or_else(
            || Ok("qperf.data".to_string()),
            |s| {
                if let Value::String(s) = s {
                    Ok(s.clone())
                } else {
                    bail!("invalid output path")
                }
            },
        )?;
        Ok(PluginArgs { freq, out_path })
    }
}

#[derive(Clone)]
pub struct Profiler {
    target: Target,
    tx: Sender<Vec<u64>>,
    intvl: Duration,
    last: Arc<Mutex<Instant>>,
    regs: Arc<AllRegs>,
}

impl Default for Profiler {
    fn default() -> Self {
        Self {
            target: Target::Riscv64,
            tx: bounded(0).0,
            intvl: Duration::MAX,
            last: Arc::new(Mutex::new(Instant::now())),
            regs: Arc::default(),
        }
    }
}

impl Profiler {
    fn sample(&mut self, ip: u64) -> qemu_plugin::Result<()> {
        let now = Instant::now();
        let Ok(mut last) = self.last.try_lock() else {
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
            if qemu_plugin_read_memory_vaddr(fp - self.target.fp_offset(), frame.as_mut_bytes())
                .is_err()
            {
                break;
            };
            if qemu_plugin_read_memory_vaddr(frame.ip, &mut [0; 8]).is_err() {
                break;
            }

            ips.push(frame.ip);
            fp = frame.fp;
        }

        self.tx.send(ips).context("Failed to send profiling data")?;

        Ok(())
    }
}

impl HasCallbacks for Profiler {
    fn on_vcpu_init(&mut self, _id: PluginId, _vcpu_id: VCPUIndex) -> qemu_plugin::Result<()> {
        self.regs = Arc::new(qemu_plugin_get_registers()?.into());
        Ok(())
    }

    fn on_translation_block_translate(
        &mut self,
        _id: PluginId,
        tb: TranslationBlock,
    ) -> qemu_plugin::Result<()> {
        const KERNEL_MASK: u64 = 0xffff_0000_0000_0000;

        let ip = tb.vaddr();
        if ip & KERNEL_MASK != 0 {
            tb.instructions().for_each(|insn| {
                let ip = insn.vaddr();
                let mut this = self.clone();
                insn.register_execute_callback_flags(
                    move |_| {
                        if let Err(e) = this.sample(ip) {
                            eprintln!("Error during sampling: {e}");
                        }
                    },
                    CallbackFlags::QEMU_PLUGIN_CB_R_REGS,
                );
            });
        }

        Ok(())
    }
}

impl Register for Profiler {
    fn register(&mut self, id: PluginId, args: &Args, info: &Info) -> qemu_plugin::Result<()> {
        eprintln!(
            "QPerf loaded: id={:?} target={} current version={}",
            id, info.target_name, info.version.current
        );
        let args = PluginArgs::try_from(args)?;
        eprintln!("QPerf arguments: {args:?}");

        let (tx, rx) = unbounded();
        spawn(move || {
            let file = File::create(args.out_path).expect("Failed to create output file");
            let mut writer = BufWriter::new(file);
            while let Ok(event) = rx.recv() {
                bincode::encode_into_std_write(event, &mut writer, bincode::config::standard())
                    .expect("Failed to write to output file");
            }
        });

        self.target = info.target_name.parse()?;
        self.tx = tx;
        self.intvl = Duration::from_secs_f64(1.0 / args.freq as f64);

        Ok(())
    }
}

qemu_plugin::register!(Profiler::default());
