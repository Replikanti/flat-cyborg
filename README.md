# flat-cyborg

An asynchronous pseudo-terminal (PTY) wrapper, written in Rust, for
bidirectional I/O interception of interactive command-line applications.

flat-cyborg encapsulates an interactive **Target CLI** inside a virtual PTY so
the target detects a fully-featured TTY and launches in interactive mode rather
than headless mode. It intercepts both I/O streams, emulates human-like input
timing, and deterministically detects the target's lifecycle state by parsing
the output ANSI stream.

## Capabilities

- **PTY & process lifecycle** — spawns the target inside a master/slave PTY pair
  with a fixed geometry (120×40), `TERM=xterm-256color`, and inherited working
  directory and environment. The master end is multiplexed by dedicated
  reader/writer threads (no async runtime), so the caller's thread never blocks
  on PTY I/O.
- **Input jittering** — commands are decomposed into individual UTF-8 characters
  and written with pseudo-random inter-character delays (alphanumerics
  40–120 ms, punctuation/separators 150–300 ms), terminated with a carriage
  return (`\r`).
- **Output ANSI state machine** — a streaming parser strips ANSI escape
  sequences in real time. The wrapper classifies the sanitized stream into a
  `State` (`Running`, `ConfirmationPrompt`, `Idle`); confirmation prompts (e.g.
  `[y/n]`) are answered automatically through the input jitterer.
- **Safety watchdogs** — every operation runs under an execution timeout. On
  timeout the wrapper attempts graceful degradation: `Ctrl+C`, then `SIGKILL` of
  the whole process group after a grace period. The interactive front-end that
  puts the host terminal in raw mode restores it to canonical mode on exit or
  panic via an RAII guard.

## Status

Early development. Components land incrementally via trunk-based development;
see the open pull requests and the issue tracker for the current state.

## Building

```sh
cargo build
cargo test
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
