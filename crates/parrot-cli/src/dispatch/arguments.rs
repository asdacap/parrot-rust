use std::io::{self, BufRead, Read};

pub const DEFAULT_MODEL: &str = "opencode-go/glm-5.2";
pub const DEFAULT_WEB_PORT: u16 = 7420;
pub const AUTH_USAGE: &str =
    "usage: parrot auth login <provider> [--api-key-stdin] [--device] [--s3-stdin]";
pub const S3_USAGE: &str = "usage: parrot auth login image_upload --s3-stdin";
pub const SERVE_USAGE: &str = "usage: parrot serve [--listen unix:/path|http://host:port|https://host:port] [--token-file <path>] [--unsafe-allow-external]";
pub const WEB_USAGE: &str = "usage: parrot web [--port <n>]";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Help,
    Version,
    Runtime(RuntimeCommand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCommand {
    Authenticate(AuthOptions),
    Models,
    Sessions,
    Chat(ChatOptions),
    Serve(ServeOptions),
    Web { port: u16 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageError {
    pub message: String,
    pub unknown_command: bool,
}
impl UsageError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unknown_command: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthOptions {
    pub provider: String,
    pub input: AuthInputKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthInputKind {
    ApiKeyStdin,
    S3Stdin,
    OAuth { device: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthInput {
    ApiKey(String),
    S3 {
        access_key: String,
        secret_key: String,
    },
    OAuth {
        device: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatOptions {
    pub model: String,
    pub model_overridden: bool,
    pub variant: Option<String>,
    pub mode: String,
    pub connect: String,
    pub token_file: String,
    pub basic: bool,
    pub take_over: bool,
    pub prompt: String,
}
impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            model_overridden: false,
            variant: None,
            mode: String::new(),
            connect: String::new(),
            token_file: String::new(),
            basic: false,
            take_over: false,
            prompt: String::new(),
        }
    }
}
impl ChatOptions {
    /// Saved settings affect fresh sessions only. Existing sessions retain the
    /// model and mode returned by LocalChatStartup.
    pub fn apply_configuration(&mut self, saved_model: &str, default_mode: &str) {
        if !self.model_overridden && !saved_model.is_empty() {
            self.model = saved_model.into();
        }
        if self.mode.is_empty() {
            self.mode = default_mode.into();
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServeOptions {
    /// None selects the ordinary application's control/parrot.sock.
    pub listen: Option<String>,
    pub token_file: String,
    pub unsafe_allow_external: bool,
}

pub fn parse(arguments: &[String]) -> Result<Command, UsageError> {
    let command = arguments.first().map_or("chat", String::as_str);
    let runtime = match command {
        "help" | "--help" | "-h" => return Ok(Command::Help),
        "version" | "--version" => return Ok(Command::Version),
        "auth" => RuntimeCommand::Authenticate(parse_auth(arguments)?),
        "models" => RuntimeCommand::Models,
        "sessions" => RuntimeCommand::Sessions,
        "chat" => RuntimeCommand::Chat(parse_chat(arguments)?),
        "serve" => RuntimeCommand::Serve(parse_serve(arguments)?),
        "web" => {
            let mut port = DEFAULT_WEB_PORT;
            let mut index = 1;
            while index < arguments.len() {
                if arguments[index] != "--port" {
                    return Err(UsageError::usage(WEB_USAGE));
                }
                let value = arguments
                    .get(index + 1)
                    .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|value| value.parse::<u16>().ok())
                    .ok_or_else(|| UsageError::usage(WEB_USAGE))?;
                port = value;
                index += 2;
            }
            RuntimeCommand::Web { port }
        }
        _ => {
            return Err(UsageError {
                message: format!("parrot: unknown command \"{command}\""),
                unknown_command: true,
            });
        }
    };
    Ok(Command::Runtime(runtime))
}

fn parse_auth(arguments: &[String]) -> Result<AuthOptions, UsageError> {
    if arguments.len() < 3 || arguments[1] != "login" {
        return Err(UsageError::usage(AUTH_USAGE));
    }
    let provider = &arguments[2];
    let has = |flag: &str| arguments.iter().any(|argument| argument == flag);
    let input = if has("--s3-stdin") {
        if provider != "image_upload" || arguments.len() != 4 {
            return Err(UsageError::usage(S3_USAGE));
        }
        AuthInputKind::S3Stdin
    } else if provider == "image_upload" {
        return Err(UsageError::usage(S3_USAGE));
    } else if has("--api-key-stdin") {
        AuthInputKind::ApiKeyStdin
    } else if provider == "chatgpt" {
        AuthInputKind::OAuth {
            device: has("--device"),
        }
    } else {
        return Err(UsageError::usage(format!(
            "parrot: {provider} needs a key: parrot auth login {provider} --api-key-stdin"
        )));
    };
    Ok(AuthOptions {
        provider: provider.clone(),
        input,
    })
}

fn parse_chat(arguments: &[String]) -> Result<ChatOptions, UsageError> {
    let mut options = ChatOptions::default();
    let mut words = Vec::new();
    let mut index = 1;
    while index < arguments.len() {
        let argument = &arguments[index];
        let next = arguments.get(index + 1);
        match (argument.as_str(), next) {
            ("--model", Some(value)) => {
                options.model = value.clone();
                options.model_overridden = true;
                index += 1;
            }
            ("--variant", Some(value)) if !value.starts_with("--") => {
                options.variant = Some(value.clone());
                index += 1;
            }
            ("--variant", _) => {
                return Err(UsageError::usage("usage: parrot chat --variant <name>"));
            }
            ("--mode", Some(value)) => {
                options.mode = value.clone();
                index += 1;
            }
            ("--connect", Some(value)) => {
                options.connect = value.clone();
                index += 1;
            }
            ("--token-file", Some(value)) => {
                options.token_file = value.clone();
                index += 1;
            }
            ("--basic", _) => options.basic = true,
            ("--take-over", _) => options.take_over = true,
            _ => words.push(argument.clone()),
        }
        index += 1;
    }
    options.prompt = words.join(" ");
    Ok(options)
}

fn parse_serve(arguments: &[String]) -> Result<ServeOptions, UsageError> {
    let mut options = ServeOptions::default();
    let mut index = 1;
    while index < arguments.len() {
        match (arguments[index].as_str(), arguments.get(index + 1)) {
            ("--listen", Some(value)) => {
                options.listen = Some(value.clone());
                index += 1;
            }
            ("--token-file", Some(value)) => {
                options.token_file = value.clone();
                index += 1;
            }
            ("--unsafe-allow-external", _) => options.unsafe_allow_external = true,
            _ => return Err(UsageError::usage(SERVE_USAGE)),
        }
        index += 1;
    }
    Ok(options)
}

/// Reads only explicitly selected stdin auth data. Persistence/OAuth is supplied
/// by the real authentication owner; this component never starts a browser.
pub fn read_auth_input(
    options: &AuthOptions,
    input: &mut dyn BufRead,
) -> io::Result<Result<AuthInput, UsageError>> {
    Ok(match options.input {
        AuthInputKind::OAuth { device } => Ok(AuthInput::OAuth { device }),
        AuthInputKind::ApiKeyStdin => {
            let mut key = String::new();
            input.read_to_string(&mut key)?;
            let key = key.trim();
            if key.is_empty() {
                Err(UsageError::usage("parrot: no key on stdin"))
            } else {
                Ok(AuthInput::ApiKey(key.into()))
            }
        }
        AuthInputKind::S3Stdin => {
            let mut text = String::new();
            input.read_to_string(&mut text)?;
            let (access, rest) = take_line(&text);
            let (secret, remainder) = take_line(rest);
            if access.trim().is_empty() || secret.trim().is_empty() || !remainder.trim().is_empty()
            {
                Err(UsageError::usage(
                    "parrot: expected an access key and secret key on two stdin lines",
                ))
            } else {
                Ok(AuthInput::S3 {
                    access_key: access.trim().into(),
                    secret_key: secret.trim().into(),
                })
            }
        }
    })
}

fn take_line(text: &str) -> (&str, &str) {
    match text.find(['\r', '\n']) {
        Some(index) => {
            let mut next = index + 1;
            if text.as_bytes()[index] == b'\r' && text.as_bytes().get(next) == Some(&b'\n') {
                next += 1;
            }
            (&text[..index], &text[next..])
        }
        None => (text, ""),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatInput {
    EmptyPipe,
    Once(String),
    Interactive,
}

pub fn read_chat_input(
    prompt: &str,
    input_redirected: bool,
    input: &mut dyn Read,
) -> io::Result<ChatInput> {
    if !prompt.is_empty() {
        return Ok(ChatInput::Once(prompt.into()));
    }
    if !input_redirected {
        return Ok(ChatInput::Interactive);
    }
    let mut text = String::new();
    input.read_to_string(&mut text)?;
    if text.trim().is_empty() {
        Ok(ChatInput::EmptyPipe)
    } else {
        Ok(ChatInput::Once(text.trim().into()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererKind {
    Basic,
    Enhanced,
}

pub fn renderer_for(basic: bool, output_redirected: bool) -> RendererKind {
    if basic || output_redirected {
        RendererKind::Basic
    } else {
        RendererKind::Enhanced
    }
}
