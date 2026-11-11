"""Pier agent wrapper driving the Parrot coding agent (parrotrust) in a sandbox.

Parrot runs one benchmark turn through its one-shot chat path:

    printf '%s' <instruction> | parrot chat --basic --new-session \
        --jsonl /logs/agent/parrot.jsonl --model <provider/model>

Stdout/stderr tee into ``/logs/agent/parrot.txt``; the JSONL event log written
by ``--jsonl`` lands at ``/logs/agent/parrot.jsonl`` and is converted into an
augmented ATIF v1.7 trajectory after the trial downloads the agent logs.
"""

import json
import shlex
from typing import Any

from pier.agents.installed.base import (
    BaseInstalledAgent,
    with_prompt_template,
)
from pier.agents.network import allowlist_from_urls, collect_url_values
from pier.environments.base import BaseEnvironment
from pier.models.agent.context import AgentContext
from pier.models.agent.install import AgentInstallSpec, InstallStep
from pier.models.agent.network import NetworkAllowlist
from pier.utils.trajectory_metrics import populate_context_from_final_metrics
from pier.utils.trajectory_utils import format_trajectory_json

from parrot_agent.trajectory import (
    convert_events_to_trajectory,
    parse_jsonl_events,
)

_BINARY_NAME = "parrot"


