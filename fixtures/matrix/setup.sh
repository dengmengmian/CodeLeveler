# Regenerate the synthesized fixture projects, deterministically.
#
# `mk` deletes before it creates, so it MUST be anchored: run from the wrong
# directory and an unanchored `rm -rf "$1"` removes whatever happens to share
# the name. Everything below is resolved against this script's own directory
# and refuses to touch anything outside it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
case "$ROOT" in
  */fixtures/matrix) : ;;
  *) echo "refusing to run: expected .../fixtures/matrix, got $ROOT" >&2; exit 1 ;;
esac
cd "$ROOT"

mk() {
  case "$1" in
    ""|.|..|/*|*/*) echo "refusing to remove suspicious target: $1" >&2; exit 1 ;;
  esac
  rm -rf "${ROOT:?}/$1"
  mkdir -p "${ROOT:?}/$1"
}

# 1. python-poetry
mk py-lib && cd py-lib
cat > pyproject.toml <<'E'
[project]
name = "slugify-lite"
version = "0.2.0"
description = "Tiny slug generator"
requires-python = ">=3.9"
E
mkdir -p src/slugify_lite tests
cat > src/slugify_lite/__init__.py <<'E'
import re

_SEP = re.compile(r"[^a-z0-9]+")

def slugify(text: str, sep: str = "-") -> str:
    """Lowercase, strip non-alphanumerics, collapse separators."""
    out = _SEP.sub(sep, text.strip().lower())
    return out.strip(sep)
E
cat > tests/test_slugify.py <<'E'
from slugify_lite import slugify

def test_basic():
    assert slugify("Hello World") == "hello-world"

def test_collapses():
    assert slugify("  A -- B  ") == "a-b"
E
cd ..

# 2. c-make
mk c-lib && cd c-lib
cat > Makefile <<'E'
CFLAGS = -Wall -Wextra -std=c11
all: libring.a
libring.a: ring.o
	ar rcs $@ $^
ring.o: ring.c ring.h
	$(CC) $(CFLAGS) -c ring.c
test: all
	$(CC) $(CFLAGS) test_ring.c libring.a -o test_ring && ./test_ring
clean:
	rm -f *.o *.a test_ring
E
cat > ring.h <<'E'
#ifndef RING_H
#define RING_H
#include <stddef.h>
typedef struct { int *buf; size_t cap, head, len; } ring_t;
int  ring_init(ring_t *r, size_t cap);
int  ring_push(ring_t *r, int v);
int  ring_pop(ring_t *r, int *out);
void ring_free(ring_t *r);
#endif
E
cat > ring.c <<'E'
#include "ring.h"
#include <stdlib.h>

int ring_init(ring_t *r, size_t cap) {
    r->buf = calloc(cap, sizeof(int));
    if (!r->buf) return -1;
    r->cap = cap; r->head = 0; r->len = 0;
    return 0;
}

int ring_push(ring_t *r, int v) {
    if (r->len == r->cap) return -1;
    r->buf[(r->head + r->len) % r->cap] = v;
    r->len++;
    return 0;
}

int ring_pop(ring_t *r, int *out) {
    if (r->len == 0) return -1;
    *out = r->buf[r->head];
    r->head = (r->head + 1) % r->cap;
    r->len--;
    return 0;
}

void ring_free(ring_t *r) { free(r->buf); r->buf = NULL; }
E
cat > test_ring.c <<'E'
#include "ring.h"
#include <assert.h>
int main(void) {
    ring_t r; assert(ring_init(&r, 3) == 0);
    assert(ring_push(&r, 1) == 0);
    assert(ring_push(&r, 2) == 0);
    int v; assert(ring_pop(&r, &v) == 0 && v == 1);
    ring_free(&r);
    return 0;
}
E
cd ..

