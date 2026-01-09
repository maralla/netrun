netrun
======

Run commands in an isolated network namespace.

Requirements
------------

- Linux with network namespace support
- Root privileges (required for network namespace operations)

Build
-----

```bash
cargo build
```

Usage
-----

```bash
# Reads default `run.yaml` from the current directory.
sudo netrun
# or
sudo ./target/debug/netrun -c config.yaml
```

Config
------

Create a YAML file (e.g., `run.yaml`) with your network settings and commands:

```yaml
network:
  host_ip: "10.200.1.1/24"    # IP on the host side (optional, defaults to 10.200.1.1/24)
  ns_ip: "10.200.1.2/24"      # IP inside the namespace (optional, defaults to 10.200.1.2/24)

commands:
  - name: "web server"
    run: "python3 -m http.server 8080"

  - name: "ping test"
    run: "ping -c 5 10.200.1.1"
```

Misc
----

- All commands start simultaneously in the same network namespace
- The program waits for all commands to exit
- Press **Ctrl+C** once to send SIGTERM to all commands (graceful shutdown)
- Press **Ctrl+C** again to force kill remaining processes
- Network namespace and interfaces are automatically cleaned up on exit

License
-------

[MIT](LICENSE)
