import os
import sys
from pathlib import Path

_EXAMPLES_ROOT = Path(__file__).resolve().parents[1]
if str(_EXAMPLES_ROOT) not in sys.path:
    sys.path.insert(0, str(_EXAMPLES_ROOT))

from _bootstrap import ensure_local_sdk_src, runtime_config

ensure_local_sdk_src()

from openai_codex import Codex

with Codex(config=runtime_config()) as codex:
    codex.login_api_key(os.environ["CODEX_API_KEY"])
    account = codex.account()

    print("account.requires_openai_auth:", account.requires_openai_auth)
