#!/usr/bin/env python3
"""C5-S2-A: estimator calibration against DeepSeek provider-reported usage.

Per sample: send [fixed 1-token-ish system, sample text as one user message],
max_tokens=1. actual_content = prompt_tokens - framing (framing measured by a
near-empty probe). Estimate = the REAL production estimator via the example
shim. Corpus = real repository content, not synthetic repeats.
"""
import json, os, subprocess, sys, urllib.request, random

API = os.environ.get("DEEPSEEK_BASE_URL", "https://api.deepseek.com") + "/chat/completions"
import re as _re
_cfg = open(os.path.expanduser("~/.leveler/config.toml")).read()
KEY = _re.search(r'\[providers\.deepseek\].*?^api_key\s*=\s*"([^"]+)"', _cfg, _re.S|_re.M).group(1)
# the shell proxy intercepts and 401s; go direct
_h = urllib.request.ProxyHandler({})
urllib.request.install_opener(urllib.request.build_opener(_h))
MODEL = "deepseek-chat"  # v4-flash equivalent endpoint alias? -> use explicit
MODEL = sys.argv[1] if len(sys.argv) > 1 else "deepseek-chat"
REPO = "/Users/mengmian/Develop/app/codeleveler"
SHIM = f"{REPO}/target/debug/examples/estimate_tokens"

def provider(messages, max_tokens=1):
    body = json.dumps({"model": MODEL, "messages": messages,
                       "max_tokens": max_tokens, "stream": False}).encode()
    req = urllib.request.Request(API, data=body, headers={
        "Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        d = json.load(r)
    u = d["usage"]
    return u["prompt_tokens"], u.get("prompt_cache_hit_tokens", 0)

def estimate(text):
    p = subprocess.run([SHIM], input=json.dumps({"role": "user", "text": text}) + "\n",
                       capture_output=True, text=True, check=True)
    return int(p.stdout.strip())

def slice_of(path, target_bytes, seed):
    data = open(path, encoding="utf-8", errors="ignore").read()
    if len(data) <= target_bytes:
        return data
    rnd = random.Random(seed)
    start = rnd.randrange(0, len(data) - target_bytes)
    return data[start:start + target_bytes]

def cat_files(paths, target_bytes, seed):
    out, rnd = [], random.Random(seed)
    paths = list(paths); rnd.shuffle(paths)
    total = 0
    for p in paths:
        t = open(p, encoding="utf-8", errors="ignore").read()
        out.append(t); total += len(t)
        if total >= target_bytes: break
    return "".join(out)[:target_bytes]

import glob
CLASSES = {
  "rust-source":  lambda n,s: cat_files(glob.glob(f"{REPO}/crates/leveler-agent/src/**/*.rs", recursive=True), n, s),
  "ts-web":       lambda n,s: cat_files(glob.glob(f"{REPO}/crates/leveler-web/web/**/*.ts*", recursive=True) or
                                        glob.glob(f"{REPO}/crates/**/*.ts", recursive=True), n, s),
  "json-config":  lambda n,s: cat_files(glob.glob(f"{REPO}/schemas/*.json") + glob.glob(f"{REPO}/evals/baselines/*.json"), n, s),
  "tool-output":  lambda n,s: cat_files(glob.glob(f"{REPO}/testdata/*.json") + glob.glob(f"{REPO}/evals/baselines/*.txt"), n, s),
  "mixed-docs":   lambda n,s: cat_files(glob.glob(f"{REPO}/docs/*.md"), n, s),
  "cjk-heavy":    lambda n,s: cat_files([p for p in glob.glob(f"{REPO}/docs/*.md")
                                         if sum(1 for c in open(p,encoding='utf-8',errors='ignore').read()[:2000] if ord(c)>127)>200], n, s),
}
SIZES = {"small": 8_000, "medium": 32_000, "large": 120_000}

def main():
    # framing baseline: two near-empty probes
    f1, _ = provider([{"role": "user", "content": "x"}])
    f2, _ = provider([{"role": "user", "content": "y"}])
    framing = min(f1, f2) - 1  # 1 char ≈ 1 token content
    print(f"framing ≈ {framing} tokens (probes {f1},{f2})", file=sys.stderr)
    rows = []
    for cls, gen in CLASSES.items():
        for size, target in SIZES.items():
            text = gen(target, 42)
            if not text or len(text) < target // 2:
                print(f"skip {cls}/{size}: corpus too small ({len(text)})", file=sys.stderr)
                continue
            est = estimate(text)
            actual_total, cached = provider([{"role": "user", "content": text}])
            actual = actual_total - framing
            err = est - actual
            rows.append(dict(sample=f"{cls}/{size}", chars=len(text), estimate=est,
                             actual=actual, signed_error=err,
                             error_pct=round(100*err/actual, 1), cached=cached))
            print(f"{cls:12}/{size:6} est={est:>7} actual={actual:>7} err={100*err/actual:+.1f}%", file=sys.stderr)
    json.dump(dict(framing=framing, model=MODEL, rows=rows), open(sys.argv[2], "w"), indent=1)

main()
