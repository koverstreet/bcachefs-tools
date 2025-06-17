
Encryption
~~~~~~~~~~

bcachefs supports authenticated (AEAD style) encryption -
ChaCha20/Poly1305. When encryption is enabled, the poly1305 MAC replaces
the normal data and metadata checksums. This style of encryption is
superior to typical block layer or filesystem level encryption (usually
AES-XTS), which only operates on blocks and doesn’t have a way to store
nonces or MACs. In contrast, we store a nonce and cryptographic MAC
alongside data pointers - meaning we have a chain of trust up to the
superblock (or journal, in the case of unclean shutdowns) and can
definitely tell if metadata has been modified, dropped, or replaced with
an earlier version - replay attacks are not possible.

Encryption can only be specified for the entire filesystem, not per file
or directory - this is because metadata blocks do not belong to a
particular file. All metadata except for the superblock is encrypted.

In the future we’ll probably add AES-GCM for platforms that have
hardware acceleration for AES, but in the meantime software
implementations of ChaCha20 are also quite fast on most platforms.

``scrypt`` is used for the key derivation function - for converting the
user supplied passphrase to an encryption key.

To format a filesystem with encryption, use

   ::

      bcachefs format --encrypted /dev/sda1

You will be prompted for a passphrase. Then, to use an encrypted
filesystem use the command

   ::

      bcachefs unlock /dev/sda1

You will be prompted for the passphrase and the encryption key will be
added to your in-kernel keyring; mount, fsck and other commands will
then work as usual.

The passphrase on an existing encrypted filesystem can be changed with
the ``bcachefs set-passphrase`` command. To permanently unlock an
encrypted filesystem, use the ``bcachefs remove-passphrase`` command -
this can be useful when dumping filesystem metadata for debugging by the
developers.

There is a ``wide_macs`` option which controls the size of the
cryptographic MACs stored on disk. By default, only 80 bits are stored,
which should be sufficient security for most applications. With the
``wide_macs`` option enabled we store the full 128 bit MAC, at the cost
of making extents 8 bytes bigger.