"""S7(f) in-process probe, run by Hermes 0.19.0's OWN venv interpreter with the
§4.4 child env (HOME = the child home, HERMES_HOME = the tollgate home).

It imports Hermes' real modules and asks them, without patching the code under
test, where they would look for Claude Code credentials, then drives the real
auxiliary 402 fallback (`call_llm`, `agent/auxiliary_client.py:6909`) against
the stub. It prints one JSON object of booleans, paths and provider labels —
never a token value.

Argv: <mode> where mode is
  redirect   the §4.4 env (the claim under test)
  forcegate  as redirect, but `is_provider_explicitly_configured` answers True
             for every provider, so the auto chain's anthropic gate
             (`agent/auxiliary_client.py:1941-1951`) is open: the worst case
             in which only the HOME redirect stands between Hermes and
             ~/.claude/.credentials.json
  control    run with HOME = the OUTER fake home (no redirect): the resolver
             must find the sentinel, proving the probe can see it
"""
import json
import logging
import os
import sys
from pathlib import Path

mode = sys.argv[1]
SENTINEL_MARK = "S7F-SENTINEL"
out = {"mode": mode}

out["env_HOME"] = os.environ.get("HOME")
out["path_home"] = str(Path.home())
out["expanduser"] = os.path.expanduser("~")
out["scrubbed_env_present"] = sorted(
    k for k in os.environ
    if k.startswith(("ANTHROPIC_", "NOUS_", "XDG_")) or k in (
        "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR", "OPENROUTER_API_KEY",
        "OPENAI_API_KEY", "GH_TOKEN", "GITHUB_TOKEN"))

import hermes_constants  # noqa: E402

out["hermes_home"] = str(hermes_constants.get_hermes_home())
out["default_hermes_root"] = str(hermes_constants.get_default_hermes_root())

from agent import anthropic_adapter as aa  # noqa: E402

cred_path = Path.home() / ".claude" / ".credentials.json"
out["claude_cred_path"] = str(cred_path)
out["claude_cred_path_exists"] = cred_path.exists()
creds = aa.read_claude_code_credentials()
out["read_claude_code_credentials_found"] = creds is not None
tok = aa.resolve_anthropic_token()
out["resolve_anthropic_token_found"] = tok is not None
out["resolve_anthropic_token_is_sentinel"] = bool(tok and SENTINEL_MARK in tok)
del tok, creds

if mode != "control":
    import hermes_cli.auth as hauth  # noqa: E402

    out["anthropic_explicitly_configured"] = hauth.is_provider_explicitly_configured("anthropic")
    if mode == "forcegate":
        hauth.is_provider_explicitly_configured = lambda _p: True

    from agent import auxiliary_client as ac  # noqa: E402

    tried_anthropic = []
    real_try = ac._try_anthropic

    def spy_try_anthropic(*a, **k):
        client, model = real_try(*a, **k)
        tried_anthropic.append(client is not None)
        return client, model

    # A spy (it calls straight through), so the probe can report whether the
    # chain REACHED the anthropic step and what the step returned.
    ac._try_anthropic = spy_try_anthropic

    records = []

    class Rec(logging.Handler):
        def emit(self, r):
            msg = r.getMessage()
            if "fallback" in msg.lower() or "anthropic" in msg.lower():
                records.append(f"{r.levelname} {msg}"[:240])

    logging.getLogger().addHandler(Rec())
    logging.getLogger().setLevel(logging.DEBUG)

    def aux_call(label):
        before = len(tried_anthropic)
        try:
            resp = ac.call_llm(
                task="title_generation",
                messages=[{"role": "user", "content": f"S7F title probe ({label})"}],
                max_tokens=16,
                timeout=10,
            )
            res = "returned " + type(resp).__name__
        except Exception as e:  # the expected outcome: 402 and no usable fallback
            res = f"raised {type(e).__name__}: {str(e)[:100]}"
        out[f"call_llm_{label}"] = res
        out[f"call_llm_{label}_reached_try_anthropic"] = len(tried_anthropic) - before

    # 1. As configured: the 402 on the main (custom) route walks the auto chain,
    #    whose local/custom step is the stub again.
    aux_call("natural")
    # 2. The exhausted-OpenRouter shape: the custom step is out of the chain
    #    (marked unhealthy, as Hermes marks a 402'd provider), so the walk goes
    #    on to the api-key step, which is where native Anthropic sits.
    ac._mark_provider_unhealthy("local/custom", ttl=600)
    aux_call("custom_unhealthy")
    before = len(tried_anthropic)
    fb = ac._try_payment_fallback("openrouter", task="title_generation", reason="payment error")
    out["try_payment_fallback_openrouter_label"] = fb[2]
    out["try_payment_fallback_reached_try_anthropic"] = len(tried_anthropic) - before
    out["try_anthropic_returned_a_client"] = any(tried_anthropic)
    out["fallback_log"] = records[-12:]

print(json.dumps(out, indent=1, sort_keys=True))
