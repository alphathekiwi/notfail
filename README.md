# notfail

A CLI tool that runs test commands and filters out the noise, showing only what matters: failures and a progress bar.

## The Problem

Running `npm test` or `vitest` produces hundreds of lines of output - most of it successful test results you don't care about. When tests fail, the actual error gets buried in the noise.

## The Solution

`notfail` wraps your test command and:

- **Hides passing tests** - No more walls of green checkmarks
- **Shows a progress bar** - See how far along your test suite is
- **Displays only failures** - When something breaks, you see exactly what failed
- **Remembers test counts** - Progress bars work immediately on subsequent runs

## Installation

### From Source

```bash
cargo install --path .
```

### From Releases

Download the appropriate binary for your platform from the [Releases](https://github.com/yourusername/notfail/releases) page.

## Usage with AI Agents (Claude Code, ACP)

AI agents running commands through ACP (Agent Control Protocol) or similar non-interactive shells do not load shell configuration files like `~/.bashrc` or `~/.zshrc`. This means shell function overrides from [TERMINAL_OVERRIDES.md](TERMINAL_OVERRIDES.md) won't apply.

To use `notfail` with AI agents, add instructions to your project's `CLAUDE.md` or agent configuration:

```
Use `notfail` instead of `npm test` to run tests. To override the command with arguments (e.g., to specify a specific test file), use `notfail -c "npm test --silent <args>"` and set a timeout of 300 seconds as the tests are long running.
```

## Usage in Terminal

You can also [override `npm test`](TERMINAL_OVERRIDES.md) in your shell to use notfail automatically.

```bash
# Run with default command (npm test -- --silent)
notfail

# Run a custom command
notfail -c "vitest run"

# Output to a file (captures full output)
notfail -c "npm test" -f test-output.log

# Allow multiple instances (disables singleton mode)
notfail -m

# Restart a running instance
notfail -r
```

## Options

| Flag | Long | Description |
|------|------|-------------|
| `-c` | `--cmd` | Command to run (default: `npm test -- --silent`) |
| `-f` | `--file` | Write full output to a file |
| `-m` | `--multi` | Allow multiple instances (disable singleton) |
| `-r` | `--restart` | Restart if already running |

## Features

### Singleton Mode

By default, `notfail` runs as a singleton per command. If you run `notfail` while tests are already running:

- It attaches to the existing process
- Shows the same progress bar and output
- Multiple terminals can watch the same test run

Use `-m` to disable this and run independent instances.

### Progress Bar

```
[████████████████░░░░░░░░░░░░░░░░░░░░░░░░]  42% (835/1987)
```

The progress bar shows:
- Visual progress indicator
- Percentage complete
- Current/total test count

On the first run in a directory, `notfail` learns the total test count. Subsequent runs show accurate progress immediately.

### Failure Detection

`notfail` captures and displays:
- `FAIL` markers
- Error messages and stack traces
- Jest/Vitest failure indicators (`●`, `✗`, `×`)
- Assertion errors with expected/received values

### Directory Memory

`notfail` remembers recently used directories. If you run it from a directory without a `package.json`, it offers to switch to a recent project:

```
No package.json found. Select a recent directory:
0> /Users/you/projects/app
1> /Users/you/projects/library

Press 0-1:
```

## How It Works

1. **PTY Allocation** - Runs your command in a pseudo-terminal to preserve colors and formatting
2. **Output Parsing** - Scans output for test progress and failure patterns
3. **Buffering** - Captures failure blocks while filtering routine output
4. **IPC** - Uses Unix domain sockets for singleton coordination
5. **Caching** - Stores test counts per directory in `~/.cache/notfail/`

## Supported Test Runners

Primarily tested with:
- Vitest
- Jest

The failure detection patterns should work with most test runners that output standard pass/fail indicators.


## License

MIT
