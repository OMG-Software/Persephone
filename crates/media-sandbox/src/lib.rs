//! Runs the untrusted media decoder as a WebAssembly guest in `wasmi`
//! (ADR-0003).
//!
//! The module may import only the four `persephone` functions below, with
//! their exact signatures. Any other import, including every WASI function, is
//! refused before instantiation, so the guest has no way to request network,
//! file, clock, environment or process access. It must export `memory` and two
//! operations, `decode` and `strip` (ADR-0009). Each operation runs in a fresh
//! store with a linear-memory cap, an instruction (fuel) budget, and host-side
//! validation of the output before anything is returned. Each output import is
//! accepted only during its own operation.

use std::fmt;
use wasmi::{
    Caller, Config, Engine, Extern, ExternType, Linker, Module, StoreLimits, StoreLimitsBuilder,
    TrapCode, ValType,
};

pub const IMPORT_MODULE: &str = "persephone";

/// The complete set of host imports a decoder module may declare.
pub const ALLOWED_IMPORTS: [(&str, &[ValType], &[ValType]); 4] = [
    ("input_len", &[], &[ValType::I32]),
    ("read_input", &[ValType::I32, ValType::I32], &[ValType::I32]),
    // Decode: width, height, pixel pointer, pixel length, EXIF orientation.
    (
        "write_output",
        &[
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
            ValType::I32,
        ],
        &[ValType::I32],
    ),
    // Strip: pointer and length of the stripped file.
    (
        "write_stripped",
        &[ValType::I32, ValType::I32],
        &[ValType::I32],
    ),
];

/// The function exports a decoder module must provide, each `() -> i32`.
pub const REQUIRED_EXPORTS: [&str; 2] = ["decode", "strip"];

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_input_bytes: usize,
    /// Cap on the guest's linear memory, enforced on every `memory.grow`.
    pub max_memory_bytes: usize,
    /// Instruction budget for one operation.
    pub fuel: u64,
    pub max_dimension: u32,
    pub max_pixels: u64,
}

impl Default for Limits {
    /// Starting budgets from the media plan, for one decode. The heaviest
    /// 16-megapixel input benchmarked (a progressive JPEG) used 40.2e9 fuel;
    /// this allows about 1.6x that. Fuel counts instructions, so it is
    /// deterministic; the worker's wall deadline is enforced separately.
    fn default() -> Self {
        Self {
            max_input_bytes: 20 * 1024 * 1024,
            // Equal to the guest's fixed linear memory (crates/media-decoder/build.rs).
            max_memory_bytes: 176 * 1024 * 1024,
            fuel: 64_000_000_000,
            max_dimension: persephone_media_protocol::MAX_DIMENSION,
            max_pixels: persephone_media_protocol::MAX_PIXELS,
        }
    }
}