class ParrotAgent(BaseInstalledAgent):
    """Runs the Parrot coding agent for one benchmark task inside the sandbox."""

    SUPPORTS_ATIF: bool = True

    _OUTPUT_FILENAME = "parrot.txt"
    _JSONL_FILENAME = "parrot.jsonl"

    # Fallback domains by provider id from the model name; provider_config
    # base URLs are allowlisted on top of these, along with models.dev (the
    # provider registry fetches the model catalog from it at startup) and
    # the binary_url install source.
    _DEFAULT_PROVIDER_DOMAINS: dict[str, list[str]] = {
        "anthropic": ["api.anthropic.com"],
        "chatgpt": ["chatgpt.com", "ab.chatgpt.com"],
        "deepseek": ["api.deepseek.com"],
        "google": [".googleapis.com"],
        "groq": ["api.groq.com"],
        "mistral": ["api.mistral.ai"],
        "openai": ["api.openai.com"],
        "openrouter": ["openrouter.ai"],
        "xai": ["api.x.ai"],
    }

    _PROVIDER_ENV_KEYS: dict[str, list[str]] = {
        "amazon-bedrock": ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"],
        "anthropic": ["ANTHROPIC_API_KEY"],
        "azure": ["AZURE_RESOURCE_NAME", "AZURE_API_KEY"],
        "chatgpt": [],
        "deepseek": ["DEEPSEEK_API_KEY"],
        "github-copilot": ["GITHUB_TOKEN"],
        "google": [
            "GEMINI_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GOOGLE_API_KEY",
        ],
        "groq": ["GROQ_API_KEY"],
        "huggingface": ["HF_TOKEN"],
        "llama": ["LLAMA_API_KEY"],
        "mistral": ["MISTRAL_API_KEY"],
        "openai": ["OPENAI_API_KEY", "OPENAI_BASE_URL"],
        "opencode": ["OPENCODE_API_KEY"],
        "openrouter": ["OPENROUTER_API_KEY"],
        "xai": ["XAI_API_KEY"],
    }

    def __init__(
        self,
        *args: Any,
        binary_url: str | None = None,
        binary_sha256: str | None = None,
        provider_config: dict[str, Any] | None = None,
        mode: str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._binary_url = binary_url
        self._binary_sha256 = binary_sha256
        self._provider_config: dict[str, Any] = provider_config or {}
        self._mode = mode

    @staticmethod
    def name() -> str:
        return "parrot"

    def get_version_command(self) -> str | None:
        return f"{_BINARY_NAME} --version"

    def install_spec(self) -> AgentInstallSpec:
        target = self._binary_url or self._default_binary_url()
        download = (
            "set -euo pipefail; "
            "mkdir -p \"$HOME/.local/bin\" \"$HOME/.parrot-install\" && "
            f"curl -fsSL {shlex.quote(target)} -o \"$HOME/.parrot-install/parrot.tar.gz\" && "
        )
        if self._binary_sha256:
            download += (
                f"echo {shlex.quote(self._binary_sha256)}"
                '"  $HOME/.parrot-install/parrot.tar.gz" | sha256sum -c - && '
            )
        download += (
            "tar -xzf \"$HOME/.parrot-install/parrot.tar.gz\" "
            "-C \"$HOME/.parrot-install\" && "
            "install -m 0755 \"$HOME/.parrot-install/parrot\" "
            "\"$HOME/.local/bin/parrot\" && "
            "rm -rf \"$HOME/.parrot-install\""
        )
        return AgentInstallSpec(
            agent_name=self.name(),
            version=self._version,
            steps=[
                InstallStep(
                    user="root",
                    env={"DEBIAN_FRONTEND": "noninteractive"},
                    run="apt-get update && apt-get install -y curl ca-certificates",
                ),
                InstallStep(user="agent", run=download),
            ],
            verification_command=self.get_version_command(),
        )

    @staticmethod
    def _default_binary_url() -> str:
        return (
            "https://github.com/asdacap/parrot-rust/releases/latest/download/"
            "parrot-x86_64-unknown-linux-musl.tar.gz"
        )

    def network_allowlist(self) -> NetworkAllowlist:
        urls = collect_url_values(self._provider_config)
        if self._binary_url:
            urls.append(self._binary_url)
        if self.model_name and "/" in self.model_name:
            provider, _ = self.model_name.split("/", 1)
            default_domains = self._DEFAULT_PROVIDER_DOMAINS.get(provider, [])
        else:
            default_domains = []
        # The provider registry eagerly fetches the model catalog, so an
        # unknown model needs models.dev reachable even under filtered egress.
        default_domains.append("models.dev")
        return allowlist_from_urls(urls, default_domains=default_domains)

    def _provider_env_keys(self) -> list[str]:
        keys: list[str] = []
        if self.model_name and "/" in self.model_name:
            provider, _ = self.model_name.split("/", 1)
            keys.extend(self._PROVIDER_ENV_KEYS.get(provider, []))
        for entry in self._provider_config.values():
            if isinstance(entry, dict) and isinstance(entry.get("api_key_env"), str):
                keys.append(entry["api_key_env"])
        return keys

    def _build_register_config_command(self) -> str | None:
        """Write provider overrides and sandbox off into the Parrot user config.

        Parrot seeds its own predefined config at startup, so the user config
        only needs to override providers. The Pier sandbox is already the
        isolation boundary and task containers ship no bubblewrap, so
        Parrot's own sandbox stays off. A pre-existing config.yaml is left
        untouched; merging YAML from shell is unreliable, so a pre-baked
        config is the operator's escape hatch in that case.
        """
        document: dict[str, Any] = {
            "providers": self._provider_config,
            "sandbox": {"enabled": False},
        }
        encoded = shlex.quote(json.dumps(document))
        return (
            "set -euo pipefail; "
            "mkdir -p \"$HOME/.config/parrotrust\" && "
            "if [ -e \"$HOME/.config/parrotrust/config.yaml\" ]; then "
            "echo 'parrot: refusing to overwrite existing config.yaml' >&2; exit 1; fi && "
            f"printf '%s\\n' {encoded} > \"$HOME/.config/parrotrust/config.yaml\""
        )

    def _build_run_command(self, instruction: str) -> str:
        if not self.model_name or "/" not in self.model_name:
            raise ValueError("Parrot agent requires a model in provider/model format")
        mode_arg = ""
        if self._mode:
            mode_arg = f" --mode {shlex.quote(self._mode)}"
        return (
            "mkdir -p /logs/agent && "
            "export PATH=\"$HOME/.local/bin:$PATH\" && "
            f"parrot chat --basic --new-session "
            f"--jsonl /logs/agent/{self._JSONL_FILENAME} "
            f"--model {shlex.quote(self.model_name or '')}{mode_arg} "
            f"{shlex.quote(instruction)} "
            "2>&1 | tee /logs/agent/parrot.txt"
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name or "/" not in self.model_name:
            raise ValueError("Parrot agent requires a model in provider/model format")

        env = self.build_process_env()
        for key in self._provider_env_keys():
            if value := self._get_env(key):
                env[key] = value

        config_command = self._build_register_config_command()
        if config_command:
            await self.exec_as_agent(environment, command=config_command, env=env)

        await self.exec_as_agent(
            environment,
            command=self._build_run_command(instruction),
            env=env,
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Convert the downloaded parrot.jsonl event log into an ATIF trajectory."""
        log_path = self.logs_dir / self._JSONL_FILENAME
        if not log_path.exists():
            return

        try:
            log_text = log_path.read_text()
        except OSError as exc:
            self.logger.debug(f"Failed to read {log_path}: {exc}")
            return

        events = parse_jsonl_events(log_text)
        if not events:
            return

        try:
            trajectory = convert_events_to_trajectory(
                events,
                agent_version=self.version() or "unknown",
                model_name=self.model_name,
            )
        except Exception:
            self.logger.exception("Failed to convert parrot events to trajectory")
            return

        if trajectory is None:
            return

        trajectory_path = self.logs_dir / "trajectory.json"
        try:
            trajectory_path.write_text(format_trajectory_json(trajectory.to_json_dict()))
            self.logger.debug(f"Wrote parrot trajectory to {trajectory_path}")
        except OSError as exc:
            self.logger.debug(f"Failed to write trajectory file {trajectory_path}: {exc}")

        if trajectory.final_metrics:
            populate_context_from_final_metrics(context, trajectory.final_metrics)
