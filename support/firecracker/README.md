# Firecracker sandbox artifacts

Exo's optional Firecracker backend accepts either an OCI image reference or an
operator-approved ext4 root filesystem, plus an uncompressed guest kernel and
a minimal initramfs. It launches Firecracker only through the matching `jailer` binary,
communicates with a static Rust PID 1 over virtio-vsock, and adds a TAP device
only when the sandbox requests networking.

## Compatible host artifact bundle

Install Firecracker, jailer, the guest kernel, and the Exo initramfs as one
immutable, checksummed bundle. A deployment image can copy all four into a
fixed directory; a standalone installation can distribute the same directory
as a release archive. No registry service is required beyond wherever that
immutable image or archive is already published.

Build the initramfs from the exact Exo source revision used by the host's
`exoharness` dependency. Pin the Firecracker release and kernel build for each
architecture, verify downloaded files before installing them, and record the
resulting initramfs checksum after building it. For example, this is a concrete
bundle known to work:

| Asset                                                  | Version or build                         | x86_64 SHA-256                                                     | aarch64 SHA-256                                                    |
| ------------------------------------------------------ | ---------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| Firecracker release archive (includes matching jailer) | 1.16.1                                   | `382a02a869e4d6d5cb14c40577f9545e8458021ea8b0b2d3fc10ec14d9c242e6` | `8d0e69f6d6f9a1724551f607f18504052c16c1828ee3d4d7b6e6c73380871e0e` |
| Firecracker CI kernel                                  | 6.18.39, build `20260812-48f1b9fb52e9-0` | `3fde810177aaf9d9a465d00e915924a7a6e2bdd34869d7fcd33c6af73b45e12d` | `92b6c36b3ea25328f32f7b09509989a0f653aa692afa745fde5274c349b6405e` |
| Exo guest initramfs                                    | same Exo revision as the host            | record after building                                              | record after building                                              |

The host verifies that Firecracker and jailer report the same version, and the
versioned guest handshake rejects an incompatible host/guest protocol. It also
fingerprints the exact binaries, kernel, initramfs, architecture, and VM shape;
running machines are not adopted and snapshots are not restored under a
different fingerprint. These checks prevent accidental mixing but do not
replace checksum verification when the bundle is built or downloaded.

Build the CLI with the backend and its OCI/filesystem dependencies enabled:

```bash
cargo build -p exo --features firecracker
```

For an OCI reference, Exo resolves the platform manifest, pulls its layers
directly from the registry, applies OCI whiteouts, creates the guest-owned
workspace directories, and creates ext4 with `mkfs.ext4 -d`. This path does not
use Docker or containerd, and the host never mounts or executes image content.
Every manifest, config, and layer blob is verified against its sha256 digest
before use; downloads and layer decompression are held to budgets derived from
the configured image size, so a malicious image cannot exhaust host disk.
setuid/setgid bits and xattrs (including `security.capability`) are stripped
during extraction. Images are cached by immutable manifest-list digest and
platform under `EXO_FIRECRACKER_STATE_ROOT/images/v4`. After the first
materialization, a digest-pinned OCI reference reaches this cache without a
registry request. Tag references resolve through the registry over HTTPS and
then cache by the returned digest; the first resolution of a tag trusts
whatever the registry returns, so pin digests where that matters.

### Compatible OCI images

Most ordinary Linux container images work, subject to these constraints:

- The image must include a Linux manifest for the Firecracker host
  architecture (`linux/arm64` in an Apple Silicon Lima VM, for example). Exo
  selects the matching platform from a multi-architecture image but does not
  emulate another architecture.
- Exo uses the image's filesystem, not its OCI runtime configuration.
  `ENTRYPOINT`, `CMD`, `USER`, environment, and working-directory metadata are
  not applied; commands and their environment are supplied through the sandbox
  API instead.
- Workload processes run as UID/GID 10001 with `no_new_privs`. Images whose
  programs or data are only accessible to root, or which require privileged
  container operations, must be adapted. Exo also strips setuid/setgid bits,
  xattrs, and file capabilities while materializing the image.
- `sandbox connect` requires the selected shell to exist in the image. It uses
  `/bin/bash` by default; pass `--shell /bin/sh` for smaller images. Distroless
  and `scratch` images can still be used with `sandbox exec` when you name an
  executable they contain.
- The extracted filesystem must fit within `--firecracker-image-size-gib`
  (8 GiB by default).

