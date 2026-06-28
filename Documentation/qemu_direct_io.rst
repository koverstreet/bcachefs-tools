.. SPDX-License-Identifier: GPL-2.0

QEMU cache=none and direct I/O alignment
========================================

QEMU's ``cache=none`` mode uses ``O_DIRECT`` for image I/O.  On bcachefs,
direct writes must be aligned to the filesystem block size: both the file
offset and I/O length have to be multiples of ``block_size``.  The kernel
advertises this contract through ``statx(STATX_DIOALIGN)``:

* ``stx_dio_mem_align`` is currently 512 bytes.
* ``stx_dio_offset_align`` is the bcachefs block size.

The common bcachefs block size is 4 KiB.  A virtual disk that exposes 512-byte
logical sectors can therefore issue guest writes that are valid for the guest
block device but not valid ``O_DIRECT`` writes to the host image file.  In that
case the host write is rejected and the guest may report I/O errors such as
``Invalid field in cdb`` while formatting or using the virtual disk.

For virtual machine image files, use one of these configurations:

* Configure the virtual disk logical and physical block size to match the
  bcachefs block size when using ``cache=none``.
* Use a QEMU cache mode that does not require unpadded 512-byte ``O_DIRECT``
  writes to the image file.
* If 512-byte direct I/O is required, format the bcachefs filesystem with a
  512-byte block size.

These errors are rejected I/O, not silent corruption.  Userspace that uses
``O_DIRECT`` should check ``STATX_DIOALIGN`` and avoid issuing writes that do
not satisfy the advertised alignment.