# 3. shell-scripts
mk shell-tools && cd shell-tools
mkdir -p bin
cat > bin/backup.sh <<'E'
#!/usr/bin/env bash
# Rotating backup helper.
set -euo pipefail
SRC="${1:?usage: backup.sh <src> <dest>}"
DEST="${2:?usage: backup.sh <src> <dest>}"
KEEP="${KEEP:-7}"
stamp=$(date +%Y%m%d-%H%M%S)
mkdir -p "$DEST"
tar czf "$DEST/backup-$stamp.tar.gz" -C "$SRC" .
ls -1t "$DEST"/backup-*.tar.gz | tail -n +$((KEEP + 1)) | xargs -r rm -f
echo "backup-$stamp.tar.gz"
E
chmod +x bin/backup.sh
cat > README.md <<'E'
# shell-tools

Small operational scripts. `bin/backup.sh` makes a timestamped tarball and
prunes all but the newest `$KEEP` (default 7).
E
cd ..

# 4. docs-only
mk docs-site && cd docs-site
mkdir -p docs
cat > README.md <<'E'
# Ledger API docs

Reference for the Ledger HTTP API. See `docs/` for endpoint pages.
E
cat > docs/authentication.md <<'E'
# Authentication

All requests carry `Authorization: Bearer <token>`. Tokens expire after
3600 seconds; refresh with `POST /v1/token/refresh`.

| Code | Meaning |
| --- | --- |
| 401 | missing or malformed token |
| 403 | token valid, scope insufficient |
E
cat > docs/entries.md <<'E'
# Entries

`GET /v1/entries` returns a paginated ledger. Pagination is cursor based:
pass `?after=<cursor>`; the response carries `next_cursor` until exhausted.

Amounts are integer minor units. Never floats.
E
cd ..

# 5. yaml-config (infra)
mk infra-config && cd infra-config
mkdir -p envs
cat > docker-compose.yml <<'E'
services:
  api:
    image: ghcr.io/example/api:1.4.2
    ports: ["8080:8080"]
    environment:
      DATABASE_URL: postgres://app@db:5432/app
    depends_on: [db]
  db:
    image: postgres:16
    volumes: ["pgdata:/var/lib/postgresql/data"]
volumes:
  pgdata: {}
E
cat > envs/production.yaml <<'E'
replicas: 6
resources:
  cpu: "2"
  memory: 4Gi
featureFlags:
  newCheckout: true
  legacyExport: false
E
cd ..

# 6. java-maven
mk java-lib && cd java-lib
cat > pom.xml <<'E'
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>retry</artifactId>
  <version>0.1.0</version>
  <properties><maven.compiler.release>17</maven.compiler.release></properties>
</project>
E
mkdir -p src/main/java/com/example
cat > src/main/java/com/example/Retry.java <<'E'
package com.example;

import java.util.function.Supplier;

/** Bounded retry with exponential backoff. */
public final class Retry {
    private Retry() {}

    public static <T> T withRetries(int attempts, long baseMillis, Supplier<T> op) {
        RuntimeException last = null;
        for (int i = 0; i < attempts; i++) {
            try {
                return op.get();
            } catch (RuntimeException e) {
                last = e;
                try {
                    Thread.sleep(baseMillis * (1L << i));
                } catch (InterruptedException ie) {
                    Thread.currentThread().interrupt();
                    throw e;
                }
            }
        }
        throw last;
    }
}
E
cd ..

# 7. failing-tests (pre-existing red)
mk red-repo && cd red-repo
cat > Cargo.toml <<'E'
[package]
name = "red-repo"
version = "0.1.0"
edition = "2021"
E
mkdir -p src
cat > src/lib.rs <<'E'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn mul(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_works() {
        assert_eq!(add(2, 2), 4);
    }

    // Pre-existing failure: this repo is ALREADY red before the agent touches it.
    #[test]
    fn mul_is_broken_on_purpose() {
        assert_eq!(mul(3, 3), 10);
    }
}
E
cd ..

