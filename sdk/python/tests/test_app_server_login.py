from __future__ import annotations

from app_server_harness import AppServerHarness

from openai_codex import Codex, CodexConfig


def _app_server_config(harness: AppServerHarness) -> CodexConfig:
    """Build an isolated login config without inheriting ambient API-key auth."""
    config = harness.app_server_config()
    config.env = {**(config.env or {}), "OPENAI_API_KEY": ""}
    return config


def test_api_key_login_authenticates_follow_up_model_requests(tmp_path) -> None:
    """API-key login should authorize the next Responses request with that key."""
    with AppServerHarness(tmp_path, requires_openai_auth=True) as harness:
        harness.responses.enqueue_assistant_message("api key auth", response_id="api-key-auth")

        with Codex(config=_app_server_config(harness)) as codex:
            codex.login_api_key("sk-sdk-login-test")
            result = codex.thread_start().run("prove api key auth")
            request = harness.responses.single_request()

    assert {
        "final_response": result.final_response,
        "authorization": request.header("authorization"),
    } == {
        "final_response": "api key auth",
        "authorization": "Bearer sk-sdk-login-test",
    }
