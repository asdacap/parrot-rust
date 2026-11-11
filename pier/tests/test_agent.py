import json
import shlex
from pathlib import Path

from parrot_agent import ParrotAgent


def _agent(tmp_path: Path, **kwargs) -> ParrotAgent:
    return ParrotAgent(logs_dir=tmp_path, model_name="openai/gpt-5.5", **kwargs)


def test_name_and_atif_support(tmp_path: Path):
    agent = _agent(tmp_path)
    assert agent.name() == "parrot"
    assert agent.SUPPORTS_ATIF is True


def test_install_spec_downloads_and_verifies(tmp_path: Path):
    agent = _agent(tmp_path, binary_url="https://example.com/parrot.tar.gz", binary_sha256="abc123")
    spec = agent.install_spec()

    assert spec.agent_name == "parrot"
    assert spec.verification_command == "parrot --version"
    root_step, agent_step = spec.steps
    assert root_step.user == "root"
    assert "curl" in root_step.run
    assert "https://example.com/parrot.tar.gz" in agent_step.run
    assert "sha256sum -c" in agent_step.run
    assert "abc123" in agent_step.run
    assert '"  $HOME/.parrot-install/parrot.tar.gz"' in agent_step.run


def test_install_spec_checksums_before_extracting(tmp_path: Path):
    agent = _agent(tmp_path, binary_sha256="abc123")
    run = agent.install_spec().steps[1].run
    assert run.index("sha256sum -c -") < run.index("tar -xzf")
    assert run.index("sha256sum -c -") > run.index("curl -fsSL")
    assert run.rstrip().endswith('rm -rf "$HOME/.parrot-install"')


def test_install_spec_default_url(tmp_path: Path):
    agent = _agent(tmp_path)
    spec = agent.install_spec()
    agent_step = spec.steps[1]
    assert "github.com" in agent_step.run
    assert "sha256sum" not in agent_step.run


def test_run_command_embeds_jsonl_model_and_instruction(tmp_path: Path):
    agent = _agent(tmp_path, mode="build")
    command = agent._build_run_command("fix the bug; keep 'quoting' safe")

    assert "--basic" in command
    assert "--new-session" in command
    assert "--jsonl /logs/agent/parrot.jsonl" in command
    assert "--model openai/gpt-5.5" in command
    assert "--mode build" in command
    assert shlex.quote("fix the bug; keep 'quoting' safe") in command
    assert "tee /logs/agent/parrot.txt" in command


def test_run_command_without_mode(tmp_path: Path):
    agent = _agent(tmp_path)
    command = agent._build_run_command("hi")
    assert " --mode " not in command


def test_run_requires_provider_qualified_model(tmp_path: Path):
    agent = ParrotAgent(logs_dir=tmp_path, model_name="gpt-5.5")
    try:
        agent._build_run_command("hi")
    except ValueError:
        pass
    else:
        raise AssertionError("expected ValueError for model without provider")


def test_provider_env_keys_from_model_and_provider_config(tmp_path: Path):
    agent = _agent(
        tmp_path,
        provider_config={
            "custom-gateway": {"base_url": "https://gw.example.com/api", "api_key_env": "GW_KEY"},
        },
    )
    keys = agent._provider_env_keys()
    assert "OPENAI_API_KEY" in keys
    assert "OPENAI_BASE_URL" in keys
    assert "GW_KEY" in keys


def test_network_allowlist_from_model_and_config(tmp_path: Path):
    agent = _agent(
        tmp_path,
        provider_config={
            "custom-gateway": {"base_url": "https://gw.example.com/api", "api_key_env": "GW_KEY"},
        },
    )
    domains = agent.network_allowlist().domains
    assert "api.openai.com" in domains
    assert "gw.example.com" in domains
    assert "models.dev" in domains


def test_network_allowlist_includes_binary_url_host(tmp_path: Path):
    agent = ParrotAgent(
        logs_dir=tmp_path,
        model_name="openai/gpt-5.5",
        binary_url="http://172.17.0.1:8765/parrot.tar.gz",
    )
    domains = agent.network_allowlist().domains
    assert "172.17.0.1" in domains
    assert "models.dev" in domains


def test_register_config_command_writes_absent_config_only(tmp_path: Path):
    agent = _agent(
        tmp_path,
        provider_config={"custom-gateway": {"type": "openai-compatible", "base_url": "https://gw.example.com/api"}},
    )
    command = agent._build_register_config_command()
    assert command is not None
    # The embedded JSON survives shlex.quote; extract and decode it.
    pieces = shlex.split(command)
    encoded = [p for p in pieces if p.startswith("{\"providers") or p.startswith("{'providers")]
    assert len(encoded) == 1
    payload = json.loads(encoded[0])
    assert payload["providers"]["custom-gateway"]["base_url"] == "https://gw.example.com/api"
    assert "config.yaml" in command
    assert "refusing to overwrite" in command


def test_register_config_absent_without_provider_config(tmp_path: Path):
    agent = _agent(tmp_path)
    assert agent._build_register_config_command() is None


def test_resolves_through_pier_factory(tmp_path: Path):
    from pier.agents.factory import AgentFactory

    agent = AgentFactory.create_agent_from_import_path(
        "parrot_agent:ParrotAgent",
        logs_dir=tmp_path,
        model_name="openai/gpt-5.5",
    )
    assert isinstance(agent, ParrotAgent)
    info = agent.to_agent_info()
    assert info.name == "parrot"
    assert info.model_info is not None
    assert info.model_info.name == "gpt-5.5"
