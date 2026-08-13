# Firecracker sandbox artifacts

Exo's Firecracker backend consumes an ext4 root filesystem and an uncompressed
guest kernel. It launches Firecracker only through the matching `jailer`
binary, communicates with the guest agent over virtio-vsock, and adds a TAP
device only when the sandbox requests networking.

Build a root filesystem as root from the same OCI image used by another Exo
backend:

```bash
support/firecracker/build-rootfs.sh \
  --image your-sandbox-image@sha256:... \
  --output /var/lib/exo/firecracker/rootfs.ext4
```

The OCI image must contain `/bin/sh`, Python 3, `setpriv`, `ip`, `mount`,
`mountpoint`, and the standard `cat`, `chown`, `mkdir`, and `rm` utilities. The
guest kernel must contain `CONFIG_VIRTIO_VSOCKETS=y`; the host requires KVM,
cgroup v2, iproute2, nftables, e2fsprogs, and `CONFIG_VHOST_VSOCK`.

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
  target/debug/exo --sandbox-backend firecracker provider configure \
  --provider firecracker \
  --default-image /var/lib/exo/firecracker/rootfs.ext4
```

Agents can then select `--sandbox-provider firecracker`; the Exo CLI and data
model remain the same as for hosted sandbox providers.

## macOS development

Firecracker itself requires Linux KVM. Its upstream development guide supports
macOS by running it in a Linux VM with nested virtualization enabled; on an
Intel Mac, VMware Fusion's **Enable hypervisor applications in this virtual
machine** setting is the documented example.

Run Exo inside that Linux VM:

```bash
sudo EXO_FIRECRACKER_KERNEL=/var/lib/exo/firecracker/vmlinux \
  target/debug/exo --sandbox-backend firecracker serve
```

The server intentionally binds only to loopback, so expose it through an SSH
tunnel rather than an unauthenticated LAN listener:

```bash
ssh -L 4766:127.0.0.1:4766 your-linux-vm
```

Then point the macOS Exo CLI at `http://127.0.0.1:4766` with
`--exoharness-url`, including when configuring the provider or creating an
agent. Firecracker is considered local only on Linux, so macOS clients leave
its sandbox operations on the remote Linux Exo server while keeping the same
Exo CLI and provider model. This uses Exo's existing HTTP mode; it does not
introduce a separate Firecracker wrapper or sandbox daemon.

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
The nested-virtualization macOS setup comes from Firecracker's
[development environment guide](https://github.com/firecracker-microvm/firecracker/blob/main/docs/dev-machine-setup.md#macos-with-vmware-fusion).
