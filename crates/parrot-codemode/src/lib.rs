//! The codemode sandbox crate: model-written JavaScript in QuickJS, whose only
//! capability is calling host-provided tools.

pub mod declarations;
pub mod identifier;
pub mod prelude;
pub mod runner;
pub mod sandbox;
pub mod source;
pub mod tool;
pub mod values;

pub use declarations::{
    CHARS_PER_TOKEN, CatalogGroup, DEFAULT_INLINE_BUDGET, DEFAULT_INPUT_SCHEMA_MAX_CHARS,
    DeclaredTool, ToolDeclaration, build_groups, doc_comment, ordered_groups, render_declaration,
    render_tool_sample, render_tool_section, render_tool_signature, resolve_schema_ref,
    schema_to_type, select_catalog,
};
pub use identifier::to_identifier;
pub use runner::{
    NestedCallOutcome, NestedCallRequest, NestedToolCapture, NestedToolRunner, SiblingToolRunner,
};
pub use sandbox::{
    HostCallRequest, SandboxOutcome, SandboxRequest, ScriptHost, StoreSnapshot, execute_script,
};
pub use source::{ScriptOptions, SourceError, parse_source};
pub use tool::{
    CODEMODE_PARAMETERS_JSON, CODEMODE_TOOL_NAME, CallTiming, CallTimings, CodemodeStore,
    CodemodeTool, RunnerInjection, ScopedCancellation, build_description, format_script_result,
};
pub use values::{
    CODEMODE_MEMORY_LIMIT_BYTES, MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS,
    MAX_STORE_VALUE_CHARS, MAX_VM_STACK_SIZE, OutputItem, ScriptCallRecord, ScriptCallStatus,
    ScriptCallTarget, ScriptError, ScriptErrorKind, ScriptGlobal, ScriptResult, ScriptTool,
    StoreWrites, ToolCallFailure,
};
