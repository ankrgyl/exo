# Firecracker sandbox artifacts

Exo's optional Firecracker backend accepts either an OCI image reference or an
existing ext4 root filesystem, plus an uncompressed guest kernel and a minimal
initramfs. It launches Firecracker only through the matching `jailer` binary,
communicates with a static Rust PID 1 over virtio-vsock, and adds a TAP device
only when the sandbox requests networking.

Build the CLI with the backend and its OCI/filesystem dependencies enabled:

```bash
cargo build -p exo --features firecracker
```

For an OCI reference, Exo resolves the platform manifest, pulls its layers
directly from the registry, applies OCI whiteouts, creates the guest-owned
workspace directories, and creates ext4 with `mkfs.ext4 -d`. This path does not
use Docker or containerd. Images are cached by immutable manifest-list digest
and platform under `EXO_FIRECRACKER_STATE_ROOT/images/v4`. After the first
materialization, a digest-pinned OCI reference reaches this cache without a
registry request.

The cached ext4 image is an immutable lower layer shared by hard link across
VMs. Every VM gets a new sparse ext4 upper layer, and the initramfs mounts the
pair with OverlayFS. VM startup therefore never copies the base filesystem.
The kernel and initramfs are likewise copied into an immutable host cache once
and hard-linked into each jail.

Build the guest runtime on the Linux KVM host and install it as a trusted host
artifact:

```bash
guest_runtime="$(support/firecracker/build-guest.sh)"
sudo install -o root -g root -m 0755 "$guest_runtime" \
  /var/lib/exo/firecracker/exo-firecracker-guest
sudo support/firecracker/build-initramfs.sh \
  --guest-runtime /var/lib/exo/firecracker/exo-firecracker-guest \
  --output /var/lib/exo/firecracker/exo-firecracker-initramfs.cpio
```

The guest runtime is statically linked and runs directly as PID 1 from the
initramfs. It mounts the immutable base and sparse upper with OverlayFS, mounts
the pseudo filesystems, configures the guest network, mounts an optional
workspace, and serves the bounded process protocol only to the host vsock CID.
Every workload child clears supplementary groups and irreversibly drops to
UID/GID 10001 with `no_new_privs`. OCI images do not need Python, a shell, `ip`,
`mount`, `setpriv`, or other boot-time utilities.

Registry credentials come from
`EXO_FIRECRACKER_REGISTRY_USERNAME`/`EXO_FIRECRACKER_REGISTRY_PASSWORD`, or the
Docker credential configuration and helper for the user running Exo. For ECR,
this supports `docker-credential-ecr-login` without requiring a Docker daemon.
Because Exo and the Firecracker jailer run as root, set `DOCKER_CONFIG` to the
intended root-owned credential configuration when necessary. Exo rejects a
credential config, credential helper, or parent path writable by non-root
users.

The generated filesystem is 8 GiB by default; set
`EXO_FIRECRACKER_IMAGE_SIZE_GIB` to change it. Structured logs report
`duration_ms` for registry authentication, manifest resolution, cache lookup,
blob pulls, layer extraction, guest-root preparation, ext4 creation, and the
total materialization path. Blob-pull logs also report bytes and cache hit/miss
counts.

The guest kernel must contain `CONFIG_BLK_DEV_INITRD=y`, `CONFIG_EXT4_FS=y`,
`CONFIG_OVERLAY_FS=y`, and `CONFIG_VIRTIO_VSOCKETS=y`; the host requires KVM,
cgroup v2, iproute2, iptables, nftables, e2fsprogs, cpio, static glibc
development files for the guest-runtime build, and `CONFIG_VHOST_VSOCK`.

Install matching official Firecracker and jailer release binaries under
`/usr/local/bin`, and install the guest kernel at
`/var/lib/exo/firecracker/vmlinux`. The binaries and all parent directories
must be root-owned and not writable by group or other users.
Reserve host UID/GID range `100000-132767` for jailed VMMs, or set
`EXO_FIRECRACKER_JAILER_UID_BASE` to the beginning of another reserved range of
32,768 IDs.

Run Exo as root, select the backend, and configure the provider binding:

```bash
sudo EXO_FIRECRACKER_KERNEL=/var/lib/exo/firecracker/vmlinux \
  EXO_FIRECRACKER_INITRAMFS=/var/lib/exo/firecracker/exo-firecracker-initramfs.cpio \
  target/debug/exo --sandbox-backend firecracker provider configure \
  --provider firecracker \
  --default-image 123456789012.dkr.ecr.us-east-1.amazonaws.com/exo-sandbox@sha256:...
```

Agents can then select `--sandbox-provider firecracker`; the Exo CLI and data
model remain the same as for hosted sandbox providers.

## macOS development

Firecracker itself requires Linux KVM. On Apple M3 and newer, Exo uses Lima to
run its Firecracker backend in an ARM64 Linux guest while the CLI stays native
on macOS. Create a dedicated VM whose only writable host mount is the Exo
checkout:

```bash
brew install lima
limactl start \
  --name=exo-firecracker \
  --vm-type=vz \
  --arch=aarch64 \
  --nested-virt \
  --cpus=6 \
  --memory=4 \
  --disk=50 \
  --containerd=none \
  --mount-only="$PWD:w" \
  template:default
limactl shell exo-firecracker
sudo test -r /dev/kvm && sudo test -w /dev/kvm
```

Lima documents nested virtualization as supported with its `vz` driver on M3
and newer:
https://github.com/lima-vm/lima/blob/master/templates/default.yaml

A 4 GiB outer VM is sufficient for development. The macOS backend defaults
inner microVMs to 1 GiB, builds a Linux bridge binary in the shared checkout,
and runs it through passwordless `sudo` in the `exo-firecracker` Lima instance.
The Firecracker binaries, kernel, initramfs, and state directory remain inside
that VM. Run the macOS Exo CLI normally with `--sandbox-backend firecracker`.
Set `EXO_FIRECRACKER_LIMA_INSTANCE` to use a differently named instance.

Enabled networking permits public IPv4 egress, blocks host access, unsolicited
ingress, link-local addresses, and private/special-use ranges, and applies a
per-VM bandwidth limit. Explicit private destinations can be admitted with a
comma-separated `EXO_FIRECRACKER_ALLOWED_EGRESS_CIDRS` value. Disabled
networking creates no TAP interface.

The implementation follows Firecracker's upstream guidance for
[production hosts](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md),
[jailer operation](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md),
[network setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md),
and [virtio-vsock](https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md).
The initramfs follows Firecracker's
[custom initrd guidance](https://github.com/firecracker-microvm/firecracker/blob/main/docs/initrd.md#custom),
and the VMM uses a
[configuration file](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md#configuring-the-microvm-without-sending-api-requests)
for atomic startup while retaining its jailed API socket for point-in-time
forks.
The nested-virtualization macOS setup comes from Firecracker's
[development environment guide](https://github.com/firecracker-microvm/firecracker/blob/main/docs/dev-machine-setup.md#macos-with-vmware-fusion).
