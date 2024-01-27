bcachefs-tools
==============
Userspace tools and docs for bcachefs

Bcachefs is an advanced new filesystem for Linux, with an emphasis on reliability and robustness
and the complete set of features one would expect from a modern filesystem.

This repo primarily consists of the following:

- bcachefs tool, the reason this repo exists.
- {mkfs,mount,fsck}.bcachefs utils, which is just wrappers calling the corresponding subcommands
  in the main tool
- docs in the form of man-pages and a user manual

Please refer to the main site for [getting started](https://bcachefs.org/#Getting_started)
An in-depth user manual is (also) found on the [official website](https://bcachefs.org/#Documentation)

Version semantics
-----------------

The tools relies on an expected disk format structure which is reflected by your current kernel version.
Disk format can be upgraded or downgraded automatically by the kernel, if needed.

- Any patch-level change means no disk format change
- Any minor-level change means a potential disk format change which **is not breaking**
- Any major-level change means **breaking changes**

Build and install
-----------------

Refer to [INSTALL.md](./INSTALL.md)

Bug reports and contributions
-----------------------------

- The official mailing list, linux-bcachefs@vger.kernel.org
- IRC: #bcache on OFTC (irc.oftc.net). Although, note that it can be easily missed.
