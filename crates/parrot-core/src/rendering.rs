use crate::{CancellationToken, ConfigKey, Error, States};
use serde_json::Value;

pub(crate) async fn render_template(
    states: &dyn States,
    key: ConfigKey,
    arguments: &[(String, Value)],
) -> Result<String, Error> {
    let template = states.get_config(key).await?.into_prompt_template()?;
    template.render(arguments, CancellationToken::uncancelled())
}
