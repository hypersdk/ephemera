# Building custom OS images with `ephemera build-image`

`ephemera build-image` takes a base disk image and applies customizations —
hostname, package installs, arbitrary commands, SSH-key injection, file
copy-in, and systemd service enablement — to produce a new, ready-to-boot
image. Everything runs through [`guestkit`](https://github.com/hypersdk/guestkit)
mounting the image directly with `qemu-nbd` and running commands in a
`chroot`. There's no libguestfs appliance and **no VM boot involved** — this
means `build-image` doesn't need `/dev/kvm` at all, only root and the `nbd`
kernel module.

This doc is a set of copy-pasteable, real-hardware-verified tutorials for the
three package-manager families `build-image` supports: Debian/Ubuntu
(`apt`), RHEL-family (`dnf`/`tdnf`/`yum`), and Arch Linux (`pacman`). Every
example below was actually run against a real cloud image of that distro —
see `scripts/test-image-customize.sh` for the automated version of the same
checks (and the CI job that runs it on every push, across all three
families).

## How it works, briefly

For each field in the request:

| Field | What it does | Needs network? |
|---|---|---|
| `hostname` | Writes `/etc/hostname` | No |
| `packages` | Detects the guest's package manager (see below), installs via it | **Yes** |
| `commands` | Runs each string via `sh -c` inside the chroot | Depends on the command |
| `ssh_key` | Appends to `/root/.ssh/authorized_keys`, creates the dir if needed, sets `0600` | No |
| `copy_in` | Copies a host file into the image at the given path | No |
| `enable_services` | Runs `systemctl enable <name>` for each unit | No |

`packages` is the one field that needs real outbound networking from
whatever host you run `build-image` on — the guest's package manager has to
actually reach its package repositories. `ephemera build-image` handles this
automatically: it stages a working `/etc/resolv.conf` into the guest for the
duration of the install (a stock cloud image's own `resolv.conf` is usually
a dangling symlink that only resolves under a running systemd instance) and
removes it again afterward. You don't need to do anything for this — it's
mentioned here so a "package not found" or DNS-looking error makes sense if
your *host's* own DNS/network is the thing that's actually broken.

### Package-manager detection

`install_packages` doesn't trust image metadata — it execs `command -v
<tool>` inside the chroot and checks what's actually there, in this order:

```
apt-get → tdnf → dnf → yum → pacman
```

If none of these are found, `packages` fails with a clear error telling you
to use an equivalent `commands` entry instead (e.g. a static binary drop-in,
or a manual `rpm -i` of a local package). This also means an image with an
unusual/custom package manager (Alpine's `apk`, for instance) isn't
supported yet — `commands` is your escape hatch there.

## Prerequisites

```bash
sudo modprobe nbd max_part=16   # done once per boot; scripts/bootstrap-host.sh does this for you
```

You need root (`qemu-nbd` mount) and enough free disk to hold a copy of the
base image plus the output image. No VMM, no `/dev/kvm`, no GPU — none of
the actual VM backends are involved in building an image.

---

## Debian / Ubuntu (`apt`)

```json
{
  "source": "/var/lib/ephemera/images/ubuntu-noble.qcow2",
  "output": "/var/lib/ephemera/images/ubuntu-dev.qcow2",
  "format": "qcow2",
  "hostname": "ubuntu-dev",
  "packages": ["tree", "jq", "qemu-guest-agent"],
  "commands": [
    "touch /etc/provisioned-by-ephemera"
  ],
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... you@example.com",
  "copy_in": [
    {"src": "/path/to/target/release/ephemera-guest-agent", "dest": "/usr/local/bin/ephemera-guest-agent"}
  ],
  "enable_services": ["qemu-guest-agent", "cron"]
}
```

```bash
sudo ephemera build-image --spec ubuntu-dev.json
```

Notes:
- Ubuntu's stock cloud image enables the `universe` component by default, so
  most common CLI tools (`tree`, `jq`, `htop`, ...) install with no extra
  repo configuration.
- The system cron daemon's unit is `cron.service` on Debian/Ubuntu — not
  `crond` (that's the RHEL-family name, see below).

## RHEL-family — Rocky Linux, AlmaLinux, Fedora (`dnf`)

```json
{
  "source": "/var/lib/ephemera/images/rocky9.qcow2",
  "output": "/var/lib/ephemera/images/rocky9-dev.qcow2",
  "format": "qcow2",
  "hostname": "rocky-dev",
  "packages": ["tree", "jq"],
  "commands": [
    "touch /etc/provisioned-by-ephemera"
  ],
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... you@example.com",
  "enable_services": ["crond"]
}
```

```bash
sudo ephemera build-image --spec rocky-dev.json
```

Notes:
- Rocky's `GenericCloud` image ships `cronie` (providing `crond.service`)
  pre-installed — `enable_services: ["crond"]` works without needing
  `packages` to install it first. If you're building from a minimal/custom
  RHEL-family image that doesn't have it, add `"cronie"` to `packages`.
- Photon OS images (`tdnf`) and older RHEL 7/CentOS 7 images (`yum`) go
  through the exact same code path — `install_packages` picks whichever of
  `tdnf`/`dnf`/`yum` it finds.

## Arch Linux (`pacman`)

```json
{
  "source": "/var/lib/ephemera/images/arch.qcow2",
  "output": "/var/lib/ephemera/images/arch-dev.qcow2",
  "format": "qcow2",
  "hostname": "arch-dev",
  "packages": ["tree", "jq"],
  "commands": [
    "touch /etc/provisioned-by-ephemera"
  ],
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... you@example.com",
  "enable_services": ["sshd"]
}
```

```bash
sudo ephemera build-image --spec arch-dev.json
```

Notes — Arch needs two things every other distro here doesn't, and
`build-image` handles both automatically:

1. **Empty keyring.** A fresh Arch image ships with no trusted pacman
   keyring, so every install fails signature verification until it's
   initialized. `build-image` runs `pacman-key --init` and `pacman-key
   --populate archlinux` before the actual install — this takes a few
   extra seconds the first time, there's nothing you need to configure.
2. **No `/etc/mtab`.** `pacman` refuses to run at all without a readable
   `/etc/mtab` (on a real system that's a symlink to `/proc/self/mounts`,
   which doesn't exist in this bare chroot). `build-image` stages a minimal
   synthetic one for the duration of the install and removes it afterward —
   again, nothing you need to do.
3. Arch's official cloud image doesn't ship `cron`/`cronie` by default, so
   this example enables `sshd` instead (which *is* preinstalled) to
   demonstrate `enable_services`. If you want cron on Arch, add `"cronie"`
   to `packages` and use `enable_services: ["cronie"]`.

---

## Verifying an image without booting it

Since `build-image` never boots a VM, you can sanity-check the output the
same way it was built — by mounting it directly:

```bash
sudo modprobe nbd max_part=16
sudo qemu-nbd -c /dev/nbd0 /var/lib/ephemera/images/ubuntu-dev.qcow2
sudo partprobe /dev/nbd0 && sudo udevadm settle
sudo mount /dev/nbd0p1 /mnt          # partition number varies by image layout
cat /mnt/etc/hostname
ls /mnt/root/.ssh/authorized_keys
sudo umount /mnt
sudo qemu-nbd -d /dev/nbd0
```

`scripts/test-image-customize.sh` automates exactly this (build → mount →
assert every field landed correctly → clean up) and accepts `--image` for
any base image plus `TEST_PACKAGE`/`TEST_SERVICE` overrides so you can point
it at your own distro of choice:

```bash
sudo TEST_SERVICE=crond ./scripts/test-image-customize.sh --image /path/to/rocky9.qcow2
```

## Troubleshooting

- **"cannot install packages: no supported package manager found"** — the
  image's package manager isn't apt/dnf/tdnf/yum/pacman (e.g. Alpine's
  `apk`). Use `commands` to install packages manually instead.
- **A package install fails with a DNS/network-looking error** — check the
  *host's* own network/DNS first; `packages` needs the host machine
  `build-image` runs on to have real outbound connectivity, since that's
  what gets staged into the guest.
- **`enable_services` fails with "unit does not exist"** — the service name
  is distro-specific (`cron` vs `crond`, `ssh` vs `sshd`) or the package
  providing it isn't installed yet. Add it to `packages` first, or check the
  exact unit name for your distro/version.
- **`packages` install of an Arch guest is slow the first time** — that's
  the one-time `pacman-key --init` GPG master-key generation, not a hang.
