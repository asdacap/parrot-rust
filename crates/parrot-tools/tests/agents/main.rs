mod input;
mod questions;
mod retained_wait;
mod runtime_agents;
pub mod runtime_support;
mod schema;
mod status;
mod tools;

use std::path::Path;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::security::{AccessSnapshot, InvocationContext};
use parrot_tools::agents::{AgentSelection, ToolInvocation};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Security(u32);
impl AccessSnapshot for Security {
    type Error = String;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), String> {
        Ok(())
    }
    fn check_write(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
}

struct Cancel(bool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        if self.0 {
            Box::pin(async {})
        } else {
            Box::pin(std::future::pending())
        }
    }
}
static NEVER: Cancel = Cancel(false);
static CANCELLED: Cancel = Cancel(true);

fn invocation(arguments: &str) -> ToolInvocation {
    ToolInvocation {
        arguments_json: arguments.into(),
        assistant_sequence: 17,
        call_id: "call-9".into(),
    }
}
fn context(cancellation: &dyn Cancellation) -> InvocationContext<'_, Security, AgentSelection> {
    InvocationContext::new(
        Security(42),
        AgentSelection {
            requested_model: "captured/model".into(),
        },
        cancellation,
    )
}
