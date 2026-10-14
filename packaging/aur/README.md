# Arch Linux / CachyOS AUR packaging

This directory contains a split AUR recipe with three install choices:

| Package | Contents |
|---|---|
| `rustinfo-git` | Monitor and `smuctl` binary; sufficient for Curve Optimizer writes |
| `rustinfo-smu-dkms-git` | Read-only telemetry module sources; DKMS rebuilds for installed kernels |
| `rustinfo-full-git` | Both packages, with telemetry loaded after installation and on boot on supported Strix Point CPUs |

The recipe does not invoke `install.sh`. The binary is installed in `/usr/bin`, and
privileged commands use normal `sudo`. No passwordless sudoers rule is created.
Installing a package does not apply or persist any Curve Optimizer value.

## Install after AUR publication

For a complete setup, install the matching kernel headers in the same transaction:

```bash
yay -S rustinfo-full-git "$(cat /usr/lib/modules/"$(uname -r)"/pkgbase)-headers"
sudo rustinfo dump
```

For example, the header package is `linux-headers` for Arch's standard kernel,
`linux-cachyos-headers` for CachyOS, and `linux-cachyos-rc-headers` for its RC kernel.
The header version must match the installed kernel. If an upgrade has removed the
running kernel's headers, reboot into the updated kernel before checking telemetry.
DKMS cannot build a module without that kernel's headers.

For the application and undervolting alone:

```bash
yay -S rustinfo-git
sudo rustinfo smuctl list
```

The full package's service matches AMD family `0x1A`, model `0x24` before
loading SMU telemetry. The module repeats this check before mailbox access.
Other CPUs keep the application and skip automatic SMU probing. The service does
not unload other drivers; stop a previously installed competing `ryzen_smu`
driver before enabling this telemetry channel. Avoid simultaneous SMU writers.

After installation, check `dkms status` and `systemctl status rustinfo-smu.service`.
If module loading is rejected by Secure Boot, enroll the DKMS signing key using
your distribution's procedure. Do not disable Secure Boot automatically.
If `/usr/local/bin/rustinfo` remains from an older `install.sh` deployment, it can
shadow the package binary; use `/usr/bin/rustinfo` or remove that older copy yourself.

The full package installs a vendor service dependency for automatic boot loading.
To opt out, use `sudo systemctl mask --now rustinfo-smu.service` (a loaded module
remains loaded until reboot or manual removal). Uninstalling `rustinfo-full-git`
removes the service dependency; uninstalling the DKMS package lets pacman's DKMS
hook remove its installed modules. Neither action resets a CO value already sent.

## Build before publication

Use a regular user; `makepkg` deliberately refuses to run as root:

```bash
cd packaging/aur
makepkg -s
sudo pacman -U rustinfo-{git,smu-dkms-git,full-git}-*.pkg.tar.zst
```

This source recipe becomes buildable from GitHub once its support files are merged
into upstream `main`. To test an unmerged PR, change the source URL's ref locally
to that PR's branch, or provide a local Git source containing those files.

## Publish

Publication requires an AUR account and an SSH public key registered to that
account. Add your maintainer identity to the PKGBUILD, then, after the upstream
support files are merged:

```bash
git clone ssh://aur@aur.archlinux.org/rustinfo-git.git aur-rustinfo
cp packaging/aur/PKGBUILD aur-rustinfo/PKGBUILD
cd aur-rustinfo
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m 'Initial rustinfo split packages'
git push origin master
```

Only `PKGBUILD` and `.SRCINFO` are needed in the AUR Git repository: the recipe
fetches source and service support files from upstream GitHub. AUR provides build
recipes; users compile the application locally. The `.ko` is built on their own
machine by DKMS, so no module is tied to the publisher's kernel.
