# Plyra Terminal for Linux

Build this beta from source on your Linux target. Native checks cover CachyOS x86_64 with Niri/Wayland. Earlier builds also exercised X11. Use a graphical Wayland or X11 session with a working Vulkan or OpenGL driver.

## Build and run

From the repository root, use Rust, Cargo, a C toolchain, and Python 3.

```sh
python3 tools/build_linux_app.py --release \
  --version 0.1.0-beta.1 \
  --output ../plyra-terminal-builds/linux-beta
cd ../plyra-terminal-builds/linux-beta
./bin/glass-terminal
```

The bundle includes the native PTY helper and ordinary system broker beside the application. The packager accepts `--build-number` for a build label. For prebuilt executables, pass `--bin-dir` and the matching `--target`, with `--helper` if the PTY helper is stored separately. The native package inherits the glibc requirements of its executables.

An optional Aether Shell bundle needs both `--aether-shell /path/to/aether-shell` and `--system-bridge /path/to/glass-system-bridge`. Both executables must match the package target. Open Settings with `Ctrl+,` and use **Terminal → New sessions** to select the installed shell. The saved alias is `@aether`.

## Install for your user

Run these from the finished bundle.

```sh
./install.sh --user
./uninstall.sh --user
```

The default prefix is `~/.local`. Use the same `--prefix /absolute/path` for removal if you chose a different prefix. The installer registers the command, desktop entry and icon. Your configuration lives at `$XDG_CONFIG_HOME/glass-terminal/config.json`, or `~/.config/glass-terminal/config.json` by default. Installer updates preserve it. Select **System default** before updating to a bundle without Aether Shell, since that update removes the optional Aether executables.

## Optional system administration

The GUI and ordinary shell sessions run as your user. To enable polkit-backed administrator actions, install the separate helper and policy from the bundle.

```sh
sudo ./install.sh --administrator
```

In the terminal's System panel, choose **Authorize…**. A desktop session uses its registered graphical polkit agent. A foreground session with a controlling terminal can use a temporary `pkttyagent`. Authorization is scoped to the application session. Use **Revoke** in the System panel to end it, and `sudo ./uninstall.sh --administrator` to remove the helper and policy.

The administrator installer checks package hashes and protected system directories. It installs the helper at `/usr/libexec/glass-terminal/plyra-privilege-broker` and policy at `/usr/share/polkit-1/actions/org.plyra.glass-terminal.policy`. For a source-tree installation of those files, use the dedicated installer with the SHA-256 of your built broker.

```sh
sudo python3 packaging/linux/install_broker.py \
  --broker /path/to/plyra-privilege-broker \
  --sha256 EXPECTED_BROKER_SHA256
```

CLI plugins on Linux require `/usr/bin/bwrap` and available kernel namespaces. Workspace writes and network access follow the capabilities selected for the plugin.
