
Rebalance
~~~~~~~~~

To be implemented: a command for moving data between devices to equalize
usage on each device. Not normally required because the allocator
attempts to equalize usage across devices as it stripes, but can be
necessary in certain scenarios - i.e. when a two-device filesystem with
replication enabled that is very full has a third device added.
