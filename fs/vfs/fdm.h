/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_FS_FDM_H
#define _BCACHEFS_FS_FDM_H

/*
 * Faults-disabled-mapping hash table
 *
 * Replaces the per-task faults_disabled_mapping field (removed in 7.0)
 * with a fixed-size cuckoo hash in bch_fs. Used to prevent deadlock
 * when the page fault handler is invoked for a mapping that is already
 * being written to by the current task (DIO path).
 *
 * Properties:
 *  - O(1) lookup: exactly three slots to check (3-way cuckoo)
 *  - No locks: each task only writes/removes its own entry
 *  - No resize: table is oversized at init, occupancy stays very low
 *  - Low bit of mapping pointer stolen for dropped_locks signaling
 *  - Closure waitlist fallback if all slots full (just a read when empty)
 *
 * Hash function approach borrowed from buckets_waiting_for_journal:
 * random seeds with hash_64() give truly independent distributions.
 * With 3 hash functions and 512 slots at ~16 concurrent tasks,
 * probability of all 3 slots occupied is ~(16/512)^3 ≈ 0.003%.
 */

#include <linux/hash.h>

#include "vendor/closure.h"

#include "vfs/fdm_gen.h"

#endif /* _BCACHEFS_FS_FDM_H */
