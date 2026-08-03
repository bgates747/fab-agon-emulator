use crate::AgonMachine;
/// Interface for a debugger
///
use ez80::Machine;
use std::cell::Cell;
use std::fmt;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};

const MAX_ADDRESS: u32 = 0x00ff_ffff;
const ADDRESS_SPACE_SIZE: u64 = 0x0100_0000;
const MAX_DEBUGGER_MEMORY_BYTES: u32 = 4096;
const MAX_DEBUGGER_DISASSEMBLY_RANGE_BYTES: u64 = 4096;

pub struct DebuggerConnection {
    pub tx: Sender<DebugResp>,
    pub rx: Receiver<DebugCmd>,
}

pub type Registers = ez80::Registers;
pub type Reg8 = ez80::Reg8;
pub type Reg16 = ez80::Reg16;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DebuggerError {
    ControllerDisconnected,
}

impl fmt::Display for DebuggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControllerDisconnected => write!(f, "debugger controller disconnected"),
        }
    }
}

impl std::error::Error for DebuggerError {}

type DebuggerResult<T = ()> = Result<T, DebuggerError>;

#[derive(Debug, Copy, Clone)]
pub enum PauseReason {
    DebuggerRequested,
    OutOfBoundsMemAccess(u32), // address
    DebuggerBreakpoint,
    IOBreakpoint(u8),
}

#[derive(Debug, Clone)]
pub enum DebugCmd {
    Ping,
    Pause(PauseReason),
    Continue,
    Step,
    StepOver,
    SetTrace(bool),
    Message(String),
    AddTrigger(Trigger),
    DeleteTrigger(u32),
    ListTriggers,
    GetMemory {
        start: u32,
        len: u32,
    },
    GetMemoryAtReg {
        reg: Reg16,
        len: u32,
    },
    GetRegisters,
    GetState,
    DisassemblePc {
        adl: Option<bool>,
    },
    Disassemble {
        adl: Option<bool>,
        start: u32,
        end: u32,
    },
}

#[derive(Debug)]
pub enum DebugResp {
    Resumed,
    Paused(PauseReason),
    Pong,
    Message(String),
    Registers(Registers),
    State {
        registers: Registers,
        instructions_executed: u64,
        total_cycles_elapsed: u64,
        stack: [u8; 16],
        pc_instruction: String,
    },
    Memory {
        start: u32,
        data: Vec<u8>,
    },
    // (start, disasm, bytes)
    Disassembly {
        pc: u32,
        adl: bool,
        disasm: Vec<ez80::disassembler::Disasm>,
    },
    Triggers(Vec<Trigger>),
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub address: u32,
    pub once: bool,
    pub actions: Vec<DebugCmd>,
}

pub struct DebuggerServer {
    con: DebuggerConnection,
    triggers: Vec<Trigger>,
    reported_memory_fault: Option<u32>,
}

struct DebuggerObservationMachine<'a> {
    machine: &'a AgonMachine,
    start: u32,
    next_offset: Cell<u32>,
    adl: bool,
    mbase: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixScan {
    pub(crate) opcode: u8,
    pub(crate) prefix_len: u32,
    pub(crate) immediate_long: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixScanError {
    Unmapped { address: u32 },
    FullSpan,
}

impl Machine for DebuggerObservationMachine<'_> {
    fn peek(&self, _address: u32) -> u8 {
        let offset = self.next_offset.get();
        self.next_offset.set(offset + 1);
        let address = code_address(self.start, offset, self.adl, self.mbase);
        self.machine.observed_byte(address).unwrap_or(0xf5)
    }

    fn poke(&mut self, _address: u32, _value: u8) {}

    fn use_cycles(&self, _cycles: i32) {}

    fn port_in(&mut self, _address: u16) -> u8 {
        0xff
    }

    fn port_out(&mut self, _address: u16, _value: u8) {}
}

pub(crate) fn code_address(start: u32, offset: u32, adl: bool, mbase: u8) -> u32 {
    if adl {
        start.wrapping_add(offset) & MAX_ADDRESS
    } else {
        (u32::from(mbase) << 16) | u32::from((start as u16).wrapping_add(offset as u16))
    }
}

pub(crate) fn scan_ez80_prefixes(
    machine: &AgonMachine,
    start: u32,
    adl: bool,
    mbase: u8,
) -> Result<PrefixScan, PrefixScanError> {
    let mut offset = 0;
    let mut immediate_long = adl;
    let span = if adl {
        ADDRESS_SPACE_SIZE as u32
    } else {
        0x1_0000
    };

    while offset < span {
        let address = code_address(start, offset, adl, mbase);
        let byte = machine
            .observed_byte(address)
            .ok_or(PrefixScanError::Unmapped { address })?;
        match byte {
            0x40 | 0x49 => immediate_long = false,
            0x52 | 0x5b => immediate_long = true,
            _ => break,
        }
        offset += 1;
    }
    if offset == span {
        return Err(PrefixScanError::FullSpan);
    }

    // The decoder never re-enters the size-prefix phase, so the index-prefix
    // phase needs its own full-span cycle bound after any size prefixes.
    let mut index_prefixes = 0;
    while index_prefixes < span {
        let address = code_address(start, offset, adl, mbase);
        let byte = machine
            .observed_byte(address)
            .ok_or(PrefixScanError::Unmapped { address })?;
        if matches!(byte, 0xdd | 0xfd) {
            offset += 1;
            index_prefixes += 1;
        } else {
            return Ok(PrefixScan {
                opcode: byte,
                prefix_len: offset,
                immediate_long,
            });
        }
    }
    Err(PrefixScanError::FullSpan)
}

