# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Personal language-learning web app: paste an article/book/paper excerpt, and Claude extracts the hard words and best sentences, which accumulate in a per-category notebook. Stack: Rust (axum) + CoreDB (Cassandra-style NoSQL) + Claude API. Currently an MVP skeleton (single commit). Most prose and code comments are in Korean — match that when editing.

## Commands

```bash
cargo run          # build + serve on $BIND_ADDR (default 0.0.0.0:8080)
cargo build        # compile only
cargo check        # fast type-check, no codegen
cargo clippy       # lint
cargo fmt          # format
```

There are **no tests** yet (`cargo test` runs nothing). Prefer unit tests over anything needing a live CoreDB.

### Running locally requires a live CoreDB (HTTP build)

The app calls `Db::connect` at startup and exits if CoreDB's HTTP API isn't reachable. Start it first. Build CoreDB from source **for your machine's architecture** (a prebuilt binary from another OS/arch fails with "Exec format error"). The `start` subcommand runs an HTTP `/query` server; `--data-dir`/`--commitlog-dir` are global args and must come **before** `start`:

```bash
git clone https://github.com/yhc007/coredb && cd coredb
cargo run -- --data-dir ./data --commitlog-dir ./commitlog start --host 127.0.0.1 --port 9142
# sanity: curl -s localhost:9142/stats
```

Required env (copy `.env.example`): `ANTHROPIC_API_KEY` is mandatory (panics if unset). `COREDB_NODE`, `BIND_ADDR`, `ANTHROPIC_MODEL` have defaults. For local runs without Google login, set `AUTH_DISABLED=1`; otherwise set the `GOOGLE_*`/`OAUTH_REDIRECT_URL`/`ALLOWED_*`/`SESSION_SECRET` vars (see `auth.rs`).

`ANTHROPIC_MODEL` defaults to `claude-sonnet-4-6`, but it's configurable — newer/more capable models exist (e.g. `claude-opus-4-8`). Set it via env without code changes; pick the model to fit cost vs. extraction quality.

## Architecture

Four modules under `src/`, each one responsibility:

- **`main.rs`** — axum server, `AppState { db, extractor, oauth, key }`, route wiring. The 4 app routes sit behind the `auth::require_auth` gate; `/auth/*` are public. `create_entry` is the core flow.
- **`auth.rs`** — Google OAuth2 login gate + email whitelist. `require_auth` middleware redirects unauthenticated requests to `/auth/login`; `/auth/callback` exchanges the code, checks the whitelist, and sets an encrypted `PrivateCookieJar` session (email). `AUTH_DISABLED=1` (or missing `GOOGLE_CLIENT_ID`) bypasses the gate for local dev. No server-side session store.
- **`db.rs`** — `Db` is a `reqwest::Client` + CoreDB HTTP `/query` URL (no scylla; the native protocol's DML result frames are incompatible with the scylla driver). All CQL goes over `POST /query` as `{"query": "..."}`; SELECT responses parse as `{"data":[{"columns":{col:{"Text":..}}}]}`. `connect()` calls `bootstrap()` on every startup (idempotent — already-exists errors are swallowed; no migration files).
- **`extract.rs`** — `Extractor` POSTs to `https://api.anthropic.com/v1/messages` via reqwest (raw HTTP, no SDK). Returns JSON parsed into `Extraction`.
- **`models.rs`** — `Category` enum (nyt/book/paper/other), form input, and the `Word`/`Sentence`/`Extraction` shapes Claude must return.

### The main flow: `POST /entries` (`create_entry`)

1. Parse `category` (reject if invalid).
2. `insert_entry` → store raw text, get `entry_id`.
3. `known_terms()` → fetch all known words from `vocab.known_words`.
4. `extractor.extract(text, known)` → call Claude, which is prompted to **exclude** known words and return strict JSON.
5. Insert each extracted word/sentence, tagged with category + entry_id + source.
6. Redirect to `/words`.

`POST /words/known` adds a term to `known_words` so future extractions skip it — this is the dedup mechanism. There is no per-user scoping; `known_words` is global.

### Data model notes (CoreDB / CQL)

- `words` and `sentences` are partitioned by `category` (compound PK `(category, created_at, id)`). `list_words(None)` loops over all four category partitions and concatenates — there is no cross-partition query. No `CLUSTERING ORDER BY` (CoreDB rejects `WITH` clauses), so rows aren't server-side ordered.
- **No bind parameters** over the HTTP API: `db.rs` inlines values into CQL. `cql_str()` quotes text and maps ASCII `'` → `'` (U+2019), because CoreDB doesn't un-double `''` and breaks on a raw `'`. uuids are bare, `None` → `NULL`, timestamps are bare `timestamp_millis()` ints.
- `Word.id`/`Sentence.id` use `#[serde(default = "Uuid::new_v4")]` so Claude's JSON doesn't need to supply them.

### Claude extraction contract (`extract.rs`)

The prompt demands a fixed JSON schema: `{"words":[{term,definition,example}],"sentences":[{text,reason}]}`. Because the model may wrap output in code fences, `extract_json_block` slices from the first `{` to the last `}` before `serde_json::from_str`. If you change `Word`/`Sentence`/`Extraction` fields, update the prompt schema string in lockstep or parsing breaks.

## Important constraints

- **CoreDB access is over its HTTP `/query` API, not the native protocol.** The scylla native driver couldn't parse CoreDB's DML result frames, so `db.rs` was rewritten to `POST /query` (JSON). `COREDB_NODE` is an HTTP `host:port` (default `127.0.0.1:9142`), not a CQL node.
- **CoreDB's CQL dialect is limited** — the bootstrap schema is shaped around it: keyspace needs `WITH REPLICATION`, tables reject any `WITH` clause, `CREATE INDEX` rejects `IF NOT EXISTS`. `bootstrap()` swallows already-exists errors. If you touch the schema, check the bootstrap log on first run.
- **Auth is a Google OAuth gate** (`auth.rs`), enabled when `GOOGLE_CLIENT_ID` is set and `AUTH_DISABLED` is not. Login is gated by an encrypted session cookie; access requires the email to match `ALLOWED_EMAIL` (comma-separated) or `ALLOWED_HD`. `SESSION_SECRET` seeds the cookie key (ephemeral if unset → sessions reset on restart). Note `time` is pinned to `=0.3.36` in Cargo.toml because `cookie 0.18.1` doesn't compile against newer `time`.
- The frontend is a single static `static/index.html` served via `include_str!` (compiled into the binary). `/words` HTML is built by hand-concatenating strings with an `esc()` helper.

## Deployment

CoreDB is a stateful server, so deployment targets a **GCP VM with a persistent disk** (not Cloud Run), running CoreDB + app + Caddy together. The full runbook is `deploy/README.md`; scripts: `provision-vm.sh` (run locally via gcloud), `setup-vm.sh` (run on VM via sudo). App and CoreDB both bind localhost-only; Caddy terminates HTTPS. `backup-coredb.sh` tars the CoreDB data dir daily (14-day retention).
