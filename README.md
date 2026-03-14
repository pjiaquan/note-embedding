# note-embedding

Watch a directory, generate embeddings for supported documents, store the results in SQLite, and expose search/retrieval through Telegram.

## Build

```bash
cargo build --release
```

Binary path:

```text
target/release/note-embedding
```

## First Run

Run the binary once to generate the default config:

```bash
./target/release/note-embedding
```

Default config path:

```text
~/.config/note-embedding/config
```

Then edit the config and set at least:

- `watch_dir`
- `processed_dir`
- `file_types`
- `[embedding]`
- `[embedding.ollama]` or `[embedding.gemini]`
- `[telegram]` if Telegram is enabled

Verify the setup:

```bash
./target/release/note-embedding --doctor
```

If Telegram is enabled and you want a default chat ID for push notifications:

```bash
./target/release/note-embedding --telegram-discover-chat
```

## Run Manually

```bash
./target/release/note-embedding
```

With an explicit config path:

```bash
./target/release/note-embedding --config ~/.config/note-embedding/config
```

## Install As Service

This project installs itself as a `systemd --user` service.

Requirements:

- `systemctl --user` must work in your session
- the binary must already be built

Install and start:

```bash
./target/release/note-embedding --service start
```

This will:

- create/update `~/.config/systemd/user/note-embedding.service`
- create/update the symlink `~/.local/bin/note-embedding`
- enable and start the user service

Useful service commands:

```bash
./target/release/note-embedding --service status
./target/release/note-embedding --service log
./target/release/note-embedding --service restart
./target/release/note-embedding --service stop
./target/release/note-embedding --service uninstall
```

`--service uninstall` removes the user service, symlink, config, and app data.

## Telegram Search

Use `/s <keywords>` in Telegram to search similar documents.

Behavior:

- one strong match: the bot sends the document
- multiple matches: the bot shows 5 results per page with `Left` and `Right` buttons
- tapping a result button sends the selected document

## Notes

- embeddings are stored in SQLite as JSON vectors
- processed files are moved to `processed_dir` only after embedding generation and database write succeed
- search-session pagination is stored in memory and is lost when the service restarts
