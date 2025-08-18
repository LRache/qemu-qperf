mod profiler;
mod reg;

use std::sync::Mutex;

use ctor::ctor;

use crate::profiler::Profiler;

#[ctor]
fn init() {
    let tracer = Profiler::default();
    qemu_plugin::plugin::PLUGIN
        .set(Mutex::new(Box::new(tracer)))
        .ok()
        .expect("Failed to set plugin");
}