fn neutral_disassemble(
    machine: &AgonMachine,
    cpu: &ez80::Cpu,
    adl_override: Option<bool>,
    start: u32,
    end: u32,
) -> Vec<ez80::disassembler::Disasm> {
    if start > MAX_ADDRESS {
        return vec![];
    }

    let requested_end = (if end <= start {
        u64::from(start).saturating_add(0x20)
    } else {
        u64::from(end)
    })
    .min(ADDRESS_SPACE_SIZE)
    .min(u64::from(start).saturating_add(MAX_DEBUGGER_DISASSEMBLY_RANGE_BYTES));

    let mut scratch = ez80::Cpu::new_ez80();
    scratch.state = cpu.state.clone();
    if let Some(adl) = adl_override {
        scratch.state.reg.adl = adl;
    }
    scratch.state.reg.pc = start;
    scratch.state.reg.mbase = (start >> 16) as u8;

    let mut result = vec![];
    while u64::from(scratch.state.pc()) < requested_end {
        let address = scratch.state.pc();
        let adl = scratch.state.reg.adl;
        let mbase = scratch.state.reg.mbase;
        let prefix_scan = match scan_ez80_prefixes(machine, address, adl, mbase) {
            Ok(scan) => scan,
            Err(PrefixScanError::Unmapped { address: unmapped }) => {
                result.push(ez80::disassembler::Disasm {
                    loc: address,
                    asm: format!("<unmapped instruction byte ${unmapped:06x}>"),
                    bytes: vec![],
                });
                break;
            }
            Err(PrefixScanError::FullSpan) => {
                result.push(ez80::disassembler::Disasm {
                    loc: address,
                    asm: "<full address span contains only eZ80 prefixes>".to_string(),
                    bytes: vec![],
                });
                break;
            }
        };
        // The dependency's operand peeks wrap by operand width, which differs
        // from instruction-fetch wrapping when an eZ80 size prefix overrides
        // the live ADL mode. Feed bytes in architectural fetch order instead.
        let mut observed = DebuggerObservationMachine {
            machine,
            start: address,
            next_offset: Cell::new(0),
            adl,
            mbase,
        };
        let text = scratch.disasm_instruction(&mut observed);
        let next = scratch.state.pc();
        let modular_len = if adl {
            next.wrapping_sub(address) & MAX_ADDRESS
        } else {
            u32::from((next as u16).wrapping_sub(address as u16))
        };
        let span = if adl {
            ADDRESS_SPACE_SIZE as u32
        } else {
            0x1_0000
        };
        let minimum_len = prefix_scan.prefix_len + 1;
        // PC differences are modulo the active code-address span. The prefix
        // count is a lower bound that restores a span lost to wraparound.
        let mut len = if modular_len == 0 { span } else { modular_len };
        if len < minimum_len {
            len += span;
        }

        let mut bytes = vec![];
        if bytes.try_reserve_exact(len as usize).is_err() {
            result.push(ez80::disassembler::Disasm {
                loc: address,
                asm: "<debugger could not allocate instruction bytes>".to_string(),
                bytes,
            });
            break;
        }
        for offset in 0..len {
            bytes.push(
                machine
                    .observed_byte(code_address(address, offset, adl, mbase))
                    .unwrap_or(0xf5),
            );
        }
        result.push(ez80::disassembler::Disasm {
            loc: address,
            asm: text,
            bytes,
        });

        scratch.state.clear_sz_prefix();
        scratch.state.index = Reg16::HL;
        if next <= address {
            break;
        }
    }
    result
}

impl DebuggerServer {
    pub fn new(con: DebuggerConnection) -> Self {
        DebuggerServer {
            con,
            triggers: vec![],
            reported_memory_fault: None,
        }
    }

    fn send(&self, response: DebugResp) -> DebuggerResult {
        self.con
            .tx
            .send(response)
            .map_err(|_| DebuggerError::ControllerDisconnected)
    }

    fn acknowledge_memory_fault(&mut self, machine: &AgonMachine) {
        machine.mem_out_of_bounds.set(None);
        self.reported_memory_fault = None;
    }

    fn on_out_of_bounds(
        &mut self,
        machine: &mut AgonMachine,
        cpu: &mut ez80::Cpu,
    ) -> DebuggerResult<bool> {
        let Some(address) = machine.mem_out_of_bounds.get() else {
            self.reported_memory_fault = None;
            return Ok(false);
        };

        machine.set_paused(true);
        if self.reported_memory_fault == Some(address) {
            return Ok(true);
        }
        self.reported_memory_fault = Some(address);

        self.send(DebugResp::Paused(PauseReason::OutOfBoundsMemAccess(
            address,
        )))?;
        self.send_disassembly(
            machine,
            cpu,
            None,
            machine.last_pc,
            code_address(machine.last_pc, 1, cpu.state.reg.adl, cpu.state.reg.mbase),
        )?;
        self.send_state(machine, cpu)?;
        Ok(true)
    }

