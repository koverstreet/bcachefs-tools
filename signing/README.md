# bcachefs module-signing keys

This directory holds the **public** half of the key hierarchy used to sign
prebuilt bcachefs kernel modules so they load under Secure Boot.

## Files

- `bcachefs-signing-ca.der` — the root CA certificate, DER-encoded. This is
  what gets enrolled into a machine's MOK (Machine Owner Key) store. DER is
  the format `mokutil --import` consumes.
- `bcachefs-signing-ca.pem` — the same certificate, PEM-encoded, for
  inspection (`openssl x509 -in bcachefs-signing-ca.pem -noout -text`).

Both are public certificates. **No private key lives in this repo** — the
root CA private key is kept offline, and the leaf signing key lives only on
the build server (delivered via agenix).

## Key hierarchy

```
root CA  (offline, on the maintainer's laptop, passphrase-protected)
  │   CA:TRUE, keyCertSign, NO digitalSignature
  │   → enrolled once per machine into .machine via MOK
  └── leaf  (on the build server, via agenix)
          CA:FALSE, digitalSignature
          → signs each module; the leaf cert is embedded in every signature
```

The leaf certificate is embedded in each module signature (the build server's
`sign-file` is patched to drop `CMS_NOCERTS`), so the kernel can build the
chain leaf → root CA at verification time. Only the **root CA** needs to be
enrolled; leaf keys can be rotated without re-enrollment.

## Why the root CA has no digitalSignature usage

The kernel's `.machine` keyring has three policies, increasing in strictness
(`security/integrity/Kconfig`, `crypto/asymmetric_keys/restrict.c`):

| Policy                            | Accepts                                  |
|-----------------------------------|------------------------------------------|
| `INTEGRITY_MACHINE_KEYRING`       | anything (no restriction)                |
| `+ CA_MACHINE_KEYRING`            | CA bit + keyCertSign                      |
| `+ CA_MACHINE_KEYRING_MAX`        | CA + keyCertSign + **not** digitalSignature |

Fedora ships the strictest (`_MAX`) variant; Debian/Ubuntu/Arch ship the
permissive plain keyring. A root CA with `keyCertSign` and **no**
`digitalSignature` satisfies all three, so a single enrolled cert works on
every distro.

## Secure Boot enrollment

On Debian, installing `bcachefs-tools` on a Secure-Boot-enabled machine
offers (via debconf) to schedule enrollment of this CA. Enrollment is
completed by the user at the next reboot through the firmware's MOK Manager.
See `debian/bcachefs-tools.{config,templates,postinst}`.

Manual enrollment on any distro:

```
sudo mokutil --import /usr/share/bcachefs-tools/bcachefs-signing-ca.der
# reboot, then confirm in the MOK Manager
```
