Formatting
----------

To format a new bcachefs filesystem use the subcommand
``bcachefs format``, or ``mkfs.bcachefs``. All persistent
filesystem-wide options can be specified at format time. For an example
of a multi device filesystem with compression, encryption, replication
and writeback caching:

   ::

      bcachefs format --compression=lz4               \
                      --encrypted                     \
                      --replicas=2                    \
                      --label=ssd.ssd1 /dev/sda       \
                      --label=ssd.ssd2 /dev/sdb       \
                      --label=hdd.hdd1 /dev/sdc       \
                      --label=hdd.hdd2 /dev/sdd       \
                      --label=hdd.hdd3 /dev/sde       \
                      --label=hdd.hdd4 /dev/sdf       \
                      --foreground_target=ssd         \
                      --promote_target=ssd            \
                      --background_target=hdd