impl Limits {
    /// Limits for one strip. The guest decodes the original and the stripped
    /// copy, so the fuel budget is twice the decode budget. The broker's wall
    /// deadline and the Job's CPU limit still apply.
    pub fn for_strip() -> Self {
        let decode = Self::default();
        Self {
            fuel: decode.fuel * 2,
            ..decode
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// EXIF orientation, 1 to 8, checked by the host. The pixels are not
    /// rotated.
    pub orientation: u8,
    pub rgba: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SandboxError {
    /// The module failed wasmi parsing or validation.
    InvalidModule(String),
    /// The module declares an import outside the allowlist.
    ForbiddenImport {
        module: String,
        name: String,
    },
    /// A required export is missing or has the wrong type.
    InvalidExports(String),
    InputTooLarge,
    /// Instantiation failed, for example because initial memory exceeds the cap.
    Instantiation(String),
    FuelExhausted,
    /// The guest trapped (including stack exhaustion and failed memory growth).
    Trap(String),
    /// The guest returned a nonzero status code.
    GuestRejected(i32),
    /// The guest finished without writing the output of its operation.
    NoOutput,
    /// The guest's output broke a host-enforced rule; nothing is returned.
    InvalidOutput(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SandboxError {}

/// Checks a module's imports and exports against the fixed ABI.
pub fn check_module_interface(module: &Module) -> Result<(), SandboxError> {
    for import in module.imports() {
        let allowed = import.module() == IMPORT_MODULE
            && ALLOWED_IMPORTS.iter().any(|(name, params, results)| {
                import.name() == *name
                    && matches!(import.ty(), ExternType::Func(ty)
                        if ty.params() == *params && ty.results() == *results)
            });
        if !allowed {
            return Err(SandboxError::ForbiddenImport {
                module: import.module().to_owned(),
                name: import.name().to_owned(),
            });
        }
    }
    match module.get_export("memory") {
        Some(ExternType::Memory(_)) => {}
        _ => return Err(SandboxError::InvalidExports("missing memory export".into())),
    }
    for name in REQUIRED_EXPORTS {
        match module.get_export(name) {
            Some(ExternType::Func(ty))
                if ty.params().is_empty() && ty.results() == [ValType::I32] => {}
            _ => {
                return Err(SandboxError::InvalidExports(format!(
                    "missing {name}: () -> i32 export"
                )));
            }
        }
    }
    Ok(())
}

/// A validated decoder module, reusable across operations.
pub struct Sandbox {
    engine: Engine,
    module: Module,
}

impl fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sandbox").finish_non_exhaustive()
    }
}

/// The guest export one store runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Decode,
    Strip,
}

impl Operation {
    const fn export(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Strip => "strip",
        }
    }
}

/// Per-operation host state. The input is borrowed, not copied, and the
/// output is recorded as a validated region of guest memory rather than
/// copied out.
struct HostState<'a> {
    input: &'a [u8],
    limits: Limits,
    operation: Operation,
    store_limits: StoreLimits,
    output: Option<Output>,
    violation: Option<String>,
}

#[derive(Clone, Copy)]
struct Region {
    offset: usize,
    length: usize,
}

#[derive(Clone, Copy)]
enum Output {
    Pixels {
        width: u32,
        height: u32,
        orientation: u8,
        region: Region,
    },
    Stripped(Region),
}

/// Measurements from one guest operation.
#[derive(Clone, Copy, Debug)]
pub struct DecodeStats {
    pub fuel_used: u64,
    /// Final size of the guest's linear memory, which only grows.
    pub guest_memory_bytes: usize,
}

