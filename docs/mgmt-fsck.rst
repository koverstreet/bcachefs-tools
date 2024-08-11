
Checking Filesystem Integrity
-----------------------------

It is possible to run fsck either in userspace with the
``bcachefs fsck`` subcommand (also available as ``fsck.bcachefs``, or in
the kernel while mounting by specifying the ``-o fsck`` mount option. In
either case the exact same fsck implementation is being run, only the
environment is different. Running fsck in the kernel at mount time has
the advantage of somewhat better performance, while running in userspace
has the ability to be stopped with ctrl-c and can prompt the user for
fixing errors. To fix errors while running fsck in the kernel, use the
``-o fix_errors`` option.

The ``-n`` option passed to fsck implies the ``-o nochanges`` option;
``bcachefs fsck -ny`` can be used to test filesystem repair in dry-run
mode.
