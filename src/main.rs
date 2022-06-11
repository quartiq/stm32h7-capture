//! STM32H7 tracing with the Embedded Trace FIFO
//!
//! # Design
//! The STM32H7 tracing infrastructure allows trace data to be generated on the SWO output, which
//! is a UART output to the debug probe. Because of the nature of this output, the throughput is
//! inherently limited. Additionally, there is very little buffering between ITM packet generation
//! and SWO output, so even a small amount of trace data generated in a short time interval can
//! result in the trace data overflowing in the SWO data path.
//!
//! To work around issues with buffering and throughput of the SWO output, this program provides a
//! mechanism to instead capture ITM trace data within the Embedded Trace FIFO (ETF). The ETC is a
//! 4KB FIFO stored in SRAM that can be used to buffer data before draining the trace data to an
//! external source. The ETF supports both draining data to the TPIU via a parallel trace hardware
//! interface as well as through the debug registers.
//!
//! This program uses the ETF in "software" mode with no external tracing utilities required.
//! Instead, the ETF is used to buffer up a trace which is then read out from the device via the
//! debug probe.
mod etf;

use clap::Parser;
use log::info;
use std::io::Write;

use probe_rs::{
    architecture::arm::{
        component::{Dwt, Itm, TraceFunnel},
        memory::{CoresightComponent, PeripheralType},
    },
    Error, Probe,
};

// The base address of the ETF trace funnel.
const CSTF_BASE_ADDRESS: u64 = 0xE00F_3000;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, default_value = "STM32H743ZITx")]
    target: String,
    #[clap(short, long)]
    output: String,
}

#[derive(thiserror::Error, Debug)]
enum CaptureError {
    #[error("Could not find a required CoresightComponent")]
    ComponentNotFound,
}

fn find_component<F>(
    components: &[CoresightComponent],
    func: F,
) -> Result<&CoresightComponent, Error>
where
    F: FnMut(&CoresightComponent) -> Option<&CoresightComponent>,
{
    components
        .iter()
        .find_map(func)
        .ok_or_else(|| Error::architecture_specific(CaptureError::ComponentNotFound))
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("stm32h7_capture=info"),
    )
    .init();

    let cli = Args::parse();

    let probe = Probe::list_all()
        .get(0)
        .ok_or(Error::UnableToOpenProbe("not found"))?
        .open()?;

    let mut session = probe.attach(cli.target, probe_rs::Permissions::default())?;

    let components = session.get_arm_components()?;

    // Enable tracing of the H7 core.
    {
        let mut core = session.core(0)?;
        probe_rs::architecture::arm::component::enable_tracing(&mut core)?;
    }

    let interface = session.get_arm_interface()?;

    // Configure the trace funnel to the ETF. There are two trace funnels in the STM32H7 system
    // that are only distinguishable via the number of input ports and the base address. One is the
    // SWO funnel and the other is the ETF funnel.
    let cstf = find_component(&components, |comp| {
        comp.iter().find(|component| {
            let id = component.component.id();
            id.peripheral_id().is_of_type(PeripheralType::TraceFunnel)
                && id.component_address() == CSTF_BASE_ADDRESS
        })
    })?;

    // Enable the ITM port of the trace funnel.
    let mut trace_funnel = TraceFunnel::new(interface, cstf);
    trace_funnel.unlock()?;
    trace_funnel.enable_port(0b10)?;

    // Configure the ITM to generate trace data from the DWT.
    let mut itm = Itm::new(
        interface,
        find_component(&components, |component| {
            component.find_component(PeripheralType::Itm)
        })?,
    );

    itm.unlock()?;
    itm.tx_enable()?;

    // Configure the DWT to trace exception entry and exit.
    let mut dwt = Dwt::new(
        interface,
        find_component(&components, |component| {
            component.find_component(PeripheralType::Dwt)
        })?,
    );

    dwt.enable()?;
    dwt.enable_exception_trace()?;

    // Configure the ETF.
    // TODO: upstream ETF and PeripheralType::Etf
    let etf = find_component(&components, |comp| {
        comp.iter().find(|component| {
            let id = component.component.id().peripheral_id();
            let code = id.jep106().and_then(|jep106| jep106.get());
            code == Some("ARM Ltd") && id.part() == 0x961
        })
    })?;

    let mut etf = etf::EmbeddedTraceFifo::new(interface, etf);
    let fifo_size = etf.fifo_size()?;

    etf.disable_capture()?;
    while !etf.ready()? {}
    etf.set_mode(etf::Mode::Software)?;
    etf.enable_capture()?;

    // Wait until ETB fills.
    while !etf.full()? {
        info!("ETB level: {} of {} bytes", etf.fill_level()?, fifo_size);
    }
    info!("ETB full");

    // This sequence is taken from "CoreSight Trace Memory Controller Technical Reference Manual"
    // Section 2.2.2 "Software FIFO Mode". Without following this procedure, the trace data does
    // not properly stop even after disabling capture.
    etf.stop_on_flush(true)?;
    etf.manual_flush()?;

    let mut output = std::fs::File::create(cli.output)?;

    // Extract ETB data.
    // TODO: Determine endianness and framing of coresight packets.
    // Read until ready and empty to allow e.g. pending stop sequence to be written
    // to ETB despite back pressure when full.
    loop {
        if let Some(data) = etf.read()? {
            output.write_all(&data.to_le_bytes())?;
        } else if etf.ready()? {
            break;
        }
    }
    assert!(etf.empty()?);

    etf.disable_capture()?;

    Ok(())
}
