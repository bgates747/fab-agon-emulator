use super::AgonMachine;
use crate::debugger::{code_address, scan_ez80_prefixes, PrefixScanError};
use ez80::{Cpu, Machine, Reg16};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::atomic::Ordering;

const MAX_ADDRESS: u32 = 0x00ff_ffff;
const ADDRESS_SPACE_SIZE: u64 = 0x0100_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadImage {
    pub address: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectEntry {
    pub pc: u32,
    pub adl: bool,
    pub madl: bool,
    pub mbase: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessBoot {
    Direct(DirectEntry),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub max_instructions: NonZeroU64,
    pub max_cycles: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessSessionConfig {
    pub boot: HeadlessBoot,
    pub images: Vec<LoadImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessRunConfig {
    pub boot: HeadlessBoot,
    pub images: Vec<LoadImage>,
    pub limits: RunLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    GuestExit(u8),
    RequestedShutdown,
    Halted,
    MemoryFault { address: u32, instruction_pc: u32 },
    InstructionLimit,
    CycleLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadlessStepOutcome {
    pub reason: Option<StopReason>,
    pub guest_status: Option<u8>,
    pub pc: u32,
    pub instructions: u64,
    pub cycles: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadlessRunOutcome {
    pub reason: StopReason,
    pub guest_status: Option<u8>,
    pub pc: u32,
    pub instructions: u64,
    pub cycles: u64,
}

#[derive(Debug, Clone)]
pub struct HeadlessSnapshot {
    pub registers: ez80::Registers,
    pub pc: u32,
    pub instructions: u64,
    pub cycles: u64,
    pub guest_status: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessMemory {
    pub start: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveStackKind {
    Spl24,
    Sps16 { mbase: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessStack {
    pub kind: ActiveStackKind,
    pub start: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessInstruction {
    pub address: u32,
    pub adl: bool,
    pub text: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessObservationError {
    AddressOutOfRange { address: u32 },
    RangeOutOfRange { start: u32, len: u32 },
    UnmappedAddress { address: u32 },
    StackWindowTooLong { len: u32, adl: bool },
    InstructionPrefixCycle { address: u32, adl: bool },
}

impl fmt::Display for HeadlessObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddressOutOfRange { address } => {
                write!(
                    f,
                    "observation address is outside 24-bit space: ${address:08x}"
                )
            }
            Self::RangeOutOfRange { start, len } => write!(
                f,
                "observation range ${start:06x}+{len} leaves 24-bit space"
            ),
            Self::UnmappedAddress { address } => {
                write!(f, "observation address ${address:06x} is unmapped")
            }
            Self::StackWindowTooLong { len, adl } => {
                let width = if *adl { "24-bit SPL" } else { "16-bit SPS" };
                write!(f, "stack observation length {len} exceeds one {width} span")
            }
            Self::InstructionPrefixCycle { address, adl } => {
                let width = if *adl { "24-bit" } else { "16-bit" };
                write!(
                    f,
                    "instruction at ${address:06x} contains a complete {width} address span of eZ80 prefixes"
                )
            }
        }
    }
}

impl std::error::Error for HeadlessObservationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessRunError {
    MachineAlreadyUsed,
    NoImages,
    EmptyImage {
        image: usize,
    },
    AddressOutOfRange {
        image: usize,
        address: u32,
    },
    RangeOutOfRange {
        image: usize,
        address: u32,
        len: usize,
    },
    ReadOnlyAddress {
        image: usize,
        address: u32,
    },
    UnmappedAddress {
        image: usize,
        address: u32,
    },
    PhysicalWrap {
        image: usize,
        address: u32,
    },
    AliasedAddress {
        image: usize,
        address: u32,
        previous_image: usize,
        previous_address: u32,
    },
    EntryOutOfRange {
        entry: u32,
    },
    EntryMbaseMismatch {
        entry: u32,
        mbase: u8,
    },
    InstructionAccountingInvariant {
        before: u64,
        after: u64,
    },
    CycleAccountingInvariant {
        pending: i32,
    },
    CycleAccountingOverflow,
}

impl fmt::Display for HeadlessRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MachineAlreadyUsed => write!(f, "machine has already executed or faulted"),
            Self::NoImages => write!(f, "direct headless boot requires at least one image"),
            Self::EmptyImage { image } => write!(f, "image {image} is empty"),
            Self::AddressOutOfRange { image, address } => {
                write!(f, "image {image} starts outside 24-bit space at ${address:08x}")
            }
            Self::RangeOutOfRange {
                image,
                address,
                len,
            } => write!(
                f,
                "image {image} range ${address:06x}+{len} leaves 24-bit space"
            ),
            Self::ReadOnlyAddress { image, address } => {
                write!(f, "image {image} targets read-only ROM at ${address:06x}")
            }
            Self::UnmappedAddress { image, address } => {
                write!(f, "image {image} targets unmapped address ${address:06x}")
            }
            Self::PhysicalWrap { image, address } => write!(
                f,
                "image {image} wraps its physical RAM backing at ${address:06x}"
            ),
            Self::AliasedAddress {
                image,
                address,
                previous_image,
                previous_address,
            } => write!(
                f,
                "image {image} address ${address:06x} aliases image {previous_image} address ${previous_address:06x}"
            ),
            Self::EntryOutOfRange { entry } => {
                write!(f, "entry PC is outside 24-bit space: ${entry:08x}")
            }
            Self::EntryMbaseMismatch { entry, mbase } => write!(
                f,
                "non-ADL entry ${entry:06x} does not use declared MBASE ${mbase:02x}"
            ),
            Self::InstructionAccountingInvariant { before, after } => write!(
                f,
                "one headless step did not execute exactly one instruction ({before} -> {after})"
            ),
            Self::CycleAccountingInvariant { pending } => write!(
                f,
                "one headless instruction produced a non-positive cycle count ({pending})"
            ),
            Self::CycleAccountingOverflow => write!(f, "guest cycle counter overflowed"),
        }
    }
}

impl std::error::Error for HeadlessRunError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StorageAddress {
    Internal(usize),
    External(usize),
}

pub struct HeadlessSession {
    machine: AgonMachine,
    cpu: Cpu,
    terminal_stop: Option<StopReason>,
}

pub(crate) struct SettledInstruction {
    pub instructions_before: u64,
    pub instructions_after: u64,
    pub cycles_before: u64,
    pub cycles_after: u64,
    pub instruction_cycles: i32,
    pub interrupt_cycles: i32,
    pub memory_fault: Option<u32>,
}

impl AgonMachine {
    pub fn begin_headless(
        mut self,
        config: HeadlessSessionConfig,
    ) -> Result<HeadlessSession, HeadlessRunError> {
        if self.cycle_counter.get() != 0
            || self.total_cycles_elapsed != 0
            || self.mem_out_of_bounds.get().is_some()
            || self.io_unhandled.get().is_some()
            || self.guest_exit_status.is_some()
        {
            return Err(HeadlessRunError::MachineAlreadyUsed);
        }

        let targets = self.validate_images(&config.images)?;
        let HeadlessBoot::Direct(entry) = config.boot;
        validate_entry(entry)?;

        self.initialize_ram();
        for (image, image_targets) in config.images.iter().zip(targets) {
            for (value, target) in image.bytes.iter().copied().zip(image_targets) {
                match target {
                    StorageAddress::Internal(address) => self.mem_internal[address] = value,
                    StorageAddress::External(address) => self.mem_external[address] = value,
                }
            }
        }

        self.headless_mode = true;
        self.enable_hostfs = false;
        self.guest_exit_status = None;

        let mut cpu = Cpu::new_ez80();
        cpu.state.reg.adl = entry.adl;
        cpu.state.reg.madl = entry.madl;
        cpu.state.reg.mbase = entry.mbase;
        cpu.state.set_pc(entry.pc);

        Ok(HeadlessSession {
            machine: self,
            cpu,
            terminal_stop: None,
        })
    }

    pub fn run_headless(
        self,
        config: HeadlessRunConfig,
    ) -> Result<HeadlessRunOutcome, HeadlessRunError> {
        let mut session = self.begin_headless(HeadlessSessionConfig {
            boot: config.boot,
            images: config.images,
        })?;
        session.run(config.limits)
    }

    fn validate_images(
        &self,
        images: &[LoadImage],
    ) -> Result<Vec<Vec<StorageAddress>>, HeadlessRunError> {
        if images.is_empty() {
            return Err(HeadlessRunError::NoImages);
        }
        let mut occupied = HashMap::<StorageAddress, (usize, u32)>::new();
        let mut all_targets = Vec::with_capacity(images.len());

        for (image_index, image) in images.iter().enumerate() {
            if image.bytes.is_empty() {
                return Err(HeadlessRunError::EmptyImage { image: image_index });
            }
            if image.address > MAX_ADDRESS {
                return Err(HeadlessRunError::AddressOutOfRange {
                    image: image_index,
                    address: image.address,
                });
            }

            let end = u64::from(image.address)
                .checked_add(image.bytes.len() as u64)
                .ok_or(HeadlessRunError::RangeOutOfRange {
                    image: image_index,
                    address: image.address,
                    len: image.bytes.len(),
                })?;
            if end > ADDRESS_SPACE_SIZE {
                return Err(HeadlessRunError::RangeOutOfRange {
                    image: image_index,
                    address: image.address,
                    len: image.bytes.len(),
                });
            }

            let mut image_targets = Vec::with_capacity(image.bytes.len());
            for offset in 0..image.bytes.len() {
                let address = image.address + offset as u32;
                let target = self.writable_storage_address(image_index, address)?;
                if let Some(previous) = image_targets.last() {
                    let wrapped = match (*previous, target) {
                        (StorageAddress::Internal(a), StorageAddress::Internal(b))
                        | (StorageAddress::External(a), StorageAddress::External(b)) => {
                            a.checked_add(1) != Some(b)
                        }
                        _ => false,
                    };
                    if wrapped {
                        return Err(HeadlessRunError::PhysicalWrap {
                            image: image_index,
                            address,
                        });
                    }
                }
                if let Some((previous_image, previous_address)) = occupied.get(&target) {
                    return Err(HeadlessRunError::AliasedAddress {
                        image: image_index,
                        address,
                        previous_image: *previous_image,
                        previous_address: *previous_address,
                    });
                }
                occupied.insert(target, (image_index, address));
                image_targets.push(target);
            }
            all_targets.push(image_targets);
        }

        Ok(all_targets)
    }

    fn writable_storage_address(
        &self,
        image: usize,
        address: u32,
    ) -> Result<StorageAddress, HeadlessRunError> {
        if let Some(address) = self.get_internal_ram_address(address) {
            return Ok(StorageAddress::Internal(address as usize));
        }
        if self.get_rom_address(address).is_some() {
            return Err(HeadlessRunError::ReadOnlyAddress { image, address });
        }
        if let Some(address) = self.get_external_ram_address(address) {
            return Ok(StorageAddress::External(address as usize));
        }
        Err(HeadlessRunError::UnmappedAddress { image, address })
    }

    pub(crate) fn observed_byte(&self, address: u32) -> Option<u8> {
        if address > MAX_ADDRESS {
            return None;
        }
        if let Some(address) = self.get_internal_ram_address(address) {
            Some(self.mem_internal[address as usize])
        } else if let Some(address) = self.get_rom_address(address) {
            Some(self.mem_rom[address as usize])
        } else {
            self.get_external_ram_address(address)
                .map(|address| self.mem_external[address as usize])
        }
    }

    pub(crate) fn execute_and_settle_instruction(&mut self, cpu: &mut Cpu) -> SettledInstruction {
        self.mem_out_of_bounds.set(None);
        let instructions_before = cpu.state.instructions_executed;
        let cycles_before = self.total_cycles_elapsed;
        self.execute_instruction(cpu);
        let instructions_after = cpu.state.instructions_executed;
        let instruction_cycles = self.apply_elapsed_cycles();

        if self.mem_out_of_bounds.get().is_none()
            && self.guest_exit_status.is_none()
            && !self.emulator_shutdown.load(Ordering::Relaxed)
            && !cpu.is_halted()
        {
            self.do_interrupts(cpu);
        }
        let interrupt_cycles = self.apply_elapsed_cycles();
        let cycles_after = self.total_cycles_elapsed;
        debug_assert_eq!(self.cycle_counter.get(), 0);

        SettledInstruction {
            instructions_before,
            instructions_after,
            cycles_before,
            cycles_after,
            instruction_cycles,
            interrupt_cycles,
            memory_fault: self.mem_out_of_bounds.get(),
        }
    }
}

pub(crate) struct ObservationMachine<'a> {
    pub(crate) machine: &'a AgonMachine,
    pub(crate) start: u32,
    pub(crate) next_offset: Cell<u32>,
    pub(crate) adl: bool,
    pub(crate) mbase: u8,
}

impl Machine for ObservationMachine<'_> {
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

impl HeadlessSession {
    pub fn step(&mut self) -> Result<HeadlessStepOutcome, HeadlessRunError> {
        if let Some(reason) = self.terminal_reason() {
            return Ok(self.step_outcome(Some(reason)));
        }

        let settled = self.machine.execute_and_settle_instruction(&mut self.cpu);
        if settled.instructions_after != settled.instructions_before.saturating_add(1) {
            return Err(HeadlessRunError::InstructionAccountingInvariant {
                before: settled.instructions_before,
                after: settled.instructions_after,
            });
        }
        if settled.instruction_cycles <= 0 {
            return Err(HeadlessRunError::CycleAccountingInvariant {
                pending: settled.instruction_cycles,
            });
        }
        if settled.interrupt_cycles < 0 {
            return Err(HeadlessRunError::CycleAccountingInvariant {
                pending: settled.interrupt_cycles,
            });
        }
        if settled.cycles_after < settled.cycles_before {
            return Err(HeadlessRunError::CycleAccountingOverflow);
        }

        let reason = if let Some(address) = settled.memory_fault {
            let reason = StopReason::MemoryFault {
                address,
                instruction_pc: self.machine.last_pc,
            };
            self.terminal_stop = Some(reason);
            Some(reason)
        } else {
            self.terminal_reason()
        };
        Ok(self.step_outcome(reason))
    }

    pub fn run(&mut self, limits: RunLimits) -> Result<HeadlessRunOutcome, HeadlessRunError> {
        loop {
            if let Some(reason) = self.terminal_reason() {
                return Ok(self.run_outcome(reason));
            }
            if self.cpu.state.instructions_executed >= limits.max_instructions.get() {
                return Ok(self.run_outcome(StopReason::InstructionLimit));
            }
            if self.machine.total_cycles_elapsed >= limits.max_cycles.get() {
                return Ok(self.run_outcome(StopReason::CycleLimit));
            }

            let step = self.step()?;
            if step.cycles > limits.max_cycles.get() {
                return Ok(self.run_outcome(StopReason::CycleLimit));
            }
            if let Some(reason) = step.reason {
                return Ok(self.run_outcome(reason));
            }
            if step.instructions >= limits.max_instructions.get() {
                return Ok(self.run_outcome(StopReason::InstructionLimit));
            }
            if step.cycles >= limits.max_cycles.get() {
                return Ok(self.run_outcome(StopReason::CycleLimit));
            }
        }
    }

    pub fn pc(&self) -> u32 {
        self.cpu.state.pc()
    }

    pub fn instructions_executed(&self) -> u64 {
        self.cpu.state.instructions_executed
    }

    pub fn total_cycles_elapsed(&self) -> u64 {
        self.machine.total_cycles_elapsed
    }

    pub fn registers(&self) -> &ez80::Registers {
        &self.cpu.state.reg
    }

    pub fn snapshot(&self) -> HeadlessSnapshot {
        HeadlessSnapshot {
            registers: self.cpu.state.reg.clone(),
            pc: self.pc(),
            instructions: self.instructions_executed(),
            cycles: self.total_cycles_elapsed(),
            guest_status: self.machine.guest_exit_status,
        }
    }

    pub fn read_memory(
        &self,
        start: u32,
        len: NonZeroU32,
    ) -> Result<HeadlessMemory, HeadlessObservationError> {
        if start > MAX_ADDRESS {
            return Err(HeadlessObservationError::AddressOutOfRange { address: start });
        }
        let len = len.get();
        let end = u64::from(start) + u64::from(len);
        if end > ADDRESS_SPACE_SIZE {
            return Err(HeadlessObservationError::RangeOutOfRange { start, len });
        }

        let mut bytes = Vec::with_capacity(len as usize);
        for offset in 0..len {
            let address = start + offset;
            let value = self
                .machine
                .observed_byte(address)
                .ok_or(HeadlessObservationError::UnmappedAddress { address })?;
            bytes.push(value);
        }
        Ok(HeadlessMemory { start, bytes })
    }

    pub fn read_active_stack(
        &self,
        len: NonZeroU32,
    ) -> Result<HeadlessStack, HeadlessObservationError> {
        let registers = &self.cpu.state.reg;
        let len = len.get();
        let (kind, start, span) = if registers.adl {
            (
                ActiveStackKind::Spl24,
                registers.get24(Reg16::SP),
                ADDRESS_SPACE_SIZE,
            )
        } else {
            (
                ActiveStackKind::Sps16 {
                    mbase: registers.mbase,
                },
                registers.get16_mbase(Reg16::SP),
                0x1_0000,
            )
        };
        if u64::from(len) > span {
            return Err(HeadlessObservationError::StackWindowTooLong {
                len,
                adl: registers.adl,
            });
        }

        let mut bytes = Vec::with_capacity(len as usize);
        for offset in 0..len {
            let address = if registers.adl {
                start.wrapping_add(offset) & MAX_ADDRESS
            } else {
                (u32::from(registers.mbase) << 16)
                    | (u32::from(registers.get16(Reg16::SP)).wrapping_add(offset) & 0xffff)
            };
            let value = self
                .machine
                .observed_byte(address)
                .ok_or(HeadlessObservationError::UnmappedAddress { address })?;
            bytes.push(value);
        }
        Ok(HeadlessStack { kind, start, bytes })
    }

    pub fn disassemble_pc(&self) -> Result<HeadlessInstruction, HeadlessObservationError> {
        let address = self.pc();
        let adl = self.cpu.state.reg.adl;
        let mbase = self.cpu.state.reg.mbase;
        let prefix_scan = match scan_ez80_prefixes(&self.machine, address, adl, mbase) {
            Ok(scan) => scan,
            Err(PrefixScanError::Unmapped { address }) => {
                return Err(HeadlessObservationError::UnmappedAddress { address });
            }
            Err(PrefixScanError::FullSpan) => {
                return Err(HeadlessObservationError::InstructionPrefixCycle { address, adl });
            }
        };
        let mut cpu = Cpu::new_ez80();
        cpu.state = self.cpu.state.clone();
        let mut machine = ObservationMachine {
            machine: &self.machine,
            start: address,
            next_offset: Cell::new(0),
            adl,
            mbase,
        };
        let text = cpu.disasm_instruction(&mut machine);

        let modular_len = if adl {
            cpu.state.pc().wrapping_sub(address) & MAX_ADDRESS
        } else {
            u32::from((cpu.state.reg.pc as u16).wrapping_sub(address as u16))
        };
        let span = if adl {
            ADDRESS_SPACE_SIZE as u32
        } else {
            0x1_0000
        };
        let minimum_len = prefix_scan.prefix_len + 1;
        let mut len = if modular_len == 0 { span } else { modular_len };
        if len < minimum_len {
            len += span;
        }
        // disasm_instruction re-reads immediate bytes while advancing its
        // scratch PC, so only this architectural instruction range decides
        // whether an unmapped byte is material.
        let mut bytes = Vec::with_capacity(len as usize);
        for offset in 0..len {
            let byte_address = code_address(address, offset, adl, mbase);
            let value = self.machine.observed_byte(byte_address).ok_or(
                HeadlessObservationError::UnmappedAddress {
                    address: byte_address,
                },
            )?;
            bytes.push(value);
        }

        Ok(HeadlessInstruction {
            address,
            adl,
            text,
            bytes,
        })
    }

    fn terminal_reason(&self) -> Option<StopReason> {
        if let Some(reason) = self.terminal_stop {
            Some(reason)
        } else if let Some(status) = self.machine.guest_exit_status {
            Some(StopReason::GuestExit(status))
        } else if self.machine.emulator_shutdown.load(Ordering::Relaxed) {
            Some(StopReason::RequestedShutdown)
        } else if self.cpu.is_halted() {
            Some(StopReason::Halted)
        } else {
            None
        }
    }

    fn step_outcome(&self, reason: Option<StopReason>) -> HeadlessStepOutcome {
        HeadlessStepOutcome {
            reason,
            guest_status: self.machine.guest_exit_status,
            pc: self.pc(),
            instructions: self.instructions_executed(),
            cycles: self.total_cycles_elapsed(),
        }
    }

    fn run_outcome(&self, reason: StopReason) -> HeadlessRunOutcome {
        HeadlessRunOutcome {
            reason,
            guest_status: self.machine.guest_exit_status,
            pc: self.pc(),
            instructions: self.instructions_executed(),
            cycles: self.total_cycles_elapsed(),
        }
    }
}

fn validate_entry(entry: DirectEntry) -> Result<(), HeadlessRunError> {
    if entry.pc > MAX_ADDRESS {
        return Err(HeadlessRunError::EntryOutOfRange { entry: entry.pc });
    }
    if !entry.adl && (entry.pc >> 16) as u8 != entry.mbase {
        return Err(HeadlessRunError::EntryMbaseMismatch {
            entry: entry.pc,
            mbase: entry.mbase,
        });
    }
    Ok(())
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
    const GUEST_42: &[u8] = &[0x3e, 0x2a, 0xd3, 0x00];

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

    fn machine(requested_shutdown: bool) -> AgonMachine {
        let (frame_tx, _frame_rx) = mpsc::channel::<GpioVgaFrame>();
        AgonMachine::new(AgonMachineConfig {
            uart0_link: Box::new(NullSerialLink),
            uart1_link: Box::new(NullSerialLink),
            soft_reset: Arc::new(AtomicBool::new(false)),
            emulator_shutdown: Arc::new(AtomicBool::new(requested_shutdown)),
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

    fn direct(bytes: &[u8], max_instructions: u64, max_cycles: u64) -> HeadlessRunConfig {
        HeadlessRunConfig {
            boot: HeadlessBoot::Direct(DirectEntry {
                pc: LOAD_ADDRESS,
                adl: true,
                madl: false,
                mbase: 0,
            }),
            images: vec![LoadImage {
                address: LOAD_ADDRESS,
                bytes: bytes.to_vec(),
            }],
            limits: RunLimits {
                max_instructions: NonZeroU64::new(max_instructions).unwrap(),
                max_cycles: NonZeroU64::new(max_cycles).unwrap(),
            },
        }
    }

    #[test]
    fn direct_adl_guest_exit_is_finite_and_settled() {
        let outcome = machine(false)
            .run_headless(direct(GUEST_42, 2, 100))
            .unwrap();
        assert_eq!(outcome.reason, StopReason::GuestExit(0x2a));
        assert_eq!(outcome.guest_status, Some(0x2a));
        assert_eq!(outcome.pc, LOAD_ADDRESS + GUEST_42.len() as u32);
        assert_eq!(outcome.instructions, 2);
        assert!(outcome.cycles > 0);
    }

    #[test]
    fn explicit_mode_is_visible_before_the_first_instruction() {
        let mut session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: LOAD_ADDRESS,
                    adl: true,
                    madl: true,
                    mbase: 0x7a,
                }),
                images: vec![LoadImage {
                    address: LOAD_ADDRESS,
                    bytes: vec![0x00],
                }],
            })
            .unwrap();
        assert_eq!(session.pc(), LOAD_ADDRESS);
        assert!(session.registers().adl);
        assert!(session.registers().madl);
        assert_eq!(session.registers().mbase, 0x7a);
        assert_eq!(session.instructions_executed(), 0);
        assert_eq!(session.total_cycles_elapsed(), 0);

        let step = session.step().unwrap();
        assert_eq!(step.reason, None);
        assert_eq!(step.instructions, 1);
        assert!(step.cycles > 0);
    }

    #[test]
    fn snapshot_and_mapped_reads_are_cycle_neutral() {
        let session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: LOAD_ADDRESS,
                    adl: true,
                    madl: true,
                    mbase: 0x7a,
                }),
                images: vec![
                    LoadImage {
                        address: LOAD_ADDRESS,
                        bytes: GUEST_42.to_vec(),
                    },
                    LoadImage {
                        address: 0xffdfff,
                        bytes: vec![0xaa, 0xbb],
                    },
                ],
            })
            .unwrap();

        let snapshot = session.snapshot();
        assert_eq!(snapshot.registers.pc, LOAD_ADDRESS);
        assert_eq!(snapshot.pc, LOAD_ADDRESS);
        assert!(snapshot.registers.adl);
        assert!(snapshot.registers.madl);
        assert_eq!(snapshot.registers.mbase, 0x7a);
        assert_eq!(snapshot.instructions, 0);
        assert_eq!(snapshot.cycles, 0);
        assert_eq!(snapshot.guest_status, None);
        assert_eq!(
            session
                .read_memory(LOAD_ADDRESS, NonZeroU32::new(4).unwrap())
                .unwrap()
                .bytes,
            GUEST_42
        );
        assert_eq!(
            session
                .read_memory(0xffdfff, NonZeroU32::new(2).unwrap())
                .unwrap()
                .bytes,
            [0xaa, 0xbb]
        );
        assert_eq!(
            session
                .read_memory(0, NonZeroU32::new(1).unwrap())
                .unwrap()
                .bytes,
            [0]
        );
        assert_eq!(session.machine.cycle_counter.get(), 0);
        assert_eq!(session.machine.total_cycles_elapsed, 0);
        assert_eq!(session.machine.mem_out_of_bounds.get(), None);
    }

    #[test]
    fn observation_errors_identify_invalid_ranges_without_mutation() {
        let mut session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: LOAD_ADDRESS,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![LoadImage {
                    address: LOAD_ADDRESS,
                    bytes: GUEST_42.to_vec(),
                }],
            })
            .unwrap();
        session.machine.cs0_lbr = 0;
        session.machine.cs0_ubr = 4;
        session.machine.cycle_counter.set(7);
        session.machine.total_cycles_elapsed = 11;
        session.machine.mem_out_of_bounds.set(Some(0x123456));
        session.machine.io_unhandled.set(Some(0x3456));
        let registers_before = format!("{:?}", session.cpu.state.reg);
        let instructions_before = session.cpu.state.instructions_executed;
        let halted_before = session.cpu.is_halted();

        assert!(matches!(
            session.read_memory(0x01000000, NonZeroU32::new(1).unwrap()),
            Err(HeadlessObservationError::AddressOutOfRange {
                address: 0x01000000
            })
        ));
        assert!(matches!(
            session.read_memory(0xffffff, NonZeroU32::new(2).unwrap()),
            Err(HeadlessObservationError::RangeOutOfRange {
                start: 0xffffff,
                len: 2
            })
        ));
        assert!(matches!(
            session.read_memory(0x050000, NonZeroU32::new(1).unwrap()),
            Err(HeadlessObservationError::UnmappedAddress { address: 0x050000 })
        ));
        let _ = session.snapshot();
        let _ = session
            .read_active_stack(NonZeroU32::new(16).unwrap())
            .unwrap();
        let instruction = session.disassemble_pc().unwrap();
        assert_eq!(instruction.address, LOAD_ADDRESS);
        assert!(instruction.adl);
        assert_eq!(instruction.bytes, [0x3e, 0x2a]);
        assert!(!instruction.text.is_empty());

        assert_eq!(format!("{:?}", session.cpu.state.reg), registers_before);
        assert_eq!(session.cpu.state.instructions_executed, instructions_before);
        assert_eq!(session.cpu.is_halted(), halted_before);
        assert_eq!(session.machine.cycle_counter.get(), 7);
        assert_eq!(session.machine.total_cycles_elapsed, 11);
        assert_eq!(session.machine.mem_out_of_bounds.get(), Some(0x123456));
        assert_eq!(session.machine.io_unhandled.get(), Some(0x3456));
    }

    #[test]
    fn active_stack_uses_distinct_spl_and_mbase_sps_with_mode_wrap() {
        let mut session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: LOAD_ADDRESS,
                    adl: true,
                    madl: false,
                    mbase: 4,
                }),
                images: vec![
                    LoadImage {
                        address: LOAD_ADDRESS,
                        bytes: vec![0x22],
                    },
                    LoadImage {
                        address: 0x04ffff,
                        bytes: vec![0x11],
                    },
                    LoadImage {
                        address: 0xffffff,
                        bytes: vec![0x33],
                    },
                ],
            })
            .unwrap();
        session.cpu.state.reg.set24(Reg16::SP, 0xffffff);
        session.cpu.state.reg.set16(Reg16::SP, 0xffff);

        let stack = session
            .read_active_stack(NonZeroU32::new(2).unwrap())
            .unwrap();
        assert_eq!(stack.kind, ActiveStackKind::Spl24);
        assert_eq!(stack.start, 0xffffff);
        assert_eq!(stack.bytes, [0x33, 0]);

        session.cpu.state.reg.adl = false;
        let stack = session
            .read_active_stack(NonZeroU32::new(2).unwrap())
            .unwrap();
        assert_eq!(stack.kind, ActiveStackKind::Sps16 { mbase: 4 });
        assert_eq!(stack.start, 0x04ffff);
        assert_eq!(stack.bytes, [0x11, 0x22]);
        assert!(matches!(
            session.read_active_stack(NonZeroU32::new(0x10001).unwrap()),
            Err(HeadlessObservationError::StackWindowTooLong {
                len: 0x10001,
                adl: false
            })
        ));
    }

    #[test]
    fn snapshot_exposes_primary_registers_flags_and_effective_non_adl_pc() {
        use ez80::{Flag, Reg8};

        let mut session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0x04ffff,
                    adl: false,
                    madl: true,
                    mbase: 4,
                }),
                images: vec![LoadImage {
                    address: 0x04ffff,
                    bytes: vec![0x00],
                }],
            })
            .unwrap();
        session.cpu.state.reg.set_a(0x80);
        session.cpu.state.reg.set24(Reg16::BC, 0x123456);
        session.cpu.state.reg.set16(Reg16::SP, 0xabcd);
        session.cpu.state.reg.put_flag(Flag::S, true);
        session.cpu.state.reg.put_flag(Flag::Z, false);

        let snapshot = session.snapshot();
        assert_eq!(snapshot.pc, 0x04ffff);
        assert_eq!(snapshot.registers.get8(Reg8::A), 0x80);
        assert_eq!(snapshot.registers.get24(Reg16::BC), 0x123456);
        assert_eq!(snapshot.registers.get16(Reg16::SP), 0xabcd);
        assert!(snapshot.registers.get_flag(Flag::S));
        assert!(!snapshot.registers.get_flag(Flag::Z));
        assert!(!snapshot.registers.adl);
        assert!(snapshot.registers.madl);
    }

    #[test]
    fn observation_handles_pc_wrap_and_reports_partial_unmapped_ranges() {
        let adl = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0xffffff,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![LoadImage {
                    address: 0xffffff,
                    bytes: vec![0x3e],
                }],
            })
            .unwrap();
        assert_eq!(adl.disassemble_pc().unwrap().bytes, [0x3e, 0]);

        let non_adl = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0x04ffff,
                    adl: false,
                    madl: false,
                    mbase: 4,
                }),
                images: vec![
                    LoadImage {
                        address: 0x04ffff,
                        bytes: vec![0x3e],
                    },
                    LoadImage {
                        address: 0x040000,
                        bytes: vec![0x2a],
                    },
                ],
            })
            .unwrap();
        assert_eq!(non_adl.disassemble_pc().unwrap().bytes, [0x3e, 0x2a]);

        let mut partial = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0x04ffff,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![LoadImage {
                    address: 0x04ffff,
                    bytes: vec![0x3e],
                }],
            })
            .unwrap();
        partial.machine.cs0_ubr = 4;
        assert_eq!(
            partial
                .read_memory(0x04ffff, NonZeroU32::new(2).unwrap())
                .unwrap_err(),
            HeadlessObservationError::UnmappedAddress { address: 0x050000 }
        );
        assert_eq!(
            partial.disassemble_pc().unwrap_err(),
            HeadlessObservationError::UnmappedAddress { address: 0x050000 }
        );
        assert_eq!(partial.machine.cycle_counter.get(), 0);
        assert_eq!(partial.machine.mem_out_of_bounds.get(), None);
    }

    #[test]
    fn disassembly_ignores_unmapped_proxy_lookahead_after_the_instruction() {
        let mut session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0x04fffe,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![LoadImage {
                    address: 0x04fffe,
                    bytes: vec![0x3e, 0x2a],
                }],
            })
            .unwrap();
        session.machine.cs0_ubr = 4;
        assert_eq!(session.disassemble_pc().unwrap().bytes, [0x3e, 0x2a]);
        assert_eq!(session.machine.cycle_counter.get(), 0);
        assert_eq!(session.machine.mem_out_of_bounds.get(), None);
    }

    #[test]
    fn disassembly_bounds_prefix_cycles_and_restores_wrapped_length() {
        fn session_with_bank(bytes: Vec<u8>) -> HeadlessSession {
            machine(false)
                .begin_headless(HeadlessSessionConfig {
                    boot: HeadlessBoot::Direct(DirectEntry {
                        pc: LOAD_ADDRESS,
                        adl: false,
                        madl: false,
                        mbase: 4,
                    }),
                    images: vec![LoadImage {
                        address: LOAD_ADDRESS,
                        bytes,
                    }],
                })
                .unwrap()
        }

        for prefix in [0x40, 0xdd] {
            let session = session_with_bank(vec![prefix; 0x1_0000]);
            assert_eq!(
                session.disassemble_pc().unwrap_err(),
                HeadlessObservationError::InstructionPrefixCycle {
                    address: LOAD_ADDRESS,
                    adl: false,
                }
            );
            assert_eq!(session.machine.cycle_counter.get(), 0);
            assert_eq!(session.machine.total_cycles_elapsed, 0);
            assert_eq!(session.machine.mem_out_of_bounds.get(), None);
            assert_eq!(session.cpu.state.instructions_executed, 0);
            assert_eq!(session.pc(), LOAD_ADDRESS);
        }

        let mut exact_span = vec![0xdd; 0x1_0000];
        *exact_span.last_mut().unwrap() = 0x00;
        let exact = session_with_bank(exact_span);
        let instruction = exact.disassemble_pc().unwrap();
        assert_eq!(instruction.bytes.len(), 0x1_0000);
        assert_eq!(instruction.bytes.first(), Some(&0xdd));
        assert_eq!(instruction.bytes.last(), Some(&0x00));

        let mut mixed = vec![0xdd; 0x1_0000];
        mixed[0] = 0x40;
        let mixed = session_with_bank(mixed);
        let instruction = mixed.disassemble_pc().unwrap();
        assert_eq!(instruction.bytes.len(), 0x1_0001);
        assert_eq!(instruction.bytes.first(), Some(&0x40));
        assert_eq!(instruction.bytes.last(), Some(&0x40));
        assert_eq!(mixed.machine.cycle_counter.get(), 0);
        assert_eq!(mixed.machine.total_cycles_elapsed, 0);
        assert_eq!(mixed.cpu.state.instructions_executed, 0);
        assert_eq!(mixed.pc(), LOAD_ADDRESS);
    }

    #[test]
    fn disassembly_fetches_immediates_in_pc_mode_across_size_overrides() {
        let adl_long = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0xfffffc,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![LoadImage {
                    address: 0xfffffc,
                    bytes: vec![0x5b, 0xcd, 0x11, 0x22],
                }],
            })
            .unwrap();
        let instruction = adl_long.disassemble_pc().unwrap();
        assert_eq!(instruction.bytes, [0x5b, 0xcd, 0x11, 0x22, 0x00]);
        assert!(instruction.text.contains("$2211"));

        let non_adl_long = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0x04fffd,
                    adl: false,
                    madl: false,
                    mbase: 4,
                }),
                images: vec![
                    LoadImage {
                        address: 0x04fffd,
                        bytes: vec![0x5b, 0x21, 0x11],
                    },
                    LoadImage {
                        address: 0x040000,
                        bytes: vec![0x22, 0x33],
                    },
                ],
            })
            .unwrap();
        let instruction = non_adl_long.disassemble_pc().unwrap();
        assert_eq!(instruction.bytes, [0x5b, 0x21, 0x11, 0x22, 0x33]);
        assert!(instruction.text.contains("$332211"));

        let adl_short = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: 0x04fffd,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![
                    LoadImage {
                        address: 0x04fffd,
                        bytes: vec![0x40, 0x21, 0x11],
                    },
                    LoadImage {
                        address: 0x050000,
                        bytes: vec![0x22],
                    },
                ],
            })
            .unwrap();
        let instruction = adl_short.disassemble_pc().unwrap();
        assert_eq!(instruction.bytes, [0x40, 0x21, 0x11, 0x22]);
        assert!(instruction.text.contains("$2211"));

        for session in [&adl_long, &non_adl_long, &adl_short] {
            assert_eq!(session.machine.cycle_counter.get(), 0);
            assert_eq!(session.machine.total_cycles_elapsed, 0);
            assert_eq!(session.machine.mem_out_of_bounds.get(), None);
            assert_eq!(session.cpu.state.instructions_executed, 0);
        }
    }

    #[test]
    fn repeated_observation_does_not_change_later_material_state() {
        fn run_with_observations(count: usize) -> (HeadlessRunOutcome, String, Vec<u8>) {
            let mut session = machine(false)
                .begin_headless(HeadlessSessionConfig {
                    boot: HeadlessBoot::Direct(DirectEntry {
                        pc: LOAD_ADDRESS,
                        adl: true,
                        madl: false,
                        mbase: 0,
                    }),
                    images: vec![LoadImage {
                        address: LOAD_ADDRESS,
                        bytes: GUEST_42.to_vec(),
                    }],
                })
                .unwrap();
            for _ in 0..count {
                let _ = session.snapshot();
                let _ = session
                    .read_memory(LOAD_ADDRESS, NonZeroU32::new(4).unwrap())
                    .unwrap();
                let _ = session
                    .read_active_stack(NonZeroU32::new(4).unwrap())
                    .unwrap();
                let _ = session.disassemble_pc().unwrap();
            }
            let outcome = session
                .run(RunLimits {
                    max_instructions: NonZeroU64::new(2).unwrap(),
                    max_cycles: NonZeroU64::new(100).unwrap(),
                })
                .unwrap();
            let registers = format!("{:?}", session.snapshot().registers);
            let memory = session
                .read_memory(LOAD_ADDRESS, NonZeroU32::new(4).unwrap())
                .unwrap()
                .bytes;
            (outcome, registers, memory)
        }

        let control = run_with_observations(0);
        assert_eq!(run_with_observations(1), control);
        assert_eq!(run_with_observations(8), control);
    }

    #[test]
    fn matching_non_adl_entry_uses_declared_mbase() {
        let outcome = machine(false)
            .run_headless(HeadlessRunConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: LOAD_ADDRESS,
                    adl: false,
                    madl: false,
                    mbase: 0x04,
                }),
                images: vec![LoadImage {
                    address: LOAD_ADDRESS,
                    bytes: GUEST_42.to_vec(),
                }],
                limits: direct(GUEST_42, 2, 100).limits,
            })
            .unwrap();
        assert_eq!(outcome.reason, StopReason::GuestExit(0x2a));
        assert_eq!(outcome.pc, LOAD_ADDRESS + GUEST_42.len() as u32);
    }

    #[test]
    fn requested_shutdown_stops_before_execution() {
        let outcome = machine(true).run_headless(direct(&[0x00], 1, 100)).unwrap();
        assert_eq!(outcome.reason, StopReason::RequestedShutdown);
        assert_eq!(outcome.instructions, 0);
        assert_eq!(outcome.cycles, 0);
    }

    #[test]
    fn halt_is_a_typed_terminal_outcome() {
        let outcome = machine(false)
            .run_headless(direct(&[0x76], 4, 100))
            .unwrap();
        assert_eq!(outcome.reason, StopReason::Halted);
        assert_eq!(outcome.instructions, 1);
    }

    #[test]
    fn instruction_limit_is_inclusive() {
        let outcome = machine(false)
            .run_headless(direct(&[0x18, 0xfe], 3, u64::MAX))
            .unwrap();
        assert_eq!(outcome.reason, StopReason::InstructionLimit);
        assert_eq!(outcome.instructions, 3);
    }

    #[test]
    fn terminal_event_wins_at_exact_cycle_limit() {
        let reference = machine(false)
            .run_headless(direct(GUEST_42, 2, 100))
            .unwrap();
        let outcome = machine(false)
            .run_headless(direct(GUEST_42, 2, reference.cycles))
            .unwrap();
        assert_eq!(outcome.reason, StopReason::GuestExit(0x2a));
        assert_eq!(outcome.cycles, reference.cycles);
    }

    #[test]
    fn cycle_overshoot_wins_over_guest_exit() {
        let outcome = machine(false)
            .run_headless(direct(&[0xd3, 0x00], 1, 1))
            .unwrap();
        assert_eq!(outcome.reason, StopReason::CycleLimit);
        assert_eq!(outcome.guest_status, Some(0xff));
        assert_eq!(outcome.instructions, 1);
    }

    #[test]
    fn invalid_loads_and_modes_fail_closed() {
        let no_images = machine(false).run_headless(HeadlessRunConfig {
            boot: HeadlessBoot::Direct(DirectEntry {
                pc: LOAD_ADDRESS,
                adl: true,
                madl: false,
                mbase: 0,
            }),
            images: vec![],
            limits: direct(&[0], 1, 1).limits,
        });
        assert_eq!(no_images.unwrap_err(), HeadlessRunError::NoImages);

        let rom = machine(false).run_headless(HeadlessRunConfig {
            boot: HeadlessBoot::Direct(DirectEntry {
                pc: 0,
                adl: true,
                madl: false,
                mbase: 0,
            }),
            images: vec![LoadImage {
                address: 0,
                bytes: vec![0],
            }],
            limits: direct(&[0], 1, 1).limits,
        });
        assert!(matches!(rom, Err(HeadlessRunError::ReadOnlyAddress { .. })));

        let wrapping = machine(false).run_headless(HeadlessRunConfig {
            boot: HeadlessBoot::Direct(DirectEntry {
                pc: 0xffffff,
                adl: true,
                madl: false,
                mbase: 0,
            }),
            images: vec![LoadImage {
                address: 0xffffff,
                bytes: vec![0, 0],
            }],
            limits: direct(&[0], 1, 1).limits,
        });
        assert!(matches!(
            wrapping,
            Err(HeadlessRunError::RangeOutOfRange { .. })
        ));

        let mismatch = machine(false).run_headless(HeadlessRunConfig {
            boot: HeadlessBoot::Direct(DirectEntry {
                pc: LOAD_ADDRESS,
                adl: false,
                madl: false,
                mbase: 0x05,
            }),
            images: vec![LoadImage {
                address: LOAD_ADDRESS,
                bytes: vec![0],
            }],
            limits: direct(&[0], 1, 1).limits,
        });
        assert!(matches!(
            mismatch,
            Err(HeadlessRunError::EntryMbaseMismatch { .. })
        ));
    }

    #[test]
    fn physical_wrap_and_cross_image_aliases_are_rejected() {
        let crossing_wrap = LoadImage {
            address: LOAD_ADDRESS,
            bytes: vec![0; 0x40001],
        };
        assert!(matches!(
            machine(false).validate_images(&[crossing_wrap]),
            Err(HeadlessRunError::PhysicalWrap { .. })
        ));

        let aliases = [
            LoadImage {
                address: LOAD_ADDRESS,
                bytes: vec![0xaa],
            },
            LoadImage {
                address: 0x0c0000,
                bytes: vec![0xbb],
            },
        ];
        assert!(matches!(
            machine(false).validate_images(&aliases),
            Err(HeadlessRunError::AliasedAddress { .. })
        ));

        let crosses_backings = [LoadImage {
            address: 0xffdfff,
            bytes: vec![0xaa, 0xbb],
        }];
        assert!(machine(false).validate_images(&crosses_backings).is_ok());
    }

    #[test]
    fn memory_fault_is_sticky_across_later_step_and_run_calls() {
        let mut session = machine(false)
            .begin_headless(HeadlessSessionConfig {
                boot: HeadlessBoot::Direct(DirectEntry {
                    pc: LOAD_ADDRESS,
                    adl: true,
                    madl: false,
                    mbase: 0,
                }),
                images: vec![LoadImage {
                    address: LOAD_ADDRESS,
                    bytes: vec![0x00],
                }],
            })
            .unwrap();
        session.machine.cs0_lbr = 5;
        session.machine.cs0_ubr = 5;

        let first = session.step().unwrap();
        assert!(matches!(first.reason, Some(StopReason::MemoryFault { .. })));
        let instructions = first.instructions;
        let second = session.step().unwrap();
        assert_eq!(second.reason, first.reason);
        assert_eq!(second.instructions, instructions);
        let run = session
            .run(RunLimits {
                max_instructions: NonZeroU64::new(10).unwrap(),
                max_cycles: NonZeroU64::new(100).unwrap(),
            })
            .unwrap();
        assert_eq!(Some(run.reason), first.reason);
        assert_eq!(run.instructions, instructions);
    }

    #[test]
    fn validation_failure_does_not_mutate_ram_or_accounting() {
        let mut machine = machine(false);
        machine.mem_external[LOAD_ADDRESS as usize] = 0x5a;
        let images = [
            LoadImage {
                address: LOAD_ADDRESS,
                bytes: vec![0xaa],
            },
            LoadImage {
                address: 0,
                bytes: vec![0xbb],
            },
        ];
        assert!(machine.validate_images(&images).is_err());
        assert_eq!(machine.mem_external[LOAD_ADDRESS as usize], 0x5a);
        assert_eq!(machine.cycle_counter.get(), 0);
        assert_eq!(machine.total_cycles_elapsed, 0);
        assert_eq!(machine.mem_out_of_bounds.get(), None);
    }
}
