use super::AgonMachine;
use ez80::Cpu;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
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
}

impl HeadlessSession {
    pub fn step(&mut self) -> Result<HeadlessStepOutcome, HeadlessRunError> {
        if let Some(reason) = self.terminal_reason() {
            return Ok(self.step_outcome(Some(reason)));
        }

        self.machine.mem_out_of_bounds.set(None);
        let instructions_before = self.cpu.state.instructions_executed;
        let cycles_before = self.machine.total_cycles_elapsed;
        self.machine.execute_instruction(&mut self.cpu);

        let instructions_after = self.cpu.state.instructions_executed;
        if instructions_after != instructions_before.saturating_add(1) {
            return Err(HeadlessRunError::InstructionAccountingInvariant {
                before: instructions_before,
                after: instructions_after,
            });
        }

        let instruction_cycles = self.machine.apply_elapsed_cycles();
        if instruction_cycles <= 0 {
            return Err(HeadlessRunError::CycleAccountingInvariant {
                pending: instruction_cycles,
            });
        }

        if self.terminal_reason().is_none() && self.machine.mem_out_of_bounds.get().is_none() {
            self.machine.do_interrupts(&mut self.cpu);
        }
        let interrupt_cycles = self.machine.apply_elapsed_cycles();
        if interrupt_cycles < 0 {
            return Err(HeadlessRunError::CycleAccountingInvariant {
                pending: interrupt_cycles,
            });
        }

        if self.machine.total_cycles_elapsed < cycles_before {
            return Err(HeadlessRunError::CycleAccountingOverflow);
        }
        debug_assert_eq!(self.machine.cycle_counter.get(), 0);

        let reason = if let Some(address) = self.machine.mem_out_of_bounds.get() {
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
