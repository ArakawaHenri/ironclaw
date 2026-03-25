# Engine v2 Orchestrator (default, v0)
#
# This is the self-modifiable execution loop. It replaces the Rust
# ExecutionLoop::run() with Python that can be patched at runtime
# by the self-improvement Mission.
#
# Host functions (provided by Rust via Monty suspension):
#   __llm_complete__(messages, actions, config)  -> response dict
#   __execute_code_step__(code, state)           -> result dict
#   __execute_action__(name, params)             -> result dict
#   __check_signals__()                          -> None | "stop" | {"inject": msg}
#   __emit_event__(kind, **data)                 -> None
#   __add_message__(role, content)               -> None
#   __save_checkpoint__(state, counters)         -> None
#   __transition_to__(state, reason)             -> None
#   __retrieve_docs__(goal, max_docs)            -> list of doc dicts
#   __check_budget__()                           -> budget dict
#   __get_actions__()                            -> list of action dicts
#
# Context variables (injected by Rust before execution):
#   context  - list of prior messages [{role, content}]
#   goal     - thread goal string
#   actions  - list of available action defs
#   state    - persisted state dict from prior steps
#   config   - thread config dict


def run_loop(context, goal, actions, state, config):
    """Main execution loop. Returns an outcome dict."""
    max_iterations = config.get("max_iterations", 30)
    max_nudges = config.get("max_tool_intent_nudges", 2)
    nudge_enabled = config.get("enable_tool_intent_nudge", True)
    max_consecutive_errors = config.get("max_consecutive_errors", 5)
    nudge_count = 0
    consecutive_errors = 0
    step_count = config.get("step_count", 0)

    for step in range(step_count, max_iterations):
        # 1. Check signals
        signal = __check_signals__()
        if signal == "stop":
            __transition_to__("completed", "stopped by signal")
            return {"outcome": "stopped"}
        if signal and isinstance(signal, dict) and "inject" in signal:
            __add_message__("user", signal["inject"])

        # 2. Check budget
        budget = __check_budget__()
        if budget.get("tokens_remaining", 1) <= 0:
            __transition_to__("completed", "token budget exhausted")
            return {"outcome": "completed", "response": "Token budget exhausted."}
        if budget.get("time_remaining_ms", 1) <= 0:
            __transition_to__("completed", "time budget exhausted")
            return {"outcome": "completed", "response": "Time budget exhausted."}
        if budget.get("usd_remaining") is not None and budget["usd_remaining"] <= 0:
            __transition_to__("completed", "cost budget exhausted")
            return {"outcome": "completed", "response": "Cost budget exhausted."}

        # 3. Inject prior knowledge on first step
        if step == 0:
            docs = __retrieve_docs__(goal, 5)
            if docs:
                knowledge = format_docs(docs)
                __add_message__("system_append", knowledge)

        # 4. Call LLM
        __emit_event__("step_started", step=step)
        response = __llm_complete__(None, actions, None)
        __emit_event__("step_completed", step=step,
                       input_tokens=response.get("usage", {}).get("input_tokens", 0),
                       output_tokens=response.get("usage", {}).get("output_tokens", 0))

        # 5. Handle response based on type
        resp_type = response.get("type", "text")

        if resp_type == "text":
            text = response.get("content", "")
            __add_message__("assistant", text)

            # Check for FINAL()
            final = extract_final(text)
            if final is not None:
                __transition_to__("completed", "FINAL() in text")
                return {"outcome": "completed", "response": final}

            # Check for tool intent nudge
            if nudge_enabled and nudge_count < max_nudges and signals_tool_intent(text):
                nudge_count += 1
                __add_message__("user",
                    "You described what you'd do but didn't write code. "
                    "Please write a ```repl code block to execute your plan.")
                continue

            # Plain text response - done
            __transition_to__("completed", "text response")
            return {"outcome": "completed", "response": text}

        elif resp_type == "code":
            code = response.get("code", "")
            nudge_count = 0
            __add_message__("assistant", "```repl\n" + code + "\n```")

            # Execute code in nested Monty VM
            result = __execute_code_step__(code, state)

            # Update persisted state with results
            if result.get("return_value") is not None:
                state["step_" + str(step) + "_return"] = result["return_value"]
                state["last_return"] = result["return_value"]
            for r in result.get("action_results", []):
                state[r.get("action_name", "unknown")] = r.get("output")

            # Format output for next LLM context
            output = format_output(result)
            __add_message__("user", output)

            # Check for FINAL() in code output
            if result.get("final_answer") is not None:
                __transition_to__("completed", "FINAL() in code")
                return {"outcome": "completed", "response": result["final_answer"]}

            # Check for approval needed
            if result.get("need_approval") is not None:
                approval = result["need_approval"]
                __save_checkpoint__(state, {
                    "nudge_count": nudge_count,
                    "consecutive_errors": consecutive_errors,
                })
                return {
                    "outcome": "need_approval",
                    "action_name": approval.get("action_name", ""),
                    "call_id": approval.get("call_id", ""),
                    "parameters": approval.get("parameters", {}),
                }

            # Track consecutive errors
            if result.get("had_error"):
                consecutive_errors += 1
                if consecutive_errors >= max_consecutive_errors:
                    __transition_to__("failed", "too many consecutive errors")
                    return {"outcome": "failed",
                            "error": str(max_consecutive_errors) + " consecutive code errors"}
            else:
                consecutive_errors = 0

            __save_checkpoint__(state, {
                "nudge_count": nudge_count,
                "consecutive_errors": consecutive_errors,
            })

        elif resp_type == "actions":
            # Tier 0: structured tool calls
            nudge_count = 0
            calls = response.get("calls", [])
            __add_message__("assistant_actions", str(calls))

            for call in calls:
                name = call.get("name", "")
                params = call.get("params", {})
                call_id = call.get("call_id", "")

                r = __execute_action__(name, params)

                __emit_event__("action_executed" if not r.get("is_error") else "action_failed",
                               action_name=name, call_id=call_id)
                __add_message__("action_result", str(r.get("output", {})))

                if r.get("need_approval"):
                    __save_checkpoint__(state, {
                        "nudge_count": nudge_count,
                        "consecutive_errors": consecutive_errors,
                    })
                    return {
                        "outcome": "need_approval",
                        "action_name": name,
                        "call_id": call_id,
                        "parameters": params,
                    }

            __save_checkpoint__(state, {
                "nudge_count": nudge_count,
                "consecutive_errors": consecutive_errors,
            })

    # Max iterations reached
    __transition_to__("completed", "max iterations reached")
    return {"outcome": "max_iterations"}


