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

There are **no tests** yet (`cargo test` runs nothing). When adding tests, the README notes the scylla↔CoreDB integration is unverified, so prefer unit tests over anything needing a live DB.

### Running locally requires a live CoreDB

The app calls `Db::connect` at startup and exits if CoreDB isn't reachable. Start it first:

```bash
git clone https://github.com/yhc007/coredb && cd coredb
cargo run -- start --host 127.0.0.1 --port 9042
```

Required env (copy `.env.example`): `ANTHROPIC_API_KEY` is mandatory (panics if unset). `COREDB_NODE`, `BIND_ADDR`, `ANTHROPIC_MODEL` have defaults. The Google OAuth vars exist in `.env.example` but are **not wired up yet**.

`ANTHROPIC_MODEL` defaults to `claude-sonnet-4-6`, but it's configurable — newer/more capable models exist (e.g. `claude-opus-4-8`). Set it via env without code changes; pick the model to fit cost vs. extraction quality.

## Architecture

Four modules under `src/`, each one responsibility:

- **`main.rs`** — axum server, `AppState { db, extractor }`, and 4 routes. `create_entry` is the core flow.
- **`db.rs`** — `Db` wraps an `Arc<Session>` (scylla driver). `connect()` calls `bootstrap()` which runs `CREATE KEYSPACE/TABLE/INDEX IF NOT EXISTS` on every startup (idempotent schema setup — there are no migration files).
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

- `words` and `sentences` are partitioned by `category` with `CLUSTERING ORDER BY (created_at DESC)`. `list_words(None)` therefore loops over all four category partitions and concatenates — there is no cross-partition query.
- Timestamps are stored as `timestamp_millis()` i64.
- `Word.id`/`Sentence.id` use `#[serde(default = "Uuid::new_v4")]` so Claude's JSON doesn't need to supply them.

### Claude extraction contract (`extract.rs`)

The prompt demands a fixed JSON schema: `{"words":[{term,definition,example}],"sentences":[{text,reason}]}`. Because the model may wrap output in code fences, `extract_json_block` slices from the first `{` to the last `}` before `serde_json::from_str`. If you change `Word`/`Sentence`/`Extraction` fields, update the prompt schema string in lockstep or parsing breaks.

## Important constraints

- **CoreDB is a limited single-node CQL implementation** ("needs more testing before production"). `CREATE INDEX` / `IF NOT EXISTS` and some syntax may need adjustment per CoreDB version — check the `cargo run` bootstrap log on first run. If the scylla driver (0.13) can't connect over Native Protocol v4, the documented fallback is to rewrite `db.rs` queries against CoreDB's HTTP API (`POST /query`).
- **No auth yet.** Routes are wide open. Do not expose to the internet before adding the Google OAuth gate + email whitelist (spec item 5); until then restrict by firewall source-range. The `ALLOWED_EMAIL`/`ALLOWED_HD` env vars anticipate this.
- The frontend is a single static `static/index.html` served via `include_str!` (compiled into the binary). `/words` HTML is built by hand-concatenating strings with an `esc()` helper.

## Deployment

CoreDB is a stateful server, so deployment targets a **GCP VM with a persistent disk** (not Cloud Run), running CoreDB + app + Caddy together. The full runbook is `deploy/README.md`; scripts: `provision-vm.sh` (run locally via gcloud), `setup-vm.sh` (run on VM via sudo). App and CoreDB both bind localhost-only; Caddy terminates HTTPS. `backup-coredb.sh` tars the CoreDB data dir daily (14-day retention).