# 8. monorepo
mk monorepo && cd monorepo
cat > package.json <<'E'
{
  "name": "acme-monorepo",
  "private": true,
  "workspaces": ["packages/*"]
}
E
mkdir -p packages/core/src packages/cli/src
cat > packages/core/package.json <<'E'
{ "name": "@acme/core", "version": "1.0.0", "main": "src/index.js" }
E
cat > packages/core/src/index.js <<'E'
export function parseDuration(text) {
  const m = /^(\d+)(ms|s|m|h)$/.exec(text.trim());
  if (!m) throw new Error(`bad duration: ${text}`);
  const n = Number(m[1]);
  return { ms: n, s: n * 1e3, m: n * 6e4, h: n * 36e5 }[m[2]];
}
E
cat > packages/cli/package.json <<'E'
{ "name": "@acme/cli", "version": "1.0.0", "dependencies": { "@acme/core": "1.0.0" } }
E
cat > packages/cli/src/main.js <<'E'
import { parseDuration } from "@acme/core";
const [, , arg] = process.argv;
console.log(parseDuration(arg ?? "500ms"));
E
cd ..

# 9. empty git repo
mk empty-repo && cd empty-repo
cat > .keep <<'E'
E
cd ..

# 10. non-git dir (no VCS at all)
mk nogit-notes && cd nogit-notes
cat > todo.md <<'E'
# Notes

- reconcile the invoice importer
- the CSV parser chokes on quoted newlines
E
cat > data.csv <<'E'
id,name,amount
1,"Acme, Inc",1200
2,"Line
Break Co",890
E
cd ..

# 11. sql/data project
mk sql-warehouse && cd sql-warehouse
mkdir -p models
cat > models/orders.sql <<'E'
-- Daily order rollup.
select
    date_trunc('day', created_at) as day,
    count(*)                      as orders,
    sum(amount_cents) / 100.0     as revenue
from raw.orders
where status = 'settled'
group by 1
order by 1 desc
E
cat > models/customers.sql <<'E'
select
    c.id,
    c.email,
    min(o.created_at) as first_order_at,
    count(o.id)       as lifetime_orders
from raw.customers c
left join raw.orders o on o.customer_id = c.id
group by 1, 2
E
cat > README.md <<'E'
# sql-warehouse

Analytics models. Each file in `models/` is one materialized view.
E
cd ..

# 12. mixed polyglot with dirty worktree
mk polyglot && cd polyglot
cat > go.mod <<'E'
module example.com/polyglot

go 1.21
E
cat > main.go <<'E'
package main

import "fmt"

func Fib(n int) int {
	if n < 2 {
		return n
	}
	a, b := 0, 1
	for i := 2; i <= n; i++ {
		a, b = b, a+b
	}
	return b
}

func main() { fmt.Println(Fib(30)) }
E
mkdir -p scripts web
cat > scripts/gen.py <<'E'
"""Generate a fixture table of Fibonacci values."""
def fib(n):
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a

if __name__ == "__main__":
    for i in range(15):
        print(i, fib(i))
E
cat > web/index.html <<'E'
<!doctype html>
<meta charset="utf-8">
<title>polyglot</title>
<h1>Fibonacci</h1>
<pre id="out"></pre>
<script>
  const out = [];
  let [a, b] = [0, 1];
  for (let i = 0; i < 15; i++) { out.push(`${i} ${a}`); [a, b] = [b, a + b]; }
  document.getElementById("out").textContent = out.join("\n");
</script>
E
cd ..

# Deterministic git baseline: every project starts committed, EXCEPT the two
# that deliberately are not (a non-git directory and a dirty worktree). A
# fixture whose baseline depends on what a previous run left behind is not a
# fixture.
for d in py-lib c-lib shell-tools docs-site infra-config java-lib red-repo \
         monorepo empty-repo sql-warehouse polyglot; do
  (
    cd "$ROOT/$d"
    rm -rf .git
    git init -q
    git add -A
    git -c user.email=fixture@example.com -c user.name=fixture commit -qm "fixture baseline"
  )
done
# polyglot keeps one uncommitted edit on purpose (dirty-worktree coverage).
echo "// uncommitted local edit" >> "$ROOT/polyglot/main.go"
# nogit-notes stays outside version control on purpose.

echo "created $(ls -d */ | wc -l) projects"
