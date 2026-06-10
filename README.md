# sqweel

A desktop database client and administration tool, built with GTK4 +
libadwaita. PostgreSQL today; the data layer sits behind traits so other
databases can be added later.

> **Note:** sqweel is a vibe-coded project — written largely through
> conversational, AI-assisted iteration rather than up-front design. Treat it
> as a playground/work-in-progress, not production software. Expect rough
> edges.

## Features

- **Connect** — host/port/database form with "Test connection", saved
  connections (stored as JSON in `~/.config/sqweel/`), and passwords kept in
  the OS secret service (keyring).
- **Schema browser** — sidebar listing tables and views per schema, with
  fast row-count estimates and search.
- **Data grid** — infinite-scroll paging, server-side sort and search,
  enum-like status badges, primary-key / foreign-key decorations, inline cell
  editing, and an "Add row" form.
- **Structure & Indexes** tabs per relation.
- **SQL editor** — a GtkSourceView scratch editor (syntax highlighting, line
  numbers). Queries (`SELECT`, `WITH`, `SHOW`, `EXPLAIN`, `VALUES`, `TABLE`)
  render in a results grid; other statements run and report rows affected.
  Run with the button or `Ctrl+Enter`.

## Stack

- **UI:** GTK4 (`gtk4`), libadwaita (`libadwaita`), Blueprint (`.blp` → `.ui`),
  GtkSourceView (`sourceview5`)
- **Database:** `sqlx` (Postgres, rustls), async via `tokio`
- **Language:** Rust (edition 2024)

The database core is database-agnostic: everything DB-specific lives behind the
`Driver` / `Connection` traits (`src/db/`), and the UI only ever talks to
`Box<dyn ...>` and neutral value types.

## Building & running

The project uses a Nix flake dev shell that wires up GTK, libadwaita,
Blueprint, GtkSourceView, and the runtime environment variables they need
(`LD_LIBRARY_PATH`, `XDG_DATA_DIRS`, GSettings schemas, icon themes).

```sh
nix develop          # enter the dev shell (or use direnv: `direnv allow`)
cargo run            # launch the app
```

Without Nix you'll need GTK4, libadwaita (≥ 1.4), GtkSourceView 5,
blueprint-compiler, and a Rust toolchain installed system-wide, plus the
matching `XDG_DATA_DIRS` / library paths.

## Trying it against a throwaway database

```sh
docker run -d --name sqweel-pg \
  -e POSTGRES_USER=marwa -e POSTGRES_PASSWORD=marwa -e POSTGRES_DB=analytics \
  -p 5432:5432 postgres:16
```

Then connect with host `localhost`, port `5432`, database `analytics`, user
`marwa`, password `marwa`.

## Smoke tests

GTK can't run inside a plain `cargo test` (it needs the main thread + a
display), so headed smoke tests live under `examples/`. They connect to a
seeded local Postgres, build the UI, exercise a path, and exit 0 if nothing
panics:

```sh
cargo run --example gui_smoke   # sidebar + table grid
cargo run --example sql_smoke   # SQL editor: open, run a query, render
```

Unit tests (SQL builders, statement routing) run normally:

```sh
cargo test --lib
```

## Project layout

```
src/
  db/            database core (traits, types, Postgres driver, registry)
  window.rs      connect page + saved connections
  main_view.rs   sidebar + tabbed content + status chrome
  table_view.rs  per-relation view (data / structure / indexes)
  result_grid.rs reusable read-only result grid
  sql_view.rs    SQL editor
  row_object.rs  result row wrapped as a GObject for the grid
  store.rs       saved-connection persistence + keyring
  runtime.rs     tokio runtime bridge for the GTK main loop
resources/       Blueprint UI (.blp), CSS, gresource manifest
```
