# Installation

The recommended way of installing `magnet` is via a shell script, which also allows that `magnet` can update itself through a command.
This will install `magnet` according from <https://github.com/capital-G/magnet_cli/releases> on your machine and add it to your `PATH` environment such that magnet becomes available as a shell command.

By default, magnet will be installed to `~/.magnet/bin`

**macOS and Linux**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/capital-G/magnet_cli/releases/latest/download/magnet-installer.sh | sh
```

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/capital-G/magnet_cli/releases/latest/download/magnet-installer.ps1 | iex"
```

Open a new terminal and check if `magnet --version` works.

## Updating

If installed via the script, magnet can update itself via

```sh
magnet self update
```

Magnet also checks once a day if there has been a new release and will notify you about it.
Set `MAGNET_NO_UPDATE_CHECK=1` to disable this check.