impl Sandbox {
    pub fn new(wasm: &[u8]) -> Result<Self, SandboxError> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).map_err(|e| SandboxError::InvalidModule(e.to_string()))?;
        check_module_interface(&module)?;
        Ok(Self { engine, module })
    }

    /// Use [`Limits::default`] for a decode.
    pub fn decode(&self, input: &[u8], limits: &Limits) -> Result<Image, SandboxError> {
        self.decode_measured(input, limits).map(|(image, _)| image)
    }

    /// Like [`Sandbox::decode`], also returning decode measurements.
    pub fn decode_measured(
        &self,
        input: &[u8],
        limits: &Limits,
    ) -> Result<(Image, DecodeStats), SandboxError> {
        self.decode_with(input, limits, |width, height, orientation, rgba| Image {
            width,
            height,
            orientation,
            rgba: rgba.to_vec(),
        })
    }

    /// Decodes and passes the validated width, height, EXIF orientation and
    /// RGBA pixels to `sink`. The pixels are a borrowed slice of guest memory,
    /// so the host makes no copy of the output. `sink` runs only when the
    /// guest succeeded and its output passed validation.
    pub fn decode_with<R>(
        &self,
        input: &[u8],
        limits: &Limits,
        sink: impl FnOnce(u32, u32, u8, &[u8]) -> R,
    ) -> Result<(R, DecodeStats), SandboxError> {
        self.run(
            input,
            limits,
            Operation::Decode,
            |output, bytes| match output {
                Output::Pixels {
                    width,
                    height,
                    orientation,
                    ..
                } => Some(sink(width, height, orientation, bytes)),
                Output::Stripped(_) => None,
            },
        )
    }

    /// Strips metadata and returns the stripped file. Use
    /// [`Limits::for_strip`]: the guest decodes twice.
    pub fn strip(&self, input: &[u8], limits: &Limits) -> Result<Vec<u8>, SandboxError> {
        self.strip_with(input, limits, <[u8]>::to_vec)
            .map(|(stripped, _)| stripped)
    }

    /// Strips metadata and passes the stripped file to `sink` as a borrowed
    /// slice of guest memory. The host has checked that it is not empty and
    /// not longer than the input. `sink` runs only when the guest succeeded,
    /// which includes its check that the stripped copy decodes to the same
    /// pixels as the original.
    pub fn strip_with<R>(
        &self,
        input: &[u8],
        limits: &Limits,
        sink: impl FnOnce(&[u8]) -> R,
    ) -> Result<(R, DecodeStats), SandboxError> {
        self.run(
            input,
            limits,
            Operation::Strip,
            |output, bytes| match output {
                Output::Stripped(_) => Some(sink(bytes)),
                Output::Pixels { .. } => None,
            },
        )
    }

    /// Runs one operation in a fresh store and passes its validated output to
    /// `sink`. The imports refuse the other operation's output, so `sink`
    /// cannot normally see the wrong kind; if it returns `None`, the output is
    /// still refused.
    fn run<R>(
        &self,
        input: &[u8],
        limits: &Limits,
        operation: Operation,
        sink: impl FnOnce(Output, &[u8]) -> Option<R>,
    ) -> Result<(R, DecodeStats), SandboxError> {
        if input.len() > limits.max_input_bytes {
            return Err(SandboxError::InputTooLarge);
        }
        let state = HostState {
            input,
            limits: *limits,
            operation,
            store_limits: StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .memories(1)
                .tables(1)
                .instances(1)
                .build(),
            output: None,
            violation: None,
        };
        let mut store = wasmi::Store::new(&self.engine, state);
        store.limiter(|state| &mut state.store_limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| SandboxError::Instantiation(e.to_string()))?;

        let mut linker = Linker::<HostState<'_>>::new(&self.engine);
        link_host_functions(&mut linker)?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| SandboxError::Instantiation(e.to_string()))?;
        let entry = instance
            .get_typed_func::<(), i32>(&store, operation.export())
            .map_err(|e| SandboxError::InvalidExports(e.to_string()))?;

        let status = entry.call(&mut store, ()).map_err(|error| {
            if error.as_trap_code() == Some(TrapCode::OutOfFuel) {
                SandboxError::FuelExhausted
            } else {
                SandboxError::Trap(error.to_string())
            }
        })?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| SandboxError::InvalidExports("missing memory export".into()))?;
        let stats = DecodeStats {
            fuel_used: limits.fuel - store.get_fuel().unwrap_or(0),
            guest_memory_bytes: memory.data_size(&store),
        };
        let state = store.data();
        if let Some(violation) = &state.violation {
            return Err(SandboxError::InvalidOutput(violation.clone()));
        }
        if status != 0 {
            return Err(SandboxError::GuestRejected(status));
        }
        let output = state.output.ok_or(SandboxError::NoOutput)?;
        let (Output::Pixels { region, .. } | Output::Stripped(region)) = output;
        // The region was bounds-checked when recorded; memory only grows, so
        // it is still in range. The guest is no longer running.
        let bytes = memory
            .data(&store)
            .get(region.offset..region.offset + region.length)
            .ok_or_else(|| {
                SandboxError::InvalidOutput("output range outside guest memory".into())
            })?;
        let result = sink(output, bytes)
            .ok_or_else(|| SandboxError::InvalidOutput("output of the wrong kind".into()))?;
        Ok((result, stats))
    }
}

fn guest_memory(caller: &Caller<'_, HostState<'_>>) -> Option<wasmi::Memory> {
    caller.get_export("memory").and_then(Extern::into_memory)
}