The image does not need an init system, kernel, Python, or networking tools;
the Exo initramfs supplies the guest agent and performs VM boot and setup.

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
workspace, reaps orphaned descendants like an init should, and serves the
bounded process protocol only to the host vsock CID. Every workload child
clears supplementary groups and irreversibly drops to UID/GID 10001 with
`no_new_privs`, and all image content is mounted `nosuid,nodev`. OCI images do
not need Python, a shell, `ip`, `mount`, `setpriv`, or other boot-time
utilities.

Registry credentials come from the Docker credential configuration and helper
for the user running Exo. Credentials are selected for the exact registry
origin; Exo intentionally does not accept a global username/password pair that
could be sent to a caller-selected registry. For ECR, this supports
`docker-credential-ecr-login` without requiring a Docker daemon. Because Exo
and the Firecracker jailer run as root, set `DOCKER_CONFIG` to the intended
root-owned credential configuration when necessary. Exo rejects a credential
config, credential helper, or parent path writable by non-root users, and
executes a configured helper only at its validated absolute path.

The generated filesystem is 8 GiB by default; set
`EXO_FIRECRACKER_IMAGE_SIZE_GIB` to change it. That size also caps how much
host disk one image materialization may consume. Structured logs report
`duration_ms` for registry authentication, manifest resolution, cache lookup,
blob pulls, layer extraction, guest-root preparation, ext4 creation, and the
total materialization path. Blob-pull logs also report bytes and cache hit/miss
counts.

The guest kernel must contain `CONFIG_BLK_DEV_INITRD=y`, `CONFIG_EXT4_FS=y`,
`CONFIG_OVERLAY_FS=y`, and `CONFIG_VIRTIO_VSOCKETS=y`; the host requires KVM,
cgroup v2, iproute2, iptables, nftables, e2fsprogs, cpio, static glibc
development files for the guest-runtime build, and `CONFIG_VHOST_VSOCK`.
Networking-enabled sandboxes additionally require `net.ipv4.ip_forward=1`,
which Exo checks but deliberately does not set — enable it persistently (eg.
via `/etc/sysctl.d`) so it survives host reboots. If you
use forks, the guest kernel must additionally be 5.18 or newer with
`CONFIG_VMGENID=y` so clones reseed their CSPRNG on restore, and
`CONFIG_HW_RANDOM_VIRTIO=y` lets the guest draw extra entropy from the
attached virtio-rng device.

Install matching official Firecracker and jailer release binaries under
`/usr/local/bin`, and install the guest kernel at
`/var/lib/exo/firecracker/vmlinux`. The binaries and all parent directories
must be root-owned and not writable by group or other users. Use the official
release (musl, statically linked) builds: debug and gnu builds ship without
Firecracker's seccomp filters, and Exo verifies the version match but not the
build flavor. Reserve host UID/GID range `100000-132767` for jailed VMMs, or
set `EXO_FIRECRACKER_JAILER_UID_BASE` to the beginning of another reserved
range of 32,768 IDs.

Run Exo as root, select the backend, and configure the provider binding:

```bash
sudo EXO_FIRECRACKER_KERNEL=/var/lib/exo/firecracker/vmlinux \
  EXO_FIRECRACKER_INITRAMFS=/var/lib/exo/firecracker/exo-firecracker-initramfs.cpio \
  target/debug/exo provider configure \
  --provider firecracker \
  --default-image 123456789012.dkr.ecr.us-east-1.amazonaws.com/exo-sandbox@sha256:...
```

Agents can then select `--provider firecracker`; the Exo CLI and data
model remain the same as for hosted sandbox providers.

To start an unnamed sandbox and connect a shell in one command, use `sandbox
play`. It destroys the sandbox when the shell exits:

```bash
sudo target/debug/exo sandbox play \
  --provider firecracker \
  --image 123456789012.dkr.ecr.us-east-1.amazonaws.com/exo-sandbox@sha256:... \
  --networking enabled \
  --firecracker-memory-mib 2048 \
  --firecracker-vcpu-count 2
```

`sandbox start --help` and `sandbox play --help` list every sandbox-creation
parameter, including the working directory, idle timeout, host mounts, durable
filesystems, and networking. Their `--firecracker-*` options control the VMM
and jailer paths, kernel, initramfs, state root, VM sizing, DNS, UID range,
image/workspace sizes, and network rate limit. The same options live under
`exo serve` for a persistent backend. The corresponding `EXO_FIRECRACKER_*`
environment variables remain supported.

For a sandbox that survives between CLI invocations, use the separate lifecycle
commands. Each command adopts the running Firecracker VM from its persisted
state:

```bash
sandbox_id=$(sudo target/debug/exo sandbox start --provider firecracker)
sudo target/debug/exo sandbox ps
sudo target/debug/exo sandbox exec "$sandbox_id" -- /bin/echo hello
sudo target/debug/exo sandbox connect "$sandbox_id"
sudo target/debug/exo sandbox terminate "$sandbox_id"
```

`connect` streams an interactive shell over stdin/stdout/stderr; the current
sandbox process API does not provide a PTY. A long-running `exo serve` process
is still useful when multiple clients need concurrent access to one backend.
Every lifecycle command accepts `--agent <slug>` to use that agent's sandbox
scope. Without it, commands share an internal singleton owner intended for
direct CLI use.

`sandbox ps` shows running sandboxes by default; `-a` includes stopped records,
and `-q` prints only IDs.
`stop` and `terminate` accept multiple IDs, so retained sandboxes can be cleaned
up compositionally:

```bash
sudo target/debug/exo sandbox ps -aq \
  | sudo target/debug/exo sandbox terminate
```

## Security model

Isolation is layered rather than resting on one wall. The workload runs as an
unprivileged user under `no_new_privs` inside the guest; the guest is contained
by KVM's hardware boundary; the VMM runs under Firecracker's default seccomp
filters inside a jailer chroot with a unique unprivileged UID per VM, its own
PID namespace, network namespace, and cgroup v2 CPU/memory limits; and the
host-side control plane never parses or mounts guest-controlled data — the
only host↔guest channel is a bounded-frame JSON protocol over vsock, and the
guest agent accepts connections only from the host CID. VMM console output is
captured through a pipe and bounded on disk, because upstream documents that a
compromised guest kernel can reactivate the serial device.

Enabled networking permits public IPv4 egress and nothing else: source
addresses are anti-spoofed, guest-to-host and guest-to-guest traffic is
blocked (guest-to-guest cannot be opened even by the allow-list), link-local
ranges including the cloud metadata service and private/special-use ranges are
rejected unless explicitly admitted via a comma-separated
`--firecracker-allowed-egress-cidr <CIDR>` (repeatable), IPv6 is dropped
outright, and each VM gets a bandwidth cap. Disabled networking creates no TAP
interface at all — the control channel is vsock, so exec still works.

Forking pauses the source VM, snapshots memory and disk, and boots clones with
their own copy-on-write disk, network namespace, and IP; snapshot templates
are copied out of the source VM's jail (never shared by hard link) and
published root-owned and immutable. Every fork snapshots the source at the
time of that call. The source resumes alongside its clones, which briefly
share identical userspace state (PRNG pools, session tokens, boot_id), and only
a VMGenID-capable guest kernel (5.18+) reseeds the kernel CSPRNG on restore.
Forks are an explicit API call, never guest-triggerable.

## Limitations and operator responsibilities

Be aware of what this backend does _not_ do, and what stays on the operator:

- **Host side-channel hardening is yours.** Exo does not disable SMT, KSM, or
  swap, and does not verify microcode or host kernel currency. For workloads
  from mutually distrusting tenants, follow the corresponding sections of
  Firecracker's [production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md).
- **Sandbox ports are reachable host-wide.** A host route makes each guest's
  IP reachable from the whole host, so any local user or process — not just
  Exo — can connect to services listening inside a networking-enabled sandbox.
  Treat in-sandbox services as unauthenticated local services.
- **No storage rate limiting.** Drives carry no I/O rate limiters and no blkio
  cgroup, so one guest can contend host disk bandwidth with its neighbors.
  Network bandwidth is capped per VM, but packet rates are not.
- **Fork snapshot hygiene is partial.** VMGenID reseeds the guest kernel
  CSPRNG and a rate-limited virtio-rng device is attached, but userspace
  processes already running at snapshot time may hold pre-snapshot randomness
  and identical secrets in source and clones. Don't fork workloads holding
  secrets that must diverge.
- **The in-guest boundary is the VM, not the chroot.** Guest workloads share
  the guest kernel with the agent, `/proc` is not mounted with `hidepid`, and
  the agent's chroot is a convenience, not a containment boundary. Isolation
  between sandboxes comes from KVM, not from anything inside the guest.
- **Command cancellation is process-group based.** Timeout and explicit
  cancellation kill the command's original process group. A deliberately
  evasive descendant can create a new session or process group and remain in
  a persistent microVM. Stop or terminate the sandbox when every descendant
  must be removed; full descendant cancellation requires per-command cgroups.
