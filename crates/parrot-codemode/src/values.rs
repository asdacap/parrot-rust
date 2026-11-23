//! Script-facing values a codemode sandbox produces and consumes.

/// Heap cap of each script's VM, mirroring pi's `CODEMODE_MEMORY_LIMIT_BYTES`.
pub const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Largest single `store()` value, in JSON characters.
pub const MAX_STORE_VALUE_CHARS: usize = 256 * 1024;
/// Largest total of all stored values, in JSON characters including keys.
pub const MAX_STORE_TOTAL_CHARS: usize = 1024 * 1024;
/// Largest output one script may produce, in characters of text.
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;
/// Largest number of output items one script may produce.
pub const MAX_OUTPUT_ITEMS: usize = 100_000;

/// Maximum QuickJS stack the interpreter may use before deep recursion throws a
/// catchable `RangeError`. Kept well under the dedicated script thread's stack.
pub const MAX_VM_STACK_SIZE: usize = 1024 * 1024;

/// One tool a script may call, as the prelude sees it.
#[derive(Clone, Debug)]
pub struct ScriptTool {
    /// The tool's raw name, which `tools["<name>"]` also accepts.
    pub name: String,
    /// The identifier-safe name the script uses as `tools.<js_name>`.
    pub js_name: String,
    pub description: String,
}

/// One host global a script may call, either bare or as a dotted namespace member.
#[derive(Clone, Debug)]
pub struct ScriptGlobal {
    /// The name the script calls; a name like `a.b` lands on a frozen `a` object.
    pub name: String,
    /// When true, the call receives all its arguments as one JSON array.
    pub spread: bool,
}

/// What a failed host call reports to the script.
#[derive(Clone, Debug)]
pub struct ToolCallFailure {
    /// The error message the script's rejected promise carries.
    pub message: String,
    /// JSON object of own properties set on the rejected `Error`, except
    /// `name`, `message`, and `stack`. `None` adds nothing.
    pub properties_json: Option<String>,
}

/// One item of the script's output, in the order the script produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputItem {
    /// An item from `text()`.
    Text(String),
    /// An item from `console.*`.
    Console(String),
}

impl OutputItem {
    /// The item's text, whichever kind produced it.
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::Console(text) => text,
        }
    }
}

/// Whether a recorded script call settled successfully, failed, or never settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptCallStatus {
    Ok,
    Error,
    Cancelled,
}

/// One call a script made to a tool or host global, with its stable id in the
/// order the script issued it.
#[derive(Clone, Debug)]
pub struct ScriptCallRecord {
    /// The prelude's call id, 1-based in issue order. Stable across runs of the
    /// same script with the same tools, which keeps the id replay-shaped.
    pub id: u32,
    /// The raw name the script called.
    pub name: String,
    /// Whether the call reached a tool or a host global.
    pub target: ScriptCallTarget,
    /// JSON text of the arguments, `None` when the script called with none.
    pub arguments_json: Option<String>,
    pub status: ScriptCallStatus,
}

/// Whether a recorded call reached a tool or a host global.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptCallTarget {
    Tool,
    Global,
}

/// Why a script ended unsuccessfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptErrorKind {
    /// The script threw, failed to parse, or hit an allocation limit.
    Script,
    /// The deadline expired or the interrupt budget ran out.
    Timeout,
    /// The host aborted the script.
    Aborted,
    /// The VM itself failed, for example a broken runtime setup.
    Sandbox,
}

/// A script's failure, shaped like pi's error report.
#[derive(Clone, Debug)]
pub struct ScriptError {
    pub kind: ScriptErrorKind,
    /// The thrown value's `name`, or `Error` for a non-Error throw.
    pub name: String,
    pub message: String,
    /// The QuickJS stack, prefixed `name: message` like a V8 error.
    pub stack: String,
}

/// The values a successful script wrote through `store()`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoreWrites {
    /// Key to the JSON text of the written value.
    pub set: Vec<(String, String)>,
    /// Keys the script deleted with `store(key, undefined)`.
    pub delete: Vec<String>,
}

/// The outcome of one script execution. `execute()` never rejects for script
/// failures; everything lands here.
#[derive(Clone, Debug)]
pub struct ScriptResult {
    pub ok: bool,
    /// Output items in production order, also present for failed executions.
    pub output: Vec<OutputItem>,
    /// JSON text of the script's return value, `None` without one.
    pub value: Option<String>,
    /// The failure when `ok` is false.
    pub error: Option<ScriptError>,
    /// Every call the script issued, in issue order.
    pub calls: Vec<ScriptCallRecord>,
}
