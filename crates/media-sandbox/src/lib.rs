//! Runs the untrusted media decoder as a WebAssembly guest in `wasmi`
//! (ADR-0003).
//!
//! The module may import only the three `persephone` functions below, with
//! their exact signatures. Any other import, including every WASI function, is
//! refused before instantiation, so the guest has no way to request network,
//! file, clock, environment or process access. Each decode runs in a fresh
//! store with a linear-memory cap, an instruction (fuel) budget, and host-side
//! validation of the output before any pixels are returned.

use std::fmt;
use wasmi::{
    Caller, Config, Engine, Extern, ExternType, Linker, Module, StoreLimits, StoreLimitsBuilder,
    TrapCode, ValType,
};

pub const IMPORT_MODULE: &str = "persephone";

/// The complete set of host imports a decoder module may declare.
pub const ALLOWED_IMPORTS: [(&str, &[ValType], &[ValType]); 3] = [
    ("input_len", &[], &[ValType::I32]),
    ("read_input", &[ValType::I32, ValType::I32], &[ValType::I32]),
    (
        "write_output",
        &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        &[ValType::I32],
    ),
];

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_input_bytes: usize,
    /// Cap on the guest's linear memory, enforced on every `memory.grow`.
    pub max_memory_bytes: usize,
    /// Instruction budget for one decode.
    pub fuel: u64,
    pub max_dimension: u32,
    pub max_pixels: u64,
}

impl Default for Limits {
    /// Starting budgets from the media plan. The heaviest 16-megapixel input
    /// benchmarked (a progressive JPEG) used 40.2e9 fuel; this allows about
    /// 1.6x that. Fuel counts instructions, so it is deterministic; the
    /// worker's wall deadline is enforced separately.
    fn default() -> Self {
        Self {
            max_input_bytes: 20 * 1024 * 1024,
            max_memory_bytes: 192 * 1024 * 1024,
            fuel: 64_000_000_000,
            max_dimension: 8192,
            max_pixels: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
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
    /// The guest finished without writing output.
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
    match module.get_export("decode") {
        Some(ExternType::Func(ty)) if ty.params().is_empty() && ty.results() == [ValType::I32] => {
            Ok(())
        }
        _ => Err(SandboxError::InvalidExports(
            "missing decode: () -> i32 export".into(),
        )),
    }
}

/// A validated decoder module, reusable across decodes.
pub struct Sandbox {
    engine: Engine,
    module: Module,
}

impl fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sandbox").finish_non_exhaustive()
    }
}

struct HostState {
    input: Vec<u8>,
    limits: Limits,
    store_limits: StoreLimits,
    output: Option<Image>,
    violation: Option<String>,
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

    pub fn decode(&self, input: &[u8], limits: &Limits) -> Result<Image, SandboxError> {
        self.decode_measured(input, limits).map(|(image, _)| image)
    }

    /// Like [`Sandbox::decode`], also returning the fuel the guest consumed.
    pub fn decode_measured(
        &self,
        input: &[u8],
        limits: &Limits,
    ) -> Result<(Image, u64), SandboxError> {
        if input.len() > limits.max_input_bytes {
            return Err(SandboxError::InputTooLarge);
        }
        let state = HostState {
            input: input.to_vec(),
            limits: *limits,
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

        let mut linker = Linker::<HostState>::new(&self.engine);
        link_host_functions(&mut linker)?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| SandboxError::Instantiation(e.to_string()))?;
        let decode = instance
            .get_typed_func::<(), i32>(&store, "decode")
            .map_err(|e| SandboxError::InvalidExports(e.to_string()))?;

        let status = decode.call(&mut store, ()).map_err(|error| {
            if error.as_trap_code() == Some(TrapCode::OutOfFuel) {
                SandboxError::FuelExhausted
            } else {
                SandboxError::Trap(error.to_string())
            }
        })?;
        let fuel_used = limits.fuel - store.get_fuel().unwrap_or(0);
        let state = store.into_data();
        if let Some(violation) = state.violation {
            return Err(SandboxError::InvalidOutput(violation));
        }
        if status != 0 {
            return Err(SandboxError::GuestRejected(status));
        }
        state
            .output
            .map(|image| (image, fuel_used))
            .ok_or(SandboxError::NoOutput)
    }
}

fn guest_memory(caller: &Caller<'_, HostState>) -> Option<wasmi::Memory> {
    caller.get_export("memory").and_then(Extern::into_memory)
}

fn link_host_functions(linker: &mut Linker<HostState>) -> Result<(), SandboxError> {
    let link_error = |e: wasmi::errors::LinkerError| SandboxError::Instantiation(e.to_string());
    linker
        .func_wrap(
            IMPORT_MODULE,
            "input_len",
            |caller: Caller<'_, HostState>| caller.data().input.len() as u32,
        )
        .map_err(link_error)?;
    linker
        .func_wrap(
            IMPORT_MODULE,
            "read_input",
            |mut caller: Caller<'_, HostState>, destination: u32, length: u32| -> i32 {
                let Some(memory) = guest_memory(&caller) else {
                    return 1;
                };
                if length as usize != caller.data().input.len() {
                    return 2;
                }
                let input = std::mem::take(&mut caller.data_mut().input);
                // Bounds-checked copy into guest memory; a bad pointer fails.
                let result = memory.write(&mut caller, destination as usize, &input);
                caller.data_mut().input = input;
                if result.is_ok() { 0 } else { 3 }
            },
        )
        .map_err(link_error)?;
    linker
        .func_wrap(
            IMPORT_MODULE,
            "write_output",
            |mut caller: Caller<'_, HostState>,
             width: u32,
             height: u32,
             source: u32,
             length: u32|
             -> i32 {
                match read_output(&caller, width, height, source, length) {
                    Ok(image) => {
                        caller.data_mut().output = Some(image);
                        0
                    }
                    Err(violation) => {
                        // Recorded so the decode fails even if the guest
                        // ignores this return code.
                        caller.data_mut().violation = Some(violation);
                        1
                    }
                }
            },
        )
        .map_err(link_error)?;
    Ok(())
}

fn read_output(
    caller: &Caller<'_, HostState>,
    width: u32,
    height: u32,
    source: u32,
    length: u32,
) -> Result<Image, String> {
    let state = caller.data();
    if state.output.is_some() {
        return Err("output written more than once".into());
    }
    let limits = &state.limits;
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
    let memory = guest_memory(caller).ok_or("guest memory missing")?;
    let mut rgba = vec![0; length as usize];
    memory
        .read(caller, source as usize, &mut rgba)
        .map_err(|_| "output range outside guest memory".to_string())?;
    Ok(Image {
        width,
        height,
        rgba,
    })
}