    fn on_unhandled_io(
        &mut self,
        machine: &mut AgonMachine,
        cpu: &mut ez80::Cpu,
    ) -> DebuggerResult {
        // An IO-space read or write occurred, that didn't correspond to
        // any EZ80F92 peripherals
        //
        // We implement some debugger functions with these unused IOs
        if let Some(address) = machine.io_unhandled.replace(None) {
            match address & 0xff {
                0x10..=0x1f => {
                    machine.set_paused(true);
                    self.send(DebugResp::Paused(PauseReason::IOBreakpoint(address as u8)))?;
                    self.send_disassembly(
                        machine,
                        cpu,
                        None,
                        machine.last_pc,
                        code_address(machine.last_pc, 1, cpu.state.reg.adl, cpu.state.reg.mbase),
                    )?;
                    self.send_state(machine, cpu)?;
                }
                0x20..=0x2f => {
                    self.send(DebugResp::Message(format!(
                        "State dump triggered by IO 0x{:x} access at PC=${:x}",
                        address & 0xff,
                        machine.last_pc
                    )))?;
                    self.send_state(machine, cpu)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Called before each instruction is executed
    pub fn tick(&mut self, machine: &mut AgonMachine, cpu: &mut ez80::Cpu) -> DebuggerResult {
        let pc = cpu.state.pc();

        // catch out of bounds memory accesses
        self.on_out_of_bounds(machine, cpu)?;
        // debugger functions triggered by IO read/write
        self.on_unhandled_io(machine, cpu)?;

        // check triggers
        let dispatch_triggers = !machine.is_paused();
        if dispatch_triggers {
            let to_run: Vec<Trigger> = self
                .triggers
                .iter()
                .filter(|t| t.address == pc)
                .cloned()
                .collect();

            for t in to_run {
                for a in &t.actions {
                    self.handle_debug_cmd(a, machine, cpu)?;
                }
            }

            // A one-shot at the paused PC remains armed until it is dispatched.
            self.triggers.retain(|b| !(b.address == pc && b.once));
        }

        loop {
            match self.con.rx.try_recv() {
                Ok(cmd) => self.handle_debug_cmd(&cmd, machine, cpu)?,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(DebuggerError::ControllerDisconnected)
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        Ok(())
    }

    fn handle_debug_cmd(
        &mut self,
        cmd: &DebugCmd,
        machine: &mut AgonMachine,
        cpu: &mut ez80::Cpu,
    ) -> DebuggerResult {
        let pc = cpu.state.pc();

        match cmd {
            DebugCmd::Message(s) => self.send(DebugResp::Message(s.clone()))?,
            DebugCmd::SetTrace(b) => {
                cpu.set_trace(*b);
                self.send(DebugResp::Pong)?;
            }
            DebugCmd::ListTriggers => {
                self.send(DebugResp::Triggers(self.triggers.clone()))?;
            }
            DebugCmd::DisassemblePc { adl } => {
                let start = cpu.state.pc();
                let end = u64::from(start)
                    .saturating_add(0x20)
                    .min(ADDRESS_SPACE_SIZE) as u32;
                self.send_disassembly(machine, cpu, *adl, start, end)?;
            }
            DebugCmd::Disassemble {
                adl,
                start,
                mut end,
            } => {
                if end <= *start {
                    end = u64::from(*start)
                        .saturating_add(0x20)
                        .min(ADDRESS_SPACE_SIZE) as u32;
                };
                self.send_disassembly(machine, cpu, *adl, *start, end)?;
            }
            DebugCmd::StepOver => {
                // if the opcode at PC is a call, set a 'once' breakpoint on the
                // instruction after it
                let adl = cpu.state.reg.adl;
                let mbase = cpu.state.reg.mbase;
                match scan_ez80_prefixes(machine, pc, adl, mbase) {
                    // RST instruction at (pc)
                    Ok(PrefixScan {
                        opcode: 0xc7 | 0xd7 | 0xe7 | 0xf7 | 0xcf | 0xdf | 0xef | 0xff,
                        prefix_len,
                        ..
                    }) => {
                        let addr_next = code_address(pc, prefix_len + 1, adl, mbase);
                        self.triggers.push(Trigger {
                            address: addr_next,
                            once: true,
                            actions: vec![
                                DebugCmd::Pause(PauseReason::DebuggerRequested),
                                DebugCmd::Message("Stepped over RST".to_string()),
                                DebugCmd::GetState,
                            ],
                        });
                        self.acknowledge_memory_fault(machine);
                        machine.set_paused(false);
                        self.send(DebugResp::Resumed)?;
                    }
                    // CALL instruction at (pc)
                    Ok(PrefixScan {
                        opcode: 0xc4 | 0xd4 | 0xe4 | 0xf4 | 0xcc | 0xcd | 0xdc | 0xec | 0xfc,
                        prefix_len,
                        immediate_long,
                    }) => {
                        let instruction_len = prefix_len + 1 + if immediate_long { 3 } else { 2 };
                        let addr_next = code_address(pc, instruction_len, adl, mbase);
                        self.triggers.push(Trigger {
                            address: addr_next,
                            once: true,
                            actions: vec![
                                DebugCmd::Pause(PauseReason::DebuggerRequested),
                                DebugCmd::Message("Stepped over CALL".to_string()),
                                DebugCmd::GetState,
                            ],
                        });
                        self.acknowledge_memory_fault(machine);
                        machine.set_paused(false);
                        self.send(DebugResp::Resumed)?;
                    }
                    // Other instructions, including an unmapped opcode, execute
                    // through the real machine path so faults retain their
                    // ordinary semantics.
                    Ok(_) | Err(PrefixScanError::Unmapped { .. }) => {
                        self.acknowledge_memory_fault(machine);
                        machine.execute_and_settle_instruction(cpu);
                        machine.set_paused(true);
                        self.send_state(machine, cpu)?;
                    }
                    Err(PrefixScanError::FullSpan) => {
                        machine.set_paused(true);
                        self.send(DebugResp::Message(
                            "StepOver refused: the complete code-address span contains only eZ80 prefixes"
                                .to_string(),
                        ))?;
                    }
                }
            }
            DebugCmd::Step => {
                self.acknowledge_memory_fault(machine);
                machine.execute_and_settle_instruction(cpu);
                machine.set_paused(true);
                self.send_state(machine, cpu)?;
            }
            DebugCmd::Pause(reason) => {
                machine.set_paused(true);
                self.send(DebugResp::Paused(*reason))?;
            }
            DebugCmd::Continue => {
                self.acknowledge_memory_fault(machine);
                machine.set_paused(false);
                self.send(DebugResp::Resumed)?;
            }
            DebugCmd::AddTrigger(t) => {
                self.triggers.push(t.clone());
                self.send(DebugResp::Pong)?;
            }
            DebugCmd::DeleteTrigger(addr) => {
                self.triggers.retain(|b| b.address != *addr);
                self.send(DebugResp::Pong)?;
            }
            DebugCmd::Ping => self.send(DebugResp::Pong)?,
            DebugCmd::GetRegisters => self.send_registers(cpu)?,
            DebugCmd::GetState => self.send_state(machine, cpu)?,
            DebugCmd::GetMemory { start, len } => {
                self.send_mem(machine, *start, *len)?;
            }
            DebugCmd::GetMemoryAtReg { reg, len } => {
                let addr = match (*reg, cpu.state.reg.adl) {
                    (Reg16::AF, true) => u32::from(cpu.state.reg.get16(Reg16::AF)),
                    (_, true) => cpu.state.reg.get24(*reg),
                    (_, false) => cpu.state.reg.get16_mbase(*reg),
                };
                self.send_mem(machine, addr, *len)?;
            }
        }
        Ok(())
    }

    fn send_disassembly(
        &self,
        machine: &AgonMachine,
        cpu: &ez80::Cpu,
        adl_override: Option<bool>,
        start: u32,
        end: u32,
    ) -> DebuggerResult {
        let disasm = neutral_disassemble(machine, cpu, adl_override, start, end);
        self.send(DebugResp::Disassembly {
            pc: cpu.state.pc(),
            adl: adl_override.unwrap_or(cpu.state.reg.adl),
            disasm,
        })
    }

    fn send_mem(&self, machine: &AgonMachine, start: u32, len: u32) -> DebuggerResult {
        let len = len.min(MAX_DEBUGGER_MEMORY_BYTES);
        let data = (0..len)
            .map(|offset| {
                start
                    .checked_add(offset)
                    .filter(|address| *address <= MAX_ADDRESS)
                    .and_then(|address| machine.observed_byte(address))
                    .unwrap_or(0xf5)
            })
            .collect();
        self.send(DebugResp::Memory { start, data })
    }

    fn send_state(&self, machine: &AgonMachine, cpu: &ez80::Cpu) -> DebuggerResult {
        let mut stack: [u8; 16] = [0; 16];
        let registers = &cpu.state.reg;
        let sp = if registers.adl {
            registers.get24(ez80::Reg16::SP)
        } else {
            registers.get16_mbase(ez80::Reg16::SP)
        };
        for (offset, value) in stack.iter_mut().enumerate() {
            let address = code_address(sp, offset as u32, registers.adl, registers.mbase);
            *value = machine.observed_byte(address).unwrap_or(0xf5);
        }

        let pc = cpu.state.pc();
        let end = u64::from(pc).saturating_add(1).min(ADDRESS_SPACE_SIZE) as u32;
        let pc_instruction = neutral_disassemble(machine, cpu, None, pc, end)
            .into_iter()
            .next()
            .map(|instruction| instruction.asm)
            .unwrap_or_default();

        self.send(DebugResp::State {
            registers: registers.clone(),
            instructions_executed: cpu.state.instructions_executed,
            total_cycles_elapsed: machine.total_cycles_elapsed,
            stack,
            pc_instruction,
        })
    }

    fn send_registers(&self, cpu: &ez80::Cpu) -> DebuggerResult {
        self.send(DebugResp::Registers(cpu.state.reg.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpio::GpioSet;
    use crate::{AgonMachineConfig, GpioVgaFrame, RamInit, SerialLink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicI32};
    use std::sync::{mpsc, Arc};

    const LOAD_ADDRESS: u32 = 0x040000;

    struct NullSerialLink;

    impl SerialLink for NullSerialLink {
        fn send(&mut self, _byte: u8) {}

        fn recv(&mut self) -> Option<u8> {
            None
        }

        fn read_clear_to_send(&mut self) -> bool {
            true
        }
    }

    fn machine() -> AgonMachine {
        let (frame_tx, _frame_rx) = mpsc::channel::<GpioVgaFrame>();
        AgonMachine::new(AgonMachineConfig {
            uart0_link: Box::new(NullSerialLink),
            uart1_link: Box::new(NullSerialLink),
            soft_reset: Arc::new(AtomicBool::new(false)),
            emulator_shutdown: Arc::new(AtomicBool::new(false)),
            exit_status: Arc::new(AtomicI32::new(-1)),
            paused: Arc::new(AtomicBool::new(false)),
            clockspeed_hz: 18_432_000,
            ram_init: RamInit::Zero,
            mos_bin: PathBuf::new(),
            gpios: Arc::new(GpioSet::new()),
            tx_gpio_vga_frame: frame_tx,
            interrupt_precision: 1,
            external_ram_size: 512 * 1024,
        })
    }

    fn machine_and_cpu(program: &[u8]) -> (AgonMachine, ez80::Cpu) {
        let mut machine = machine();
        for (offset, value) in program.iter().copied().enumerate() {
            machine.poke(LOAD_ADDRESS + offset as u32, value);
        }
        machine.cycle_counter.set(0);
        machine.total_cycles_elapsed = 0;
        machine.mem_out_of_bounds.set(None);

        let mut cpu = ez80::Cpu::new_ez80();
        cpu.state.reg.adl = true;
        cpu.state.set_pc(LOAD_ADDRESS);
        (machine, cpu)
    }

    fn debugger_channels() -> (DebuggerServer, Sender<DebugCmd>, Receiver<DebugResp>) {
        let (response_tx, response_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        (
            DebuggerServer::new(DebuggerConnection {
                tx: response_tx,
                rx: command_rx,
            }),
            command_tx,
            response_rx,
        )
    }

    fn write_code(machine: &mut AgonMachine, start: u32, adl: bool, mbase: u8, bytes: &[u8]) {
        for (offset, value) in bytes.iter().copied().enumerate() {
            machine.poke(code_address(start, offset as u32, adl, mbase), value);
        }
        machine.cycle_counter.set(0);
        machine.total_cycles_elapsed = 0;
        machine.mem_out_of_bounds.set(None);
    }

    fn step_over_successor(program: &[u8], start: u32, adl: bool, mbase: u8) -> u32 {
        let (mut machine, mut cpu) = machine_and_cpu(&[]);
        write_code(&mut machine, start, adl, mbase, program);
        cpu.state.reg.adl = adl;
        cpu.state.reg.mbase = mbase;
        cpu.state.set_pc(start);
        machine.set_paused(true);
        let (mut debugger, _command_tx, response_rx) = debugger_channels();

        debugger
            .handle_debug_cmd(&DebugCmd::StepOver, &mut machine, &mut cpu)
            .unwrap();
        assert!(matches!(response_rx.recv().unwrap(), DebugResp::Resumed));
        assert!(!machine.is_paused());
        assert_eq!(machine.cycle_counter.get(), 0);
        assert_eq!(machine.total_cycles_elapsed, 0);
        assert_eq!(machine.mem_out_of_bounds.get(), None);
        assert_eq!(debugger.triggers.len(), 1);
        debugger.triggers[0].address
    }

    #[test]
    fn debugger_observations_preserve_cpu_cycles_and_markers() {
        let (mut machine, mut cpu) = machine_and_cpu(&[0x3e, 0x2a, 0x00]);
        cpu.state.reg.set24(Reg16::SP, LOAD_ADDRESS + 0x100);
        machine.poke(LOAD_ADDRESS + 0x100, 0xaa);
        machine.poke(LOAD_ADDRESS + 0x101, 0xbb);
        machine.cycle_counter.set(7);
        machine.total_cycles_elapsed = 11;
        machine.mem_out_of_bounds.set(Some(0x123456));
        machine.io_unhandled.set(Some(0x3456));
        let registers_before = format!("{:?}", cpu.state.reg);
        let pc_before = cpu.state.pc();
        let instructions_before = cpu.state.instructions_executed;

        let (mut debugger, _command_tx, response_rx) = debugger_channels();
        debugger
            .handle_debug_cmd(
                &DebugCmd::GetMemory {
                    start: LOAD_ADDRESS,
                    len: 3,
                },
                &mut machine,
                &mut cpu,
            )
            .unwrap();
        match response_rx.recv().unwrap() {
            DebugResp::Memory { start, data } => {
                assert_eq!(start, LOAD_ADDRESS);
                assert_eq!(data, [0x3e, 0x2a, 0x00]);
            }
            response => panic!("unexpected response: {response:?}"),
        }

        debugger
            .handle_debug_cmd(&DebugCmd::GetState, &mut machine, &mut cpu)
            .unwrap();
        match response_rx.recv().unwrap() {
            DebugResp::State {
                stack,
                pc_instruction,
                ..
            } => {
                assert_eq!(&stack[..2], &[0xaa, 0xbb]);
                assert!(!pc_instruction.is_empty());
            }
            response => panic!("unexpected response: {response:?}"),
        }

        debugger
            .handle_debug_cmd(
                &DebugCmd::DisassemblePc { adl: None },
                &mut machine,
                &mut cpu,
            )
            .unwrap();
        match response_rx.recv().unwrap() {
            DebugResp::Disassembly { disasm, .. } => {
                assert_eq!(disasm[0].loc, LOAD_ADDRESS);
                assert_eq!(disasm[0].bytes, [0x3e, 0x2a]);
            }
            response => panic!("unexpected response: {response:?}"),
        }

        assert_eq!(format!("{:?}", cpu.state.reg), registers_before);
        assert_eq!(cpu.state.pc(), pc_before);
        assert_eq!(cpu.state.instructions_executed, instructions_before);
        assert_eq!(machine.cycle_counter.get(), 7);
        assert_eq!(machine.total_cycles_elapsed, 11);
        assert_eq!(machine.mem_out_of_bounds.get(), Some(0x123456));
        assert_eq!(machine.io_unhandled.get(), Some(0x3456));
    }

    #[test]
    fn step_and_step_over_fallback_share_settled_execution() {
        fn run(command: DebugCmd) -> (u32, u64, u64, String) {
            let (mut machine, mut cpu) = machine_and_cpu(&[0x00]);
            machine.set_paused(true);
            let (mut debugger, _command_tx, response_rx) = debugger_channels();
            debugger
                .handle_debug_cmd(&command, &mut machine, &mut cpu)
                .unwrap();
            assert!(matches!(
                response_rx.recv().unwrap(),
                DebugResp::State { .. }
            ));
            assert!(machine.is_paused());
            assert_eq!(machine.cycle_counter.get(), 0);
            (
                cpu.state.pc(),
                cpu.state.instructions_executed,
                machine.total_cycles_elapsed,
                format!("{:?}", cpu.state.reg),
            )
        }

        let stepped = run(DebugCmd::Step);
        assert_eq!(stepped, run(DebugCmd::StepOver));
        assert_eq!(stepped.0, LOAD_ADDRESS + 1);
        assert_eq!(stepped.1, 1);
        assert!(stepped.2 > 0);
    }

    #[test]
    fn memory_fault_is_reported_once_and_continue_acknowledges_it() {
        let (mut machine, mut cpu) = machine_and_cpu(&[0x00]);
        machine.last_pc = LOAD_ADDRESS;
        machine.mem_out_of_bounds.set(Some(0x123456));
        let (mut debugger, command_tx, response_rx) = debugger_channels();

        debugger.tick(&mut machine, &mut cpu).unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Paused(PauseReason::OutOfBoundsMemAccess(0x123456))
        ));
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Disassembly { .. }
        ));
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::State { .. }
        ));
        assert!(machine.is_paused());
        assert_eq!(machine.mem_out_of_bounds.get(), Some(0x123456));

        debugger.tick(&mut machine, &mut cpu).unwrap();
        assert!(matches!(
            response_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        command_tx.send(DebugCmd::Continue).unwrap();
        debugger.tick(&mut machine, &mut cpu).unwrap();
        assert!(matches!(response_rx.recv().unwrap(), DebugResp::Resumed));
        assert!(!machine.is_paused());
        assert_eq!(machine.mem_out_of_bounds.get(), None);

        machine.mem_out_of_bounds.set(Some(0x123456));
        debugger.tick(&mut machine, &mut cpu).unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Paused(PauseReason::OutOfBoundsMemAccess(0x123456))
        ));
    }

    #[test]
    fn disconnects_are_fallible_and_paused_one_shot_is_retained() {
        let (mut machine, mut cpu) = machine_and_cpu(&[0x00]);
        let (mut debugger, command_tx, response_rx) = debugger_channels();
        debugger.triggers.push(Trigger {
            address: LOAD_ADDRESS,
            once: true,
            actions: vec![DebugCmd::Message("triggered".to_string())],
        });

        machine.set_paused(true);
        debugger.tick(&mut machine, &mut cpu).unwrap();
        assert_eq!(debugger.triggers.len(), 1);
        machine.set_paused(false);
        debugger.tick(&mut machine, &mut cpu).unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Message(message) if message == "triggered"
        ));
        assert!(debugger.triggers.is_empty());
        drop(command_tx);
        assert_eq!(
            debugger.tick(&mut machine, &mut cpu),
            Err(DebuggerError::ControllerDisconnected)
        );

        let (mut debugger, command_tx, response_rx) = debugger_channels();
        drop(response_rx);
        assert_eq!(
            debugger.handle_debug_cmd(&DebugCmd::Ping, &mut machine, &mut cpu),
            Err(DebuggerError::ControllerDisconnected)
        );
        drop(command_tx);

        let (response_tx, response_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let mut attached = Some(DebuggerServer::new(DebuggerConnection {
            tx: response_tx,
            rx: command_rx,
        }));
        machine.set_paused(true);
        machine.mem_out_of_bounds.set(Some(0x123456));
        drop(response_rx);
        command_tx.send(DebugCmd::Ping).unwrap();
        machine.debugger_tick(&mut attached, &mut cpu);
        assert!(attached.is_none());
        assert!(!machine.is_paused());
        assert_eq!(machine.mem_out_of_bounds.get(), None);
    }

    #[test]
    fn step_over_inspection_is_neutral_and_wraps_non_adl_pc() {
        assert_eq!(
            step_over_successor(&[0xcd, 0x34, 0x12], 0x04ffff, false, 4),
            0x040002
        );
        assert_eq!(
            step_over_successor(&[0x5b, 0xdd, 0xff], 0xfffffd, true, 0),
            0
        );
    }

    #[test]
    fn disassembly_fetches_immediates_in_pc_mode_across_size_overrides() {
        fn disassemble_case(program: &[u8], start: u32, adl: bool, mbase: u8) -> (String, Vec<u8>) {
            let (mut machine, mut cpu) = machine_and_cpu(&[]);
            write_code(&mut machine, start, adl, mbase, program);
            cpu.state.reg.adl = adl;
            cpu.state.reg.mbase = mbase;
            cpu.state.set_pc(start);

            let disasm = neutral_disassemble(&machine, &cpu, None, start, start + 1);
            assert_eq!(disasm.len(), 1);
            assert_eq!(machine.cycle_counter.get(), 0);
            assert_eq!(machine.total_cycles_elapsed, 0);
            assert_eq!(machine.mem_out_of_bounds.get(), None);
            assert_eq!(cpu.state.instructions_executed, 0);
            assert_eq!(cpu.state.pc(), start);
            (disasm[0].asm.clone(), disasm[0].bytes.clone())
        }

        let (text, bytes) = disassemble_case(&[0x5b, 0xcd, 0x11, 0x22, 0x00], 0xfffffc, true, 0);
        assert_eq!(bytes, [0x5b, 0xcd, 0x11, 0x22, 0x00]);
        assert!(text.contains("$2211"));

        let (text, bytes) = disassemble_case(&[0x5b, 0x21, 0x11, 0x22, 0x33], 0x04fffd, false, 4);
        assert_eq!(bytes, [0x5b, 0x21, 0x11, 0x22, 0x33]);
        assert!(text.contains("$332211"));

        let (text, bytes) = disassemble_case(&[0x40, 0x21, 0x11, 0x22], 0x04fffd, true, 0);
        assert_eq!(bytes, [0x40, 0x21, 0x11, 0x22]);
        assert!(text.contains("$2211"));
    }

    #[test]
    fn step_over_scans_size_and_index_prefixes_in_decoder_order() {
        let size_prefixes = [(0x40, false), (0x49, false), (0x52, true), (0x5b, true)];
        for adl in [false, true] {
            let mbase = 4;
            for (prefix, immediate_long) in size_prefixes {
                let program = [prefix, 0xcd, 0x34, 0x12, 0x04];
                let len = 2 + if immediate_long { 3 } else { 2 };
                assert_eq!(
                    step_over_successor(&program, LOAD_ADDRESS, adl, mbase),
                    code_address(LOAD_ADDRESS, len, adl, mbase)
                );
            }

            assert_eq!(
                step_over_successor(
                    &[0x52, 0x40, 0xdd, 0xfd, 0xdd, 0xcd, 0x34, 0x12],
                    LOAD_ADDRESS,
                    adl,
                    mbase,
                ),
                code_address(LOAD_ADDRESS, 8, adl, mbase)
            );
            assert_eq!(
                step_over_successor(
                    &[0x40, 0x5b, 0xfd, 0xdd, 0xfd, 0xcd, 0x34, 0x12, 0x04],
                    LOAD_ADDRESS,
                    adl,
                    mbase,
                ),
                code_address(LOAD_ADDRESS, 9, adl, mbase)
            );
            assert_eq!(
                step_over_successor(
                    &[0xdd, 0xfd, 0xdd, 0xcd, 0x34, 0x12, 0x04],
                    LOAD_ADDRESS,
                    adl,
                    mbase,
                ),
                code_address(LOAD_ADDRESS, if adl { 7 } else { 6 }, adl, mbase)
            );
        }
    }

    #[test]
    fn repeated_prefix_disassembly_and_full_span_fallback_are_bounded() {
        let (mut machine, mut cpu) = machine_and_cpu(&[]);
        let mut repeated = vec![0x40; 24];
        repeated.push(0x00);
        write_code(&mut machine, LOAD_ADDRESS, true, 0, &repeated);
        cpu.state.reg.adl = true;
        cpu.state.set_pc(LOAD_ADDRESS);
        let disasm = neutral_disassemble(&machine, &cpu, None, LOAD_ADDRESS, LOAD_ADDRESS + 1);
        assert_eq!(disasm.len(), 1);
        assert_eq!(disasm[0].bytes, repeated);

        for offset in 0..0x1_0000 {
            machine.poke(LOAD_ADDRESS + offset, 0x40);
        }
        machine.cycle_counter.set(0);
        machine.total_cycles_elapsed = 0;
        machine.mem_out_of_bounds.set(None);
        cpu.state.reg.adl = false;
        cpu.state.reg.mbase = 4;
        cpu.state.set_pc(LOAD_ADDRESS);
        assert_eq!(
            scan_ez80_prefixes(&machine, LOAD_ADDRESS, false, 4),
            Err(PrefixScanError::FullSpan)
        );
        let disasm = neutral_disassemble(&machine, &cpu, None, LOAD_ADDRESS, LOAD_ADDRESS + 16);
        assert_eq!(disasm.len(), 1);
        assert!(disasm[0].asm.contains("full address span"));
        assert!(disasm[0].bytes.is_empty());

        machine.set_paused(true);
        let (mut debugger, _command_tx, response_rx) = debugger_channels();
        debugger
            .handle_debug_cmd(&DebugCmd::StepOver, &mut machine, &mut cpu)
            .unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Message(message) if message.contains("complete code-address span")
        ));
        assert!(machine.is_paused());
        assert_eq!(cpu.state.instructions_executed, 0);
        assert!(debugger.triggers.is_empty());
    }

    #[test]
    fn mixed_prefix_phases_each_get_a_full_address_span() {
        let (mut machine, mut cpu) = machine_and_cpu(&[]);
        for offset in 0..0x1_0000 {
            machine.poke(LOAD_ADDRESS + offset, 0xdd);
        }
        machine.poke(LOAD_ADDRESS, 0x40);
        machine.cycle_counter.set(0);
        machine.total_cycles_elapsed = 0;
        machine.mem_out_of_bounds.set(None);
        cpu.state.reg.adl = false;
        cpu.state.reg.mbase = 4;
        cpu.state.set_pc(LOAD_ADDRESS);

        assert_eq!(
            scan_ez80_prefixes(&machine, LOAD_ADDRESS, false, 4),
            Ok(PrefixScan {
                opcode: 0x40,
                prefix_len: 0x1_0000,
                immediate_long: false,
            })
        );

        let disasm = neutral_disassemble(&machine, &cpu, None, LOAD_ADDRESS, LOAD_ADDRESS + 1);
        assert_eq!(disasm.len(), 1);
        assert_eq!(disasm[0].bytes.len(), 0x1_0001);
        assert_eq!(disasm[0].bytes.first(), Some(&0x40));
        assert_eq!(disasm[0].bytes.last(), Some(&0x40));
        assert!(!disasm[0].asm.contains("full address span"));

        machine.set_paused(true);
        let (mut debugger, _command_tx, response_rx) = debugger_channels();
        debugger
            .handle_debug_cmd(&DebugCmd::StepOver, &mut machine, &mut cpu)
            .unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::State { .. }
        ));
        assert!(machine.is_paused());
        assert_eq!(cpu.state.instructions_executed, 1);
        assert_eq!(cpu.state.pc(), LOAD_ADDRESS + 1);
        assert!(debugger.triggers.is_empty());
    }

    #[test]
    fn unmapped_step_over_executes_through_the_real_fault_path() {
        let (mut machine, mut cpu) = machine_and_cpu(&[]);
        machine.port_out(0xa8, 4);
        machine.port_out(0xa9, 4);
        machine.cycle_counter.set(0);
        machine.total_cycles_elapsed = 0;
        machine.mem_out_of_bounds.set(None);
        cpu.state.reg.adl = true;
        cpu.state.set_pc(0x050000);
        machine.set_paused(true);
        assert_eq!(
            scan_ez80_prefixes(&machine, 0x050000, true, 0),
            Err(PrefixScanError::Unmapped { address: 0x050000 })
        );

        let (mut debugger, _command_tx, response_rx) = debugger_channels();
        debugger
            .handle_debug_cmd(&DebugCmd::StepOver, &mut machine, &mut cpu)
            .unwrap();

        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::State { .. }
        ));
        assert!(machine.is_paused());
        assert_eq!(cpu.state.instructions_executed, 1);
        assert!(machine.mem_out_of_bounds.get().is_some());
        assert!(debugger.triggers.is_empty());
    }

    #[test]
    fn debugger_caps_memory_handles_adl_af_and_reports_override_mode() {
        let (mut machine, mut cpu) = machine_and_cpu(&[0x00]);
        cpu.state.reg.adl = true;
        cpu.state.reg.mbase = 0xff;
        cpu.state.reg.set16(Reg16::AF, 0xe000);
        machine.cycle_counter.set(0);
        machine.mem_out_of_bounds.set(None);
        let (mut debugger, _command_tx, response_rx) = debugger_channels();

        debugger
            .handle_debug_cmd(
                &DebugCmd::GetMemoryAtReg {
                    reg: Reg16::AF,
                    len: 1,
                },
                &mut machine,
                &mut cpu,
            )
            .unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Memory { start: 0x00e000, data } if data == [0]
        ));

        debugger
            .handle_debug_cmd(
                &DebugCmd::GetMemory {
                    start: LOAD_ADDRESS,
                    len: u32::MAX,
                },
                &mut machine,
                &mut cpu,
            )
            .unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Memory { data, .. } if data.len() == MAX_DEBUGGER_MEMORY_BYTES as usize
        ));

        debugger
            .handle_debug_cmd(
                &DebugCmd::DisassemblePc { adl: Some(false) },
                &mut machine,
                &mut cpu,
            )
            .unwrap();
        assert!(matches!(
            response_rx.recv().unwrap(),
            DebugResp::Disassembly { adl: false, .. }
        ));
    }
}
