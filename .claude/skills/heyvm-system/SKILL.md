---
name: heyvm-system
description: Check if the host system meets all requirements to run VM sandboxes. On Linux, verifies KVM, Firecracker, kernel images, networking permissions. On macOS, verifies Apple Container and Apple Virtualization native (apple_virt) readiness and VM images. Runs a full end-to-end test on each platform. Use when the user wants to diagnose setup issues or verify their host is ready.
argument-hint: "[--skip-test]"
allowed-tools: Bash, Read, Grep
---

# heyvm-system — Host Readiness Check

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills.

Run platform-specific checks to verify the host can run VM sandboxes, then optionally run a full end-to-end test.

First, detect the platform:
```bash
uname -s
```

Then run the appropriate section below.

---

## macOS — Apple Container / Apple Virtualization Native

Run each check in order. Print a clear PASS/FAIL for each.

### 1. macOS version

```bash
sw_vers --productVersion
```

Apple Container and apple_virt backends require macOS 13+.

### 2. Apple Silicon check

```bash
uname -m
```

Apple Container and apple_virt work best on Apple Silicon (arm64). Intel Macs have limited support.

### 3. Virtualization entitlement

Check if the heyvm binary is signed with the required entitlement:
```bash
codesign -d --entitlements - $(which heyvm) 2>&1 | grep -c "com.apple.security.virtualization" && echo "PASS: virtualization entitlement present" || echo "FAIL: missing com.apple.security.virtualization entitlement — run 'make install' from mvm-ctrl/"
```

### 4. VM image

Check if the default apple_virt image exists:
```bash
test -f ~/.heyo/images/apple_virt/default/vmlinuz && test -f ~/.heyo/images/apple_virt/default/rootfs.img && echo "PASS: default image present" || echo "INFO: default image not found — it will be auto-downloaded on first use, or build manually with install/build-apple-virt-image.sh"
```

### 5. Docker (for image building)

```bash
docker --version 2>/dev/null && echo "PASS" || echo "WARN: Docker not found — needed to build custom VM images with install/build-apple-virt-image.sh"
```

### 6. End-to-end test (unless --skip-test)

```bash
heyvm test-apple-virt
```

This creates a VM, verifies exec works, and cleans up. Images are auto-downloaded if missing.

### Interpreting results (macOS)

- **All checks pass + test succeeds**: Host is fully ready for apple_virt sandboxes.
- **Checks pass but test fails**: Check `~/.heyo/heyvm.log` for detailed errors. Try rebuilding the image with `install/build-apple-virt-image.sh`.
- **Entitlement fails**: The binary must be code-signed. Run `make install` from the `mvm-ctrl/` directory.
- **VM start fails with VZErrorDomain**: Ensure macOS 13+ and Apple Silicon. Check that the binary has the `com.apple.security.virtualization` entitlement.
- **apple_container** backend: Requires Apple's `container` CLI installed separately. Not checked by `test-apple-virt`.

---

## Linux — Firecracker

Run each check in order. Print a clear PASS/FAIL for each. Stop on the first hard failure and explain what to fix.

### 1. KVM support

```bash
test -r /dev/kvm && test -w /dev/kvm && echo "PASS: /dev/kvm accessible" || echo "FAIL: /dev/kvm not accessible (add user to kvm group: sudo usermod -aG kvm $USER)"
```

### 2. Firecracker binary

```bash
firecracker --version 2>/dev/null && echo "PASS" || echo "FAIL: firecracker not found on PATH"
```

### 3. Kernel image

Check if a kernel exists at the default location or in `~/.heyo/images/firecracker/vmlinux.bin`. If missing, suggest:
```bash
heyvm mvm fetch-kernel
```

### 4. Firecracker guest images

```bash
heyvm mvm images
```

If no images are listed, inform the user they need to build one:
```bash
heyvm mvm build --local-only -f third-party/Dockerfile.firecracker-nginx -n nginx-fc
```

**IMPORTANT:** If an `nginx-fc` image already exists, the user must rebuild it after any changes to `Dockerfile.firecracker-nginx`:
```bash
heyvm mvm build --local-only -f third-party/Dockerfile.firecracker-nginx -n nginx-fc
```

### 5. Sudo / networking permissions

```bash
sudo -n ip link show >/dev/null 2>&1 && echo "PASS: passwordless sudo for ip" || echo "WARN: sudo requires password — run 'sudo -v' before tests or configure passwordless sudo (see docs/content/docs/host-requirements.md)"
```

### 6. e2fsprogs (mke2fs / debugfs)

```bash
which mke2fs >/dev/null 2>&1 && echo "PASS" || echo "FAIL: mke2fs not found (install e2fsprogs)"
```

### 7. End-to-end test (unless --skip-test)

Run the built-in Firecracker integration test:
```bash
heyvm test-firecracker
```

If this is the first run after changes to `Dockerfile.firecracker-nginx`, remind the user to rebuild the image first.

### Interpreting results (Linux)

- **All checks pass + test succeeds**: Host is fully ready for Firecracker sandboxes.
- **Checks pass but test fails**: Likely a guest image issue. Rebuild with `heyvm mvm build`.
- **KVM or Firecracker fails**: Hardware/software prerequisites missing — refer user to `docs/content/docs/host-requirements.md`.
- **Sudo fails**: Networking commands need privileges — either cache credentials with `sudo -v` or set up passwordless sudo for `ip`, `iptables`, `sysctl`.