fn link_host_functions(linker: &mut Linker<HostState<'_>>) -> Result<(), SandboxError> {
    let link_error = |e: wasmi::errors::LinkerError| SandboxError::Instantiation(e.to_string());
    linker
        .func_wrap(
            IMPORT_MODULE,
            "input_len",
            |caller: Caller<'_, HostState<'_>>| caller.data().input.len() as u32,
        )
        .map_err(link_error)?;
    linker
        .func_wrap(
            IMPORT_MODULE,
            "read_input",
            |mut caller: Caller<'_, HostState<'_>>, destination: u32, length: u32| -> i32 {
                let Some(memory) = guest_memory(&caller) else {
                    return 1;
                };
                let input = caller.data().input;
                if length as usize != input.len() {
                    return 2;
                }
                // Bounds-checked copy into guest memory; a bad pointer fails.
                match memory.write(&mut caller, destination as usize, input) {
                    Ok(()) => 0,
                    Err(_) => 3,
                }
            },
        )
        .map_err(link_error)?;
    linker
        .func_wrap(
            IMPORT_MODULE,
            "write_output",
            |mut caller: Caller<'_, HostState<'_>>,
             width: u32,
             height: u32,
             source: u32,
             length: u32,
             orientation: u32|
             -> i32 {
                let output = validate_pixels(&caller, width, height, source, length, orientation);
                record(&mut caller, output)
            },
        )
        .map_err(link_error)?;
    linker
        .func_wrap(
            IMPORT_MODULE,
            "write_stripped",
            |mut caller: Caller<'_, HostState<'_>>, source: u32, length: u32| -> i32 {
                let output = validate_stripped(&caller, source, length);
                record(&mut caller, output)
            },
        )
        .map_err(link_error)?;
    Ok(())
}

/// Records a validated output, or the violation, so the operation fails even
/// if the guest ignores the nonzero return code.
fn record(caller: &mut Caller<'_, HostState<'_>>, output: Result<Output, String>) -> i32 {
    match output {
        Ok(output) => {
            caller.data_mut().output = Some(output);
            0
        }
        Err(violation) => {
            caller.data_mut().violation = Some(violation);
            1
        }
    }
}

/// Checks the rules every output shares: the right operation, one write, and
/// a range inside guest memory.
fn validate_region(
    caller: &Caller<'_, HostState<'_>>,
    operation: Operation,
    source: u32,
    length: u32,
) -> Result<Region, String> {
    let state = caller.data();
    if state.operation != operation {
        return Err(format!(
            "{operation:?} output written during {:?}",
            state.operation
        ));
    }
    if state.output.is_some() {
        return Err("output written more than once".into());
    }
    let memory = guest_memory(caller).ok_or("guest memory missing")?;
    let (offset, length) = (source as usize, length as usize);
    if offset
        .checked_add(length)
        .is_none_or(|end| end > memory.data_size(caller))
    {
        return Err("output range outside guest memory".into());
    }
    Ok(Region { offset, length })
}

fn validate_pixels(
    caller: &Caller<'_, HostState<'_>>,
    width: u32,
    height: u32,
    source: u32,
    length: u32,
    orientation: u32,
) -> Result<Output, String> {
    let region = validate_region(caller, Operation::Decode, source, length)?;
    let limits = &caller.data().limits;
    if width == 0 || height == 0 {
        return Err("zero dimension".into());
    }
    if width > limits.max_dimension || height > limits.max_dimension {
        return Err(format!("dimension {width}x{height} exceeds limit"));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limits.max_pixels {
        return Err(format!("{pixels} pixels exceeds limit"));
    }
    if u64::from(length) != pixels * 4 {
        return Err(format!(
            "length {length} does not match {width}x{height} RGBA"
        ));
    }
    let orientation = u8::try_from(orientation)
        .ok()
        .filter(|value| (1..=8).contains(value))
        .ok_or_else(|| format!("orientation {orientation} outside 1 to 8"))?;
    Ok(Output::Pixels {
        width,
        height,
        orientation,
        region,
    })
}

/// The guest strips in place, so a valid output is never longer than the
/// input. That also keeps it within `max_input_bytes` and one encoded frame.
fn validate_stripped(
    caller: &Caller<'_, HostState<'_>>,
    source: u32,
    length: u32,
) -> Result<Output, String> {
    let region = validate_region(caller, Operation::Strip, source, length)?;
    let input_len = caller.data().input.len();
    if length == 0 {
        return Err("empty stripped output".into());
    }
    if region.length > input_len {
        return Err(format!(
            "stripped length {length} exceeds input length {input_len}"
        ));
    }
    Ok(Output::Stripped(region))
}
