# Terminal Overrides

Override `npm test` to automatically use `notfail` in your shell.

## Fish

Add to `~/.config/fish/config.fish`:

```fish
function npm
    if test (count $argv) -ge 1 -a "$argv[1]" = "test"
        if test (count $argv) -gt 1
            notfail -c "npm test "(string escape -- $argv[2..-1] | string join ' ')
        else
            notfail
        end
    else
        command npm $argv
    end
end
```

Reload with:
```bash
source ~/.config/fish/config.fish
```

## Bash

Add to `~/.bashrc`:

```bash
npm() {
    if [[ "$1" == "test" ]]; then
        if [[ $# -gt 1 ]]; then
            shift
            notfail -c "npm test $*"
        else
            notfail
        end
    else
        command npm "$@"
    fi
}
```

Reload with:
```bash
source ~/.bashrc
```

## Zsh

Add to `~/.zshrc`:

```zsh
npm() {
    if [[ "$1" == "test" ]]; then
        if [[ $# -gt 1 ]]; then
            shift
            notfail -c "npm test $*"
        else
            notfail
        fi
    else
        command npm "$@"
    fi
}
```

Reload with:
```bash
source ~/.zshrc
```

## PowerShell

Add to your PowerShell profile (`$PROFILE`):

```powershell
function npm {
    if ($args[0] -eq "test") {
        if ($args.Count -gt 1) {
            $testArgs = $args[1..($args.Count - 1)] -join ' '
            & notfail -c "npm test $testArgs"
        } else {
            & notfail
        }
    } else {
        & npm.cmd @args
    }
}
```

Reload with:
```powershell
. $PROFILE
```

## Usage

After setting up the override, use npm as normal:

```bash
# Runs through notfail
npm test

# Passes additional arguments
npm test -- --watch
npm test -- --coverage

# Other npm commands work normally
npm install
npm run build
```

## Removing the Override

To temporarily bypass the override and use the real npm:

```bash
# Fish/Bash/Zsh
command npm test

# PowerShell
npm.cmd test
```

To permanently remove, delete the function from your shell config file and reload.
