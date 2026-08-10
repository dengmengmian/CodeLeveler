#!/usr/bin/env python3
"""C5-S2-B: DeepSeek V4 Pro long-context recall probe.

Deterministic code-like corpus with seeded unique needles at 5 positions
(early/25/50/75/late). Minimal request: no tools, tiny output, exact-match
scoring. Hard budget enforced: max 16 calls / 10M provider input tokens.
"""
import json, os, re, random, sys, time, urllib.request

API = "https://api.deepseek.com/chat/completions"
_cfg = open(os.path.expanduser("~/.leveler/config.toml")).read()
KEY = re.search(r'\[providers\.deepseek\].*?^api_key\s*=\s*"([^"]+)"', _cfg, re.S | re.M).group(1)
urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))
MODEL = sys.argv[3] if len(sys.argv) > 3 else "deepseek-v4-pro"
STATE = sys.argv[1]  # json state file for budget accounting across invocations

# ~3.6 bytes/token for this generator's mix (verified against actual usage on
# the first call; corpus sizing self-corrects using the measured density).
DENSITY = 2.9

def gen_corpus(target_tokens, seed):
    rnd = random.Random(seed)
    needles = {pos: f"{rnd.getrandbits(64):016x}" for pos in ("early", "p25", "p50", "p75", "late")}
    words = ("batch record decode pipeline sink flush metric label registry "
             "buffer commit replay ledger anchor guard verify fold").split()
    def block(i):
        w = rnd.choice(words); n = rnd.randrange(1000, 9999)
        kind = i % 4
        if kind == 0:
            return (f"// {w} handler for shard {n}\n"
                    f"fn {w}_{n}(input: &[u8]) -> Result<u32, Error> {{\n"
                    f"    let checksum = crc32(input) ^ 0x{rnd.getrandbits(32):08x};\n"
                    f"    Ok(checksum % {n})\n}}\n")
        if kind == 1:
            return (f"{w}.{n}.enabled = true\n{w}.{n}.threshold = {rnd.random():.4f}\n")
        if kind == 2:
            return (f"[2026-08-10T{rnd.randrange(24):02d}:{rnd.randrange(60):02d}] "
                    f"{w} shard={n} status=ok latency={rnd.randrange(5,900)}ms\n")
        return (f"The {w} stage processes shard {n} before handing records "
                f"downstream; see the {rnd.choice(words)} notes for details.\n")
    target_bytes = int(target_tokens * DENSITY)
    parts, size, i = [], 0, 0
    while size < target_bytes:
        b = block(i); parts.append(b); size += len(b); i += 1
    # plant needles at positional fractions
    total = len(parts)
    for pos, frac in (("early", 0.02), ("p25", 0.25), ("p50", 0.50), ("p75", 0.75), ("late", 0.97)):
        idx = min(int(total * frac), total - 1)
        parts[idx] += f'\nRECALL_KEY_{pos.upper()} = "{needles[pos]}"\n\n'
    return "".join(parts), needles

def probe(target_tokens, seed):
    corpus, needles = gen_corpus(target_tokens, seed)
    question = ("\n\nAnswer with ONLY five lines, one per key, in the form "
                "NAME=value, for these keys defined somewhere above: "
                "RECALL_KEY_EARLY, RECALL_KEY_P25, RECALL_KEY_P50, "
                "RECALL_KEY_P75, RECALL_KEY_LATE.")
    body = json.dumps({"model": MODEL, "stream": False, "max_tokens": 300,
                       "thinking": {"type": "disabled"},
                       "messages": [{"role": "user", "content": corpus + question}]}).encode()
    req = urllib.request.Request(API, data=body, headers={
        "Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=3600) as r:
        d = json.load(r)
    text = d["choices"][0]["message"]["content"]
    u = d["usage"]
    score = {}
    for pos, val in needles.items():
        m = re.search(rf"RECALL_KEY_{pos.upper()}\s*=\s*\"?([0-9a-f]{{16}})\"?", text)
        score[pos] = bool(m and m.group(1) == val)
    return dict(model=MODEL, target=target_tokens, seed=seed,
                input_tokens=u["prompt_tokens"], cached=u.get("prompt_cache_hit_tokens", 0),
                output_tokens=u["completion_tokens"], latency_s=round(time.time() - t0, 1),
                recall=score, overall=sum(score.values()),
                raw_answer=text[:400])

def main():
    state = json.load(open(STATE)) if os.path.exists(STATE) else dict(calls=0, input_total=0, probes=[])
    plan = json.loads(sys.argv[2])  # [[target, seed], ...]
    for target, seed in plan:
        if state["calls"] >= 16 or state["input_total"] >= 10_000_000:
            print("BUDGET EXHAUSTED — stopping", file=sys.stderr)
            break
        try:
            r = probe(target, seed)
        except Exception as e:
            r = dict(target=target, seed=seed, error=str(e)[:300])
        state["probes"].append(r)
        state["calls"] += 1
        state["input_total"] += r.get("input_tokens", 0)
        json.dump(state, open(STATE, "w"), indent=1)
        print(json.dumps({k: r.get(k) for k in ("target", "seed", "input_tokens", "overall", "recall", "latency_s", "error")}), file=sys.stderr)
    print(f"calls={state['calls']} input_total={state['input_total']}", file=sys.stderr)

main()