# ── Helper functions (self-modifiable glue) ──────────────────


def extract_final(text):
    """Extract FINAL() content from text. Returns None if not found."""
    idx = text.find("FINAL(")
    if idx < 0:
        return None
    after = text[idx + 6:]
    # Handle triple-quoted strings
    for q in ['"""', "'''"]:
        if after.startswith(q):
            end = after.find(q, len(q))
            if end >= 0:
                return after[len(q):end]
    # Handle single/double quoted strings
    if after and after[0] in ('"', "'"):
        quote = after[0]
        end = after.find(quote, 1)
        if end >= 0:
            return after[1:end]
    # Handle balanced parens
    depth = 1
    for i, ch in enumerate(after):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return after[:i]
    return None


def signals_tool_intent(text):
    """Check if text describes tool usage without actually executing tools."""
    lower = text.lower()
    intent_phrases = ["i will", "i'll", "let me", "i would", "i should",
                      "i can", "i need to", "we should", "we can"]
    tool_phrases = ["search", "fetch", "call", "run", "execute",
                    "use the", "query", "look up"]
    has_intent = any(p in lower for p in intent_phrases)
    has_tool = any(p in lower for p in tool_phrases)
    return has_intent and has_tool


def format_output(result, max_chars=8000):
    """Format code execution result for the next LLM context message."""
    parts = []

    stdout = result.get("stdout", "")
    if stdout:
        parts.append("[stdout]\n" + stdout)

    for r in result.get("action_results", []):
        name = r.get("action_name", "?")
        output = str(r.get("output", ""))
        if r.get("is_error"):
            parts.append("[" + name + " ERROR] " + output)
        else:
            preview = output[:500] + "..." if len(output) > 500 else output
            parts.append("[" + name + "] " + preview)

    ret = result.get("return_value")
    if ret is not None:
        parts.append("[return] " + str(ret))

    text = "\n\n".join(parts)

    # Truncate from the front (keep the tail with most recent results)
    if len(text) > max_chars:
        text = "... (truncated) ...\n" + text[-max_chars:]

    # Add hint about state dict if there was an error
    if result.get("had_error") and state:
        keys = list(state.keys()) if isinstance(state, dict) else []
        if keys:
            text += "\n\n[HINT] Variables don't persist between code blocks. " \
                    "Use the `state` dict. Keys: " + str(keys)

    if not text:
        text = "[code executed, no output]"

    return text


def format_docs(docs):
    """Format memory docs for context injection."""
    parts = ["## Prior Knowledge (from completed threads)\n"]
    for doc in docs:
        label = doc.get("type", "NOTE").upper()
        content = doc.get("content", "")[:500]
        truncated = "..." if len(doc.get("content", "")) > 500 else ""
        parts.append("### [" + label + "] " + doc.get("title", "") +
                      "\n" + content + truncated + "\n")
    return "\n".join(parts)


# Entry point: call run_loop with injected context variables
result = run_loop(context, goal, actions, state, config)
FINAL(result)