- **Tag references are trust-on-first-use.** Only digest-pinned references
  anchor the full download chain to something you chose.
- **Manifest and config responses are buffered in memory without a size
  cap** (a limitation of the underlying OCI client library). Two consequences:
  a hostile registry _server_ can exhaust the host process's memory, and even
  on an honest registry, a _published image_ can declare an oversized config
  blob that gets buffered whole — real registries cap manifest sizes at push
  but not config blobs. The failure mode is a crash of the Exo process, not a
  guest escape or data exposure. Layer blobs are not affected — they stream
  to disk under strict size budgets. Agents inside a turn cannot create
  sandboxes or choose images, but any full-scope API client can name an image
  reference; pass `--firecracker-allowed-registry <HOST>` (repeatable or
  comma-separated, eg.
  `--firecracker-allowed-registry docker.io,123456789012.dkr.ecr.us-east-1.amazonaws.com`)
  to restrict registry entry points, and only materialize images from
  publishers you trust. Permitted registries may redirect blob downloads to
  other object-storage hosts and OCI descriptors can name alternate blob
  URLs, so selecting an unrestricted registry grants that publisher outbound
  network access from the privileged materializer. This is a publisher
  allowlist rather than a host-network allowlist; do not give untrusted clients
  full-scope image selection. The durable fix is a bounded response read and
  destination policy in the OCI client library.
- **The image cache grows without bound.** Every unique image digest and layer
  blob a full-scope client requests is retained under
  `EXO_FIRECRACKER_STATE_ROOT/images` — the standard behavior of a
  digest-addressed store (Docker and containerd do the same), but there is no
  quota or eviction yet. Each individual materialization is budget-capped;
  the aggregate is bounded only by the volume. Size the state-root volume for
  your image set, and prune `images/` offline when needed — it is a cache, so
  deleting entries while no materialization is running is always safe (VMs in
  flight keep their content alive via hard links). Local `.ext4` images are
  operator-supplied trusted input and are not size-checked. The standard
  `/var/lib/exo/firecracker/rootfs.ext4` image is permitted by default; grant
  any other exact path with a repeatable
  `--firecracker-allowed-local-image <PATH>` flag.

The implementation follows Firecracker's upstream guidance for
[jailer operation](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md),
[seccomp](https://github.com/firecracker-microvm/firecracker/blob/main/docs/seccomp.md)
(default filters, never overridden),
[network setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md),
and [virtio-vsock](https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md).
Of the [production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)
guidance it implements the jailer, seccomp, serial-device, egress-filtering,
and network rate-limit items; the host-level items listed above remain
operator responsibility. The initramfs follows Firecracker's
[custom initrd guidance](https://github.com/firecracker-microvm/firecracker/blob/main/docs/initrd.md#custom),
and the VMM uses a
[configuration file](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md#configuring-the-microvm-without-sending-api-requests)
for atomic startup while retaining its jailed API socket for point-in-time
forks.

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
installs it to a root-owned path inside the VM, and runs it through
passwordless `sudo` in the `exo-firecracker` Lima instance. Exo refuses to
auto-create the instance: it must already exist, so a typo'd
`EXO_FIRECRACKER_LIMA_INSTANCE` cannot silently provision a default VM that
mounts your home directory. The Firecracker binaries, kernel, initramfs, and
state directory remain inside that VM. Run the macOS Exo CLI normally with
`--provider firecracker`.

Use `make restart-firecracker-lima` to restart the outer VM without removing
Exo state. Use `make clean-firecracker-microvms` to restart it and remove all
per-microVM state while retaining downloaded OCI images and the cached kernel
and initramfs. Both commands honor `EXO_FIRECRACKER_LIMA_INSTANCE` and
`EXO_FIRECRACKER_STATE_ROOT`; the clean command refuses state roots outside
`/var/lib/exo/firecracker`.

This path is for development only. Isolation _between_ microVMs inside the
Lima VM is the same as on Linux, but the Lima VM itself belongs to your macOS
user: its default account holds passwordless sudo, the Exo checkout is mounted
writable, and the bridge protocol is unauthenticated stdio between your own
processes. None of the Linux host's control-plane trust properties hold here —
do not present it as a production configuration.

The nested-virtualization macOS setup comes from Firecracker's
[development environment guide](https://github.com/firecracker-microvm/firecracker/blob/main/docs/dev-machine-setup.md#macos-with-vmware-fusion).
