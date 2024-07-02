
Compression
~~~~~~~~~~~

bcachefs supports gzip, lz4 and zstd compression. As with data
checksumming, we compress entire extents, not individual disk blocks -
this gives us better compression ratios than other filesystems, at the
cost of reduced small random read performance.

Data can also be compressed or recompressed with a different algorithm
in the background by the rebalance thread, if the
``background_compression`` option is set.