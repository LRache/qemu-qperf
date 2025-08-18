use std::{collections::BTreeMap, error::Error, fmt, str::FromStr};

use anyhow::{Context, anyhow};
use qemu_plugin::RegisterDescriptor;
use zerocopy::FromBytes;

#[derive(Default)]
pub struct AllRegs(BTreeMap<String, RegisterDescriptor<'static>>);

impl AllRegs {
    pub fn read(&self, name: &str) -> anyhow::Result<u64> {
        let value = self
            .0
            .get(name)
            .context(format!("Register {name} not found"))?
            .read()?;

        value
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|v| anyhow!("Unexpected size for register {name}: {}", v.len()))
    }
}

impl From<Vec<RegisterDescriptor<'static>>> for AllRegs {
    fn from(regs: Vec<RegisterDescriptor<'static>>) -> Self {
        let map = regs
            .into_iter()
            .map(|reg| (reg.name.clone(), reg))
            .collect();
        AllRegs(map)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Riscv64,
    LoongArch64,
}

#[derive(Debug)]
pub struct TargetNoSupport;

impl fmt::Display for TargetNoSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Target not supported")
    }
}

impl Error for TargetNoSupport {}

impl FromStr for Target {
    type Err = TargetNoSupport;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "riscv64" => Ok(Target::Riscv64),
            "loongarch64" => Ok(Target::LoongArch64),
            _ => Err(TargetNoSupport),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(unused)]
pub enum Reg {
    Sp,
    Fp,
}

#[derive(Debug, Clone, Copy, FromBytes)]
#[repr(C)]
pub struct Frame {
    pub fp: u64,
    pub ip: u64,
}

impl Target {
    pub fn reg(&self, reg: Reg) -> &'static str {
        match self {
            Target::Riscv64 => match reg {
                Reg::Sp => "sp",
                Reg::Fp => "fp",
            },
            Target::LoongArch64 => match reg {
                Reg::Sp => "r3",
                Reg::Fp => "r22",
            },
        }
    }

    pub fn fp_offset(&self) -> u64 {
        match self {
            Target::Riscv64 | Target::LoongArch64 => size_of::<Frame>() as u64,
        }
    }
}
