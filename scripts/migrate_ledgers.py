import glob
import json
import os
import sys

KEY_MAP = {
    # metrics fields
    "promptTokens": "prompt_tokens",
    "completionTokens": "completion_tokens",
    "reasoningTokens": "reasoning_tokens",
    "totalCompletionTokens": "total_completion_tokens",
    "tokensPerSec": "tokens_per_sec",
    "totalMs": "total_ms",
    "ttftMs": "ttft_ms",
    "tpotMs": "tpot_ms",
    "prefillMs": "prefill_ms",
    "prefillTps": "prefill_tps",
    "decodeTps": "decode_tps",
    "generationMs": "generation_ms",
    "hadReasoning": "had_reasoning",
    "reasoningCeilingHit": "reasoning_ceiling_hit",
    "streamIdleTier": "stream_idle_tier",
    "streamIdleTimeoutMs": "stream_idle_timeout_ms",
    # root fields
    "sessionId": "session_id",
    "taskId": "task_id",
    "turnIndex": "turn_index",
    "toolCalls": "tool_calls",
    "finishReason": "finish_reason",
    "durationMs": "duration_ms",
    "latencyMs": "latency_ms",
    "backoffMs": "backoff_ms",
    "retryNumber": "retry_number",
    "maxRetries": "max_retries",
    "maxTurns": "max_turns",
    "sessionTurns": "session_turns",
    "turnsTaken": "turns_taken",
    "reasoningEffort": "reasoning_effort",
    "promptChars": "prompt_chars",
    "substantiveChars": "substantive_chars",
    "maxBytes": "max_bytes",
    "continuationNumber": "continuation_number",
    "continuationsInjected": "continuations_injected",
    "maxContinuations": "max_continuations",
    "consecutiveNonMutatingBash": "consecutive_non_mutating_bash",
    "probeStreakActive": "probe_streak_active",
    "isError": "is_error",
    "toolName": "tool_name",
    "toolCallId": "tool_call_id",
}

def migrate_dict(d):
    changed = False
    new_d = {}
    for k, v in d.items():
        new_k = KEY_MAP.get(k, k)
        if new_k != k:
            changed = True
        if isinstance(v, dict):
            sub_val, sub_changed = migrate_dict(v)
            if sub_changed:
                changed = True
            new_d[new_k] = sub_val
        elif isinstance(v, list):
            new_list = []
            for item in v:
                if isinstance(item, dict):
                    item_val, item_changed = migrate_dict(item)
                    if item_changed:
                        changed = True
                    new_list.append(item_val)
                else:
                    new_list.append(item)
            new_d[new_k] = new_list
        else:
            new_d[new_k] = v
    return new_d, changed

def migrate_file(path):
    migrated_lines = 0
    total_lines = 0
    lines = []
    has_changes = False
    
    with open(path, "r", encoding="utf-8", errors="ignore") as fh:
        for line in fh:
            total_lines += 1
            stripped = line.strip()
            if not stripped:
                lines.append(line)
                continue
            try:
                obj = json.loads(stripped)
                migrated_obj, changed = migrate_dict(obj)
                if changed:
                    has_changes = True
                    migrated_lines += 1
                    lines.append(json.dumps(migrated_obj, separators=(",", ":")) + "\n")
                else:
                    lines.append(line)
            except Exception:
                lines.append(line)
                
    if has_changes:
        tmp_path = path + ".tmp"
        with open(tmp_path, "w", encoding="utf-8", newline="\n") as fh:
            fh.writelines(lines)
        os.replace(tmp_path, path)
        return True, migrated_lines, total_lines
    return False, 0, total_lines

def main():
    total_files_migrated = 0
    total_lines_migrated = 0
    paths = []
    
    for base in ["/mnt/c/Users/Apath/.castor/sessions", "/mnt/c/Users/Apath/.anser/sessions"]:
        if os.path.exists(base):
            paths.extend(glob.glob(f"{base}/*/events.jsonl"))
            
    print(f"Discovered {len(paths)} session files across .castor and .anser.")
    
    for p in paths:
        changed, m_lines, t_lines = migrate_file(p)
        if changed:
            total_files_migrated += 1
            total_lines_migrated += m_lines
            print(f"Migrated: {p} ({m_lines}/{t_lines} lines updated)")
            
    print(f"\nMigration complete!")
    print(f"Files migrated: {total_files_migrated}")
    print(f"Lines migrated: {total_lines_migrated}")

if __name__ == "__main__":
    main()
